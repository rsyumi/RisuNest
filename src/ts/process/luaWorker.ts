import { LuaFactory } from 'wasmoon'
import { loadLuaAsyncifyModule } from './luaAsyncifyModule'
import { LuaAsyncifyRuntime } from './luaAsyncifyRuntime'
import { LuaWorkerStrings, luaErrorMessage, encodeLuaFailure, decodeLuaFailure, type LuaWorkerCommand, type LuaWorkerEvent, type LuaWorkerRequest, type LuaWorkerResponse } from './luaWorkerProtocol'

let totalExecutionMs = 0

class WorkerEngine {
    active = false
    private runtime?: LuaAsyncifyRuntime
    private sequence = 0
    private call = 0
    private disposed = false
    private executionMs = 0
    private readonly strings = new LuaWorkerStrings()
    private readonly replies = new Map<number, (reply: Extract<LuaWorkerCommand, { kind: 'reply' }>) => void>()
    private readonly promises = new Map<number, {
        promise: Promise<unknown>
        resolve(value: unknown): void
        reject(error: unknown): void
    }>()

    constructor(private readonly id: number) {}

    send(event: LuaWorkerEvent): void {
        if (!this.disposed) postMessage({ engine: this.id, event } satisfies LuaWorkerResponse)
    }

    async receive(message: LuaWorkerCommand): Promise<void> {
        if (message.kind === 'reply') {
            if (!this.replies.has(message.call)) return
            if (message.promise !== undefined) {
                const entry = {} as (typeof this.promises extends Map<number, infer T> ? T : never)
                entry.promise = new Promise((resolve, reject) => { entry.resolve = resolve; entry.reject = reject })
                // A script may deliberately leave a rejected host promise unobserved.
                void entry.promise.catch(() => {})
                this.promises.set(message.promise, entry)
            }
            try { this.replies.get(message.call)?.({ ...message, value: this.strings.decode(message) }) }
            catch (error) { this.replies.get(message.call)?.({ kind: 'reply', call: message.call, error: luaErrorMessage(error) }) }
            this.replies.delete(message.call)
            return
        }
        if (message.kind === 'settled') {
            const entry = this.promises.get(message.promise)
            if (message.failure !== undefined) entry?.reject(decodeLuaFailure(message.failure))
            else entry?.resolve(message.value)
            this.promises.delete(message.promise)
            return
        }
        if (this.active) {
            this.send({ kind: 'result', sequence: message.sequence, error: 'Concurrent Lua invocation' })
            return
        }
        this.active = true
        this.strings.clear()
        this.sequence = message.sequence
        try {
            let value: unknown
            if (message.kind === 'prepare') {
                if (this.runtime) throw new Error('Lua worker already initialized')
                const { url, module } = await loadLuaAsyncifyModule()
                const factory = new LuaFactory(url, undefined, module)
                await factory.mountFile('json.lua', message.json)
                if (this.disposed) return
                const engine = await factory.createEngine({ injectObjects: true, enableProxy: false })
                if (this.disposed) { engine.global.close(); return }
                this.runtime = new LuaAsyncifyRuntime(engine, message.timeout,
                    (name, args) => new Promise((resolve, reject) => {
                        const call = ++this.call
                        this.replies.set(call, resolve)
                        try { this.send({ kind: 'call', sequence: this.sequence, call, name, args }) }
                        catch (error) { this.replies.delete(call); reject(error) }
                    }),
                    id => {
                        const entry = this.promises.get(id)
                        if (!entry) throw new Error('Missing Lua host promise')
                        return entry.promise
                    }, {
                        // Source waits retain their wall budget, excluding other engines' execution.
                        now: () => performance.now() - totalExecutionMs + this.executionMs,
                        run: run => {
                            this.send({ kind: 'execution', running: true })
                            const started = performance.now()
                            try { return run() }
                            finally {
                                const elapsed = performance.now() - started
                                totalExecutionMs += elapsed
                                this.executionMs += elapsed
                                this.send({ kind: 'execution', running: false })
                            }
                        },
                    })
                for (const name of message.names) this.runtime.register(name)
            } else {
                if (!this.runtime) throw new Error('Lua worker is not initialized')
                if (message.kind === 'initialize') await this.runtime.initialize(message.source)
                else value = await this.runtime.invoke(message.name, message.args)
            }
            this.send({ kind: 'result', sequence: this.sequence, value })
        } catch (error) {
            this.send({ kind: 'result', sequence: this.sequence, error: luaErrorMessage(error), failure: encodeLuaFailure(error), fatal: this.runtime?.isTainted() })
        } finally {
            this.active = false
            this.strings.clear()
        }
    }

    dispose(): void {
        this.disposed = true
        this.runtime?.abandon()
        for (const [call, resolve] of this.replies) resolve({ kind: 'reply', call, error: 'Lua executor closed' })
        this.replies.clear()
        for (const entry of this.promises.values()) entry.reject(new Error('Lua executor closed'))
        this.promises.clear()
        this.strings.clear()
        this.runtime = undefined
    }
}

const engines = new Map<number, WorkerEngine>()
setInterval(() => { for (const engine of engines.values()) if (engine.active) engine.send({ kind: 'heartbeat' }) }, 250)
self.onmessage = (event: MessageEvent<LuaWorkerRequest>) => {
    const { engine: id, command } = event.data
    if (command.kind === 'ping') {
        postMessage({ engine: id, event: { kind: 'pong', token: command.token } } satisfies LuaWorkerResponse)
        return
    }
    if (command.kind === 'dispose') {
        engines.get(id)?.dispose()
        engines.delete(id)
        postMessage({ engine: id, event: { kind: 'disposed' } } satisfies LuaWorkerResponse)
        return
    }
    if (command.kind === 'prepare' && !engines.has(id)) engines.set(id, new WorkerEngine(id))
    void engines.get(id)?.receive(command)
}
