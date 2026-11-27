export type LuaWorkerCommand =
    | { kind: 'prepare'; sequence: number; json: string; names: string[]; timeout: number }
    | { kind: 'initialize'; sequence: number; source: string }
    | { kind: 'invoke'; sequence: number; name: string; args: unknown[] }
    | { kind: 'reply'; call: number; value?: unknown; error?: string; failure?: LuaWorkerFailure; promise?: number; stringId?: number; resetStrings?: boolean; elapsed?: number; waited?: number }
    | { kind: 'settled'; promise: number; value?: unknown; failure?: LuaWorkerFailure }

export type LuaWorkerEvent =
    | { kind: 'result'; sequence: number; value?: unknown; error?: string; failure?: LuaWorkerFailure; fatal?: boolean }
    | { kind: 'call'; sequence: number; call: number; name: string; args: unknown[] }
    | { kind: 'heartbeat' }
    | { kind: 'pong'; token: number }
    | { kind: 'execution'; running: boolean }
    | { kind: 'disposed' }

export type LuaWorkerRequest = { engine: number; command: LuaWorkerCommand | { kind: 'dispose' } | { kind: 'ping'; token: number } }
export type LuaWorkerResponse = { engine: number; event: LuaWorkerEvent }

export type LuaWorkerFailure = { kind: 'error'; message: string } | { kind: 'value'; value: unknown }

type HostCall = <T>(read: () => T) => T

/** A synchronous Lua API waiting for its conversation view to reopen. */
export class LuaDeferredReply {
    private readonly started = performance.now()
    private resumed?: number
    private host: HostCall = read => read()

    constructor(private readonly value: Promise<unknown>) {}

    /** Runs the API, once its view reopens, where the executor runs its other host calls. */
    runIn(host: HostCall) { this.host = host }

    resume<T>(read: () => T): T {
        this.resumed = performance.now()
        return this.host(read)
    }

    async read(elapsed: number) {
        let reply: { value?: unknown; failure?: LuaWorkerFailure }
        try { reply = { value: await this.value } }
        catch (error) { reply = { failure: encodeLuaFailure(error) } }
        const end = performance.now()
        const resumed = this.resumed ?? end
        return { ...reply, elapsed: elapsed + end - resumed, waited: resumed - this.started }
    }
}

export function encodeLuaFailure(error: unknown): LuaWorkerFailure {
    return error instanceof Error ? { kind: 'error', message: error.message } : { kind: 'value', value: error }
}

export function decodeLuaFailure(failure: LuaWorkerFailure): unknown {
    return failure.kind === 'error' ? new Error(failure.message) : failure.value
}

export function luaErrorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error)
}

/** Exact-value interning avoids retransmitting large, unchanged API strings. */
export class LuaWorkerStrings {
    private readonly outgoing = new Map<string, number>()
    private readonly incoming = new Map<number, string>()
    private bytes = 0
    private sequence = 0

    constructor(private readonly maxBytes = 2 * 1024 * 1024, private readonly minLength = 1024) {}

    clear(): void {
        this.outgoing.clear()
        this.incoming.clear()
        this.bytes = 0
    }

    encode(value: unknown): { value?: unknown; stringId?: number; resetStrings?: boolean } {
        if (typeof value !== 'string' || value.length < this.minLength || value.length * 2 > this.maxBytes) return { value }
        const known = this.outgoing.get(value)
        if (known !== undefined) return { stringId: known }
        const resetStrings = this.bytes + value.length * 2 > this.maxBytes || this.outgoing.size >= 8
        if (resetStrings) this.clear()
        const stringId = ++this.sequence
        this.outgoing.set(value, stringId)
        this.bytes += value.length * 2
        return { value, stringId, resetStrings }
    }

    decode(message: { value?: unknown; stringId?: number; resetStrings?: boolean }): unknown {
        if (message.resetStrings) this.incoming.clear()
        if (message.stringId === undefined) return message.value
        if (typeof message.value === 'string') this.incoming.set(message.stringId, message.value)
        const value = this.incoming.get(message.stringId)
        if (value === undefined) throw new Error('Unknown Lua string reference')
        return value
    }
}
