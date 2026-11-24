import type { PluginDatabaseAccess } from './pluginDatabaseAccess'

// Shapes and permission rules shared by the risunest* data methods. Like the other
// risunest* methods they are not declared in risuai.d.ts and change together with
// RisuNest's own plugins.

export type RisunestTarget = { id: string }

export type RisunestWriteResult =
    | { status: 'applied' }
    | { status: 'denied' }
    | { status: 'not-found' }
    | { status: 'rejected'; reason: string }

export type RisunestJsonKind = 'string' | 'number' | 'boolean' | 'array' | 'object'

export interface RisunestHostContext {
    /** The plugin every call is confined to. */
    pluginName: string
    databaseAccess: PluginDatabaseAccess
    /** The periodic `db` permission, the one `getDatabase` asks for. */
    hasDatabasePermission(): Promise<boolean>
    /** The periodic `dbWrite` permission. It answers false once the plugin starts unloading. */
    hasDatabaseWritePermission(): Promise<boolean>
    lifetimeSignal: AbortSignal
}

export function throwIfAborted(signal: AbortSignal): void {
    if (!signal.aborted) return
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

export function isPlainJsonObject(value: unknown): value is Record<string, unknown> {
    if (!value || typeof value !== 'object' || Array.isArray(value)) return false
    const prototype = Object.getPrototypeOf(value)
    return prototype === Object.prototype || prototype === null
}

/** The JSON kind of a value, or null for anything JSON cannot hold (including null itself). */
export function jsonKindOf(value: unknown): RisunestJsonKind | null {
    if (typeof value === 'string') return 'string'
    if (typeof value === 'number') return Number.isFinite(value) ? 'number' : null
    if (typeof value === 'boolean') return 'boolean'
    if (Array.isArray(value)) return 'array'
    if (isPlainJsonObject(value)) return 'object'
    return null
}

export function normalizeTarget(input: unknown): RisunestTarget {
    if (!isPlainJsonObject(input) || typeof input.id !== 'string' || input.id.length === 0) {
        throw new TypeError('target must be an object with a non-empty string id')
    }
    return { id: input.id }
}

/** Rejects unless the plugin holds the `db` permission. */
export async function requireDatabaseRead(
    context: Pick<RisunestHostContext, 'hasDatabasePermission' | 'lifetimeSignal'>,
    method: string,
): Promise<void> {
    throwIfAborted(context.lifetimeSignal)
    const granted = await context.hasDatabasePermission()
    throwIfAborted(context.lifetimeSignal)
    if (!granted) throw new Error(`${method} requires the db permission`)
}

/** Runs a write once the plugin holds `dbWrite`. A declined prompt answers `denied` and writes nothing. */
export async function runDatabaseWrite<T>(
    context: Pick<RisunestHostContext, 'hasDatabaseWritePermission' | 'lifetimeSignal'>,
    write: () => Promise<T>,
): Promise<T | { status: 'denied' }> {
    throwIfAborted(context.lifetimeSignal)
    const granted = await context.hasDatabaseWritePermission()
    throwIfAborted(context.lifetimeSignal)
    if (!granted) return { status: 'denied' }
    return write()
}
