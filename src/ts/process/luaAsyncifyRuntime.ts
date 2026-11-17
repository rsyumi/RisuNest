import { LUA_REGISTRYINDEX, LuaReturn, LuaType, type LuaEngine, type LuaThread } from 'wasmoon'
import { getLuaExecutionBudget } from './luaExecutionBudget'
import { decodeLuaFailure, encodeLuaFailure, type LuaWorkerFailure } from './luaWorkerProtocol'

type Reply = { value?: unknown; error?: string; failure?: LuaWorkerFailure; promise?: number; elapsed?: number }
type AsyncifyExports = {
    asyncify_get_state(): number
    asyncify_start_unwind(data: number): void
    asyncify_stop_unwind(): void
    asyncify_start_rewind(data: number): void
    asyncify_stop_rewind(): void
}
const formatInLua = Symbol('formatInLua')

export class LuaAsyncifyRuntime {
    private readonly lua
    private readonly module
    private readonly exports: AsyncifyExports
    private readonly budget
    private readonly stack: number
    private readonly data: number
    private readonly entrypoint: number
    private readonly sourceLoader: number
    private readonly callbackEntry: number
    private readonly hostWrapper: number
    private readonly gcControl: number
    private readonly controlCall
    private queue: Promise<unknown> = Promise.resolve()
    private pending?: Promise<Reply>
    private reply?: Reply
    private sourceDeadline?: number
    private closed = false
    private tainted = false
    private invocation = 0
    private currentInvocation = 0
    private active = false

    constructor(
        private readonly engine: LuaEngine,
        private readonly timeout: number,
        private readonly request: (name: string, args: unknown[]) => Promise<Reply>,
        private readonly hostPromise: (id: number) => Promise<unknown>,
        private readonly execution = { now: () => performance.now(), run: <T>(run: () => T): T => run() },
    ) {
        this.lua = engine.global.lua
        this.module = this.lua.module as typeof this.lua.module & {
            risuAsyncifyExports: AsyncifyExports
            risuRun?: (thread: LuaThread, count: number) => Promise<unknown[]>
            risuCall?: (parent: LuaThread, ref: number, args: unknown[], invocation: number) => Promise<unknown>
            risuInvocation?: () => number
            risuRelease?: (release: () => void) => void
        }
        this.exports = this.module.risuAsyncifyExports
        this.controlCall = this.lua.lua_pcallk.bind(this.lua)
        this.budget = getLuaExecutionBudget(this.lua)
        this.budget.attach(engine, timeout, this.execution.now, true)
        engine.global.loadString(`local gc = collectgarbage
            local running = gc('isrunning')
            return function(resume)
                if resume then
                    if running then gc('restart') end
                else
                    running = gc('isrunning')
                    gc('stop')
                end
            end`)
        engine.global.assertOk(this.controlCall(engine.global.address, 0, 1, 0, 0, null))
        this.gcControl = this.lua.luaL_ref(engine.global.address, LUA_REGISTRYINDEX)
        engine.global.loadString(`local globals, kind, fail, protected, describe = _ENV, type, error, xpcall, tostring
            return function(name, ...)
                local fn = globals[name]
                if fn == nil or fn == false then return false end
                if kind(fn) ~= 'function' then fail('Lua handler is not a function') end
                local ok, value = protected(fn, describe, ...)
                if not ok then fail(value, 0) end
                return true, value
            end`)
        engine.global.assertOk(this.lua.lua_pcallk(engine.global.address, 0, 1, 0, 0, null))
        this.entrypoint = this.lua.luaL_ref(engine.global.address, LUA_REGISTRYINDEX)
        engine.global.loadString(`local compile, protected, describe, fail = load, xpcall, tostring, error
            return function(source)
                local chunk, problem = compile(source)
                if not chunk then fail(problem, 0) end
                local ok, value = protected(chunk, describe)
                if not ok then fail(value, 0) end
            end`)
        engine.global.assertOk(this.lua.lua_pcallk(engine.global.address, 0, 1, 0, 0, null))
        this.sourceLoader = this.lua.luaL_ref(engine.global.address, LUA_REGISTRYINDEX)
        engine.global.loadString(`local protected, describe, fail = xpcall, tostring, error
            return function(fn, ...)
                local ok, value = protected(fn, describe, ...)
                if not ok then fail(value, 0) end
                return value
            end`)
        engine.global.assertOk(this.lua.lua_pcallk(engine.global.address, 0, 1, 0, 0, null))
        this.callbackEntry = this.lua.luaL_ref(engine.global.address, LUA_REGISTRYINDEX)
        // Mirrors Wasmoon's table conversion order so key __tostring calls run as resumable Lua.
        engine.global.loadString(`local kind, walk, describe, count, pick = type, next, tostring, select, select
            local function collect(strings, seen, value)
                if kind(value) ~= 'table' or seen[value] then return end
                seen[value] = true
                for key in walk, value do strings[#strings + 1] = describe(key) end
                for key, item in walk, value do
                    strings[#strings + 1] = describe(key)
                    collect(strings, seen, item)
                end
            end
            local function wrap(raw)
                local function finish(...)
                    if (...) ~= wrap then return ... end
                    local strings = {}
                    for i = 2, count('#', ...) do collect(strings, {}, (pick(i, ...))) end
                    return raw(strings, pick(2, ...))
                end
                return function(...) return finish(raw(nil, ...)) end
            end
            return wrap`)
        engine.global.assertOk(this.lua.lua_pcallk(engine.global.address, 0, 1, 0, 0, null))
        this.hostWrapper = this.lua.luaL_ref(engine.global.address, LUA_REGISTRYINDEX)
        this.stack = this.module._malloc(1024 * 1024)
        this.data = this.module._malloc(8)
        this.module.setValue(this.data, this.stack, 'i32')
        this.module.setValue(this.data + 4, this.stack + 1024 * 1024, 'i32')
        this.module.risuRun = (thread, count) => this.run(thread, count)
        this.module.risuInvocation = () => this.currentInvocation
        this.module.risuCall = (parent, ref, args, invocation) => this.callReference(parent, ref, args, invocation)
        this.module.risuRelease = release => { void this.exclusive(release).catch(() => {}) }
        this.controlGC(false)
    }

