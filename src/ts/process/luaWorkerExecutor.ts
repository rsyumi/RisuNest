import { LuaDeferredReply, LuaWorkerStrings, decodeLuaFailure, encodeLuaFailure, type LuaWorkerCommand, type LuaWorkerEvent } from './luaWorkerProtocol'
import { LuaWorkerInterruptedError, LuaWorkerTransport } from './luaWorkerTransport'

type HostBoundary = { run<T>(callback: () => T): T; flush(): void }

export class LuaWorkerUnavailableError extends Error {
    constructor(cause: unknown) { super('Lua Worker could not start', { cause }) }
}

export class LuaWorkerExecutor {
    private transport: LuaWorkerTransport
    private engine: number
    private readonly callbacks = new Map<string, Function>()
    private sequence = 0
    private promise = 0
    private pending?: { sequence: number; resolve: (value: unknown) => void; reject: (error: unknown) => void; signal?: AbortSignal }
    private closed = false
    private startupTimer?: ReturnType<typeof setInterval>
    private readonly strings = new LuaWorkerStrings()

    constructor(private readonly timeout: number, private readonly boundary?: HostBoundary, private readonly createWorker?: () => Worker) {
        let connection: ReturnType<typeof LuaWorkerTransport.connect>
        try { connection = LuaWorkerTransport.connect({ receive: event => this.receive(event), fail: error => this.fail(error) }, createWorker) }
        catch (error) { throw new LuaWorkerUnavailableError(error) }
        this.transport = connection.transport
        this.engine = connection.engine
    }

    register(name: string, callback: Function): void { this.callbacks.set(name, callback) }

    async initialize(source: string, json: string, signal?: AbortSignal): Promise<unknown> {
        signal?.throwIfAborted()
        const callbacks = [...this.callbacks]
        for (let attempt = 0; ; attempt++) {
            try {
                const started = this.transport.startupClock()
                this.startupTimer = setInterval(() => {
                    if (this.transport.startupClock() - started > 10_000) this.fail(new Error('Lua Worker startup timed out'))
                }, 250)
                await this.command({ kind: 'prepare', json, names: [...this.callbacks.keys()], timeout: this.timeout, sequence: ++this.sequence })
                break
            } catch (error) {
                this.close()
                if (!(error instanceof LuaWorkerInterruptedError)) throw new LuaWorkerUnavailableError(error)
                signal?.throwIfAborted()
                if (attempt > 0) throw error
                // Preparation runs no user code, so only this phase can safely be retried.
                try {
                    const connection = LuaWorkerTransport.connect({ receive: event => this.receive(event), fail: error => this.fail(error) }, this.createWorker)
                    this.transport = connection.transport
                    this.engine = connection.engine
                    this.closed = false
                    for (const [name, callback] of callbacks) this.callbacks.set(name, callback)
                } catch (cause) { throw new LuaWorkerUnavailableError(cause) }
            } finally { clearInterval(this.startupTimer) }
        }
        signal?.throwIfAborted()
        // Finish source evaluation so a superseded render retains initialized globals.
        return this.command({ kind: 'initialize', source, sequence: ++this.sequence })
    }

    invoke(name: string, args: unknown[], signal?: AbortSignal): Promise<unknown> {
        return this.command({ kind: 'invoke', name, args, sequence: ++this.sequence }, signal)
    }

    private command(message: Extract<LuaWorkerCommand, { sequence: number }>, signal?: AbortSignal): Promise<unknown> {
        if (this.closed) return Promise.reject(new Error('Lua executor closed'))
        if (this.pending) return Promise.reject(new Error('Concurrent Lua invocation'))
        if (signal?.aborted) return Promise.reject(signal.reason)
        return new Promise((resolve, reject) => {
            this.strings.clear()
            this.pending = { sequence: message.sequence, resolve, reject, signal }
            this.transport.begin(this.engine, message.kind === 'initialize' ? 5_000 : this.timeout)
            this.send(message)
        })
    }

    private send(message: LuaWorkerCommand): void {
        if (this.closed) return
        this.transport.send(this.engine, message)
    }

    private receive(message: LuaWorkerEvent): void {
        if (this.closed) return
        if (message.kind === 'heartbeat' || message.kind === 'pong' || message.kind === 'disposed' || message.kind === 'execution') return
        const pending = this.pending
        if (!pending || pending.sequence !== message.sequence) return
        if (message.kind === 'result') {
            this.boundary?.flush()
            if (message.fatal) { this.fail(new Error(message.error || 'Lua worker failed')); return }
            this.transport.end(this.engine)
            this.pending = undefined
            this.strings.clear()
            if (pending.signal?.aborted) pending.reject(pending.signal.reason)
            else if (message.failure !== undefined) pending.reject(decodeLuaFailure(message.failure))
            else if (message.error !== undefined) pending.reject(new Error(message.error))
            else pending.resolve(message.value)
            return
        }
        const started = performance.now()
        try {
            const callback = this.callbacks.get(message.name)
            if (!callback) throw new Error('Unknown Lua host API')
            const value = this.boundary ? this.boundary.run(() => callback(...message.args)) : callback(...message.args)
            if (value instanceof LuaDeferredReply) {
                const boundary = this.boundary
                boundary?.flush()
                if (boundary) value.runIn(read => boundary.run(read))
                const elapsed = performance.now() - started
                void value.read(elapsed).then(({ value, ...timingAndFailure }) => {
                    this.send({ kind: 'reply', call: message.call, ...this.strings.encode(value), ...timingAndFailure })
                })
            } else if (value && typeof value.then === 'function') {
                this.boundary?.flush()
                const promise = ++this.promise
                this.send({ kind: 'reply', call: message.call, promise, elapsed: performance.now() - started })
                void Promise.resolve(value).then(
                    value => this.send({ kind: 'settled', promise, value }),
                    error => this.send({ kind: 'settled', promise, failure: encodeLuaFailure(error) }),
                )
            } else this.send({ kind: 'reply', call: message.call, ...this.strings.encode(value), elapsed: performance.now() - started })
        } catch (error) {
            this.send({ kind: 'reply', call: message.call, failure: encodeLuaFailure(error), elapsed: performance.now() - started })
        }
    }

    isClosed(): boolean { return this.closed }

    private fail(error: Error): void {
        const pending = this.pending
        this.pending = undefined
        this.close()
        pending?.reject(error)
    }

    close(): void {
        if (this.closed) return
        this.closed = true
        clearInterval(this.startupTimer)
        this.boundary?.flush()
        this.transport.release(this.engine)
        this.callbacks.clear()
        this.strings.clear()
        const pending = this.pending
        this.pending = undefined
        pending?.reject(new Error('Lua executor closed'))
    }
}
