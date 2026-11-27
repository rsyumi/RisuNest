import { createLuaFactory } from './luaRuntime'
import { loadLuaAsyncifyModule } from './luaAsyncifyModule'
import { LuaAsyncifyRuntime } from './luaAsyncifyRuntime'
import { LuaDeferredReply } from './luaWorkerProtocol'

/** The output script needs a suspendable host call even when a Worker cannot start. */
export class LuaLocalOutputExecutor {
    private readonly callbacks = new Map<string, Function>()
    private runtime?: LuaAsyncifyRuntime
    private closed = false

    constructor(private readonly timeout: number) {}

    register(name: string, callback: Function) { this.callbacks.set(name, callback) }

    async initialize(source: string, json: string, signal?: AbortSignal) {
        const factory = await createLuaFactory(await loadLuaAsyncifyModule())
        await factory.mountFile('json.lua', json)
        const engine = await factory.createEngine({ injectObjects: true, enableProxy: false, functionTimeout: this.timeout })
        this.runtime = new LuaAsyncifyRuntime(engine, this.timeout, async (name, args) => {
            // Let Asyncify unwind the Lua stack before the host can call back into the app.
            await Promise.resolve()
            const started = performance.now()
            const result = this.callbacks.get(name)!(...args)
            const elapsed = performance.now() - started
            // An ordinary promise is a Lua value. A synchronous chat API waits on the host stack.
            if (result instanceof LuaDeferredReply) return result.read(elapsed)
            return { value: result, elapsed }
        }, () => { throw new Error('Unexpected worker promise') })
        for (const name of this.callbacks.keys()) this.runtime.register(name)
        signal?.throwIfAborted()
        await this.runtime.initialize(source)
    }

    async invoke(name: string, args: unknown[], signal?: AbortSignal) {
        signal?.throwIfAborted()
        try { return await this.runtime!.invoke(name, args) }
        finally {
            if (this.runtime?.isTainted()) this.close()
            signal?.throwIfAborted()
        }
    }

    isClosed() { return this.closed }

    close() {
        this.closed = true
        this.runtime?.abandon()
        this.runtime = undefined
        this.callbacks.clear()
    }
}