    private controlGC(resume: boolean): void {
        // A finalizer can call a host API. Run GC only inside the resumable Lua
        // entry, never in a JS binding's intermediate pushValue/newThread call.
        const address = this.engine.global.address
        this.lua.lua_rawgeti(address, LUA_REGISTRYINDEX, BigInt(this.gcControl))
        this.lua.lua_pushboolean(address, resume ? 1 : 0)
        // A deadline that passes here must not unwind through a suspended host callback.
        this.budget.runUninterrupted(() => this.engine.global.assertOk(this.controlCall(address, 1, 0, 0, 0, null)))
    }

    private pushReply(thread: LuaThread, value: unknown): void {
        this.controlGC(false)
        try { thread.pushValue(value) }
        finally { this.controlGC(true) }
    }

    private exclusive<T>(work: () => T | Promise<T>): Promise<T> {
        const result = this.queue.then(() => {
            if (this.closed) throw new Error('Lua executor closed')
            return work()
        })
        this.queue = result.catch(() => {})
        return result
    }

    register(name: string): void {
        const callback = this.module.addFunction((address: number) => {
            const thread = this.engine.global.stateToThread(address)
            if (this.exports.asyncify_get_state() === 2) {
                this.exports.asyncify_stop_rewind()
                const reply = this.reply!
                this.reply = undefined
                this.budget.consumeHost(this.engine, reply.elapsed ?? 0)
                if (this.budget.expired(this.engine)) {
                    this.lua.lua_pushstring(address, 'thread timeout exceeded')
                    return this.lua.lua_error(address)
                }
                if (reply.failure !== undefined || reply.error !== undefined) {
                    this.pushReply(thread, reply.failure === undefined ? new Error(reply.error) : decodeLuaFailure(reply.failure))
                    return this.lua.lua_error(address)
                }
                const value = reply.promise === undefined ? reply.value : this.hostPromise(reply.promise)
                if (value === undefined) return 0
                this.pushReply(thread, value)
                return 1
            }
            if (!this.active || this.currentInvocation !== this.invocation) {
                this.pushReply(thread, new Error('Lua invocation ended'))
                return this.lua.lua_error(address)
            }
            // Suspend the WASM stack, including Lua C-library callbacks, without yielding Lua.
            this.controlGC(false)
            let args: unknown[] | undefined
            try { args = this.convertArguments(thread) }
            finally { this.controlGC(true) }
            if (!args) {
                // The wrapper formats the table keys in Lua and calls again with the strings.
                this.lua.lua_rawgeti(address, LUA_REGISTRYINDEX, BigInt(this.hostWrapper))
                this.lua.lua_copy(address, -1, 1)
                this.lua.lua_settop(address, -2)
                return this.lua.lua_gettop(address)
            }
            this.pending = this.request(name, args).catch(error => ({ failure: encodeLuaFailure(error) }))
            this.exports.asyncify_start_unwind(this.data)
            return 0
        }, 'ii')
        const address = this.engine.global.address
        this.lua.lua_rawgeti(address, LUA_REGISTRYINDEX, BigInt(this.hostWrapper))
        this.lua.lua_pushcclosure(address, callback, 0)
        this.engine.global.assertOk(this.controlCall(address, 1, 1, 0, 0, null))
        this.lua.lua_setglobal(address, name)
    }

    private convertArguments(thread: LuaThread): unknown[] | undefined {
        // Wasmoon formats table keys with luaL_tolstring. A __tostring metamethod would run
        // under this JS frame, where a nested host call cannot suspend the WASM stack.
        const address = thread.address
        const top = this.lua.lua_gettop(address)
        const format = thread.indexToString
        let strings: string[] | undefined
        if (this.lua.lua_type(address, 1) === LuaType.Table) {
            strings = []
            for (let i = 1, count = this.lua.lua_rawlen(address, 1); i <= count; i++) {
                this.lua.lua_rawgeti(address, 1, BigInt(i))
                strings.push(this.lua.lua_tolstring(address, -1, null))
                this.lua.lua_settop(address, -2)
            }
        }
        let used = 0
        thread.indexToString = strings
            ? index => used < strings.length ? strings[used++] : format.call(thread, index)
            : index => {
                if (this.lua.luaL_getmetafield(address, index, '__tostring') !== LuaType.Nil) throw formatInLua
                return format.call(thread, index)
            }
        try { return thread.getStackValues(1) }
        catch (error) {
            if (error !== formatInLua) throw error
            this.lua.lua_settop(address, top)
        }
    }

    private async resume(thread: LuaThread, count: number, invocation: number) {
        return this.exclusive(async () => {
            this.currentInvocation = invocation
            const pointer = this.module._malloc(4)
            let returned = false
            try {
                this.controlGC(true)
                let status = this.execution.run(() => this.lua.lua_resume(thread.address, null, count, pointer))
                while (this.exports.asyncify_get_state() === 1) {
                    this.exports.asyncify_stop_unwind()
                    this.reply = await this.pending!
                    this.pending = undefined
                    if (this.closed) throw new Error('Lua executor closed')
                    this.exports.asyncify_start_rewind(this.data)
                    status = this.execution.run(() => this.lua.lua_resume(thread.address, null, count, pointer))
                }
                returned = true
                this.controlGC(false)
                thread.assertOk(status)
                if (this.budget.expired(this.engine)) throw new Error('thread timeout exceeded')
                if (status === LuaReturn.Yield) {
                    const count = this.module.getValue(pointer, 'i32') as number
                    const yielded = count > 0 ? thread.getValue(-1) : undefined
                    thread.pop(count)
                    return { yielded, done: false, values: [] }
                }
                return { done: true, values: thread.getStackValues() }
            } catch (error) {
                if (!returned) this.tainted = true
                if (this.budget.didTimeout(this.engine)) throw new Error('thread timeout exceeded')
                throw error
            } finally {
                if (!this.tainted) this.module._free(pointer)
            }
        })
    }

    private async run(thread: LuaThread, count: number, invocation = this.invocation): Promise<unknown[]> {
        while (true) {
            const result = await this.resume(thread, count, invocation)
            if (result.done) return result.values
            if (result.yielded instanceof Promise) await result.yielded
            else await new Promise(resolve => setTimeout(resolve, 0))
            await this.exclusive(() => this.budget.startSegment(this.engine,
                this.sourceDeadline === undefined ? this.timeout : this.sourceDeadline - this.execution.now()))
            count = 0
        }
    }

    private async withThread(load: (thread: LuaThread) => number | void, args: unknown[], invocation = this.invocation) {
        const owned = await this.exclusive(() => {
            this.budget.startSegment(this.engine,
                this.sourceDeadline === undefined ? this.timeout : this.sourceDeadline - this.execution.now())
            const thread = this.engine.global.newThread()
            const reference = this.lua.luaL_ref(this.engine.global.address, LUA_REGISTRYINDEX)
            try {
                const extra = load(thread) ?? 0
                for (const value of args) thread.pushValue(value)
                return { thread, reference, count: args.length + (typeof extra === 'number' ? extra : 0) }
            } catch (error) {
                thread.close()
                this.lua.luaL_unref(this.engine.global.address, LUA_REGISTRYINDEX, reference)
                throw error
            }
        })
        try { return await this.run(owned.thread, owned.count, invocation) }
        finally {
            if (!this.tainted) await this.exclusive(() => {
                owned.thread.close()
                this.lua.luaL_unref(this.engine.global.address, LUA_REGISTRYINDEX, owned.reference)
            })
        }
    }

    isTainted(): boolean { return this.tainted }

    abandon(): void {
        // An isolated WASM instance can be collected without entering user finalizers
        // or trying to close a Lua state whose C stack is suspended.
        this.closed = true
        this.tainted = true
        this.active = false
    }

    private async callReference(_parent: LuaThread, ref: number, args: unknown[], invocation: number) {
        if (this.closed) return
        try {
            const values = await this.withThread(thread => {
                this.lua.lua_rawgeti(thread.address, LUA_REGISTRYINDEX, BigInt(this.callbackEntry))
                const type = this.lua.lua_rawgeti(thread.address, LUA_REGISTRYINDEX, BigInt(ref))
                if (type !== LuaType.Function) throw new Error('Lua callback is not callable')
                return 1
            }, args, invocation)
            return values[0] ?? null
        } catch (error) {
            if (!this.closed) throw error
        }
    }

    async initialize(source: string): Promise<void> {
        this.invocation++
        this.active = true
        this.sourceDeadline = this.execution.now() + 5_000
        this.budget.setSourceDeadline(this.engine, this.sourceDeadline)
        try { await this.withThread(thread => {
            this.lua.lua_rawgeti(thread.address, LUA_REGISTRYINDEX, BigInt(this.sourceLoader))
        }, [source]) }
        finally {
            this.active = false
            this.sourceDeadline = undefined
            if (!this.tainted) this.budget.setSourceDeadline(this.engine, undefined)
        }
    }

    async invoke(name: string, args: unknown[]): Promise<unknown> {
        this.invocation++
        this.active = true
        try {
            const values = await this.withThread(thread => {
                this.lua.lua_rawgeti(thread.address, LUA_REGISTRYINDEX, BigInt(this.entrypoint))
            }, [name, ...args])
            return values[0] ? await (values[1] ?? null) : undefined
        } finally { this.active = false }
    }

}
