import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'
import { pluginDeviceStorage, pluginDevicePrefix } from './pluginDeviceStorage'

export type PluginDeviceSpace = 'string' | 'json'

export interface PluginDeviceEntry {
    space: PluginDeviceSpace
    key: string
    value: string
}

export interface PluginDeviceHydration {
    complete: boolean
    byteSize: number
    entries: PluginDeviceEntry[]
}

export type PluginDeviceMutation =
    | { type: 'set'; space: PluginDeviceSpace; key: string; value: string }
    | { type: 'delete'; space: PluginDeviceSpace; key: string }
    | { type: 'clear'; space: PluginDeviceSpace }

export interface PluginDeviceBackend {
    hydrate(owner: string): Promise<PluginDeviceHydration>
    read(owner: string, space: PluginDeviceSpace, key: string): Promise<string | null>
    keys(owner: string, space: PluginDeviceSpace): Promise<string[]>
    write(owner: string, mutations: readonly PluginDeviceMutation[]): Promise<void>
}

/**
 * The native store answers by owner and space, so the browser backend builds
 * the same address out of one opaque key. An owner or key holding the separator
 * still cannot reach another plugin's keyspace.
 */
function browserKey(owner: string, space: PluginDeviceSpace, key: string): string {
    return `${pluginDevicePrefix}${JSON.stringify([owner, space, key])}`
}

function browserKeyOf(stored: string, owner: string, space: PluginDeviceSpace): string | null {
    if (!stored.startsWith(pluginDevicePrefix)) return null
    try {
        const parsed: unknown = JSON.parse(stored.substring(pluginDevicePrefix.length))
        if (!Array.isArray(parsed) || parsed.length !== 3) return null
        const [storedOwner, storedSpace, key] = parsed as unknown[]
        if (storedOwner !== owner || storedSpace !== space) return null
        return typeof key === 'string' ? key : null
    } catch {
        return null
    }
}

/**
 * The web build keeps the browser stores. Values stay strings on both sides so
 * one keyspace serves either backend without a second shape.
 */
export function createBrowserPluginDeviceBackend(): PluginDeviceBackend {
    const localKeys = (owner: string): string[] => {
        const keys: string[] = []
        for (let index = 0; index < localStorage.length; index += 1) {
            const stored = localStorage.key(index)
            const key = stored === null ? null : browserKeyOf(stored, owner, 'string')
            if (key !== null) keys.push(key)
        }
        return keys
    }
    const forageKeys = async (owner: string): Promise<string[]> => {
        const keys: string[] = []
        await pluginDeviceStorage.iterate((_value, stored) => {
            const key = browserKeyOf(stored, owner, 'json')
            if (key !== null) keys.push(key)
        })
        return keys
    }
    const read = async (
        owner: string,
        space: PluginDeviceSpace,
        key: string,
    ): Promise<string | null> => {
        if (space === 'string') return localStorage.getItem(browserKey(owner, space, key))
        return await pluginDeviceStorage.getItem<string>(browserKey(owner, space, key))
    }
    return {
        async hydrate(owner: string): Promise<PluginDeviceHydration> {
            const entries: PluginDeviceEntry[] = []
            let byteSize = 0
            for (const space of ['json', 'string'] as const) {
                const keys = space === 'string' ? localKeys(owner) : await forageKeys(owner)
                for (const key of keys.sort()) {
                    const value = await read(owner, space, key)
                    if (typeof value !== 'string') continue
                    byteSize += value.length
                    entries.push({ space, key, value })
                }
            }
            return { complete: true, byteSize, entries }
        },
        read,
        async keys(owner: string, space: PluginDeviceSpace): Promise<string[]> {
            return space === 'string' ? localKeys(owner) : await forageKeys(owner)
        },
        async write(owner: string, mutations: readonly PluginDeviceMutation[]): Promise<void> {
            for (const mutation of mutations) {
                if (mutation.type === 'clear') {
                    const keys =
                        mutation.space === 'string'
                            ? localKeys(owner)
                            : await forageKeys(owner)
                    for (const key of keys) {
                        if (mutation.space === 'string') {
                            localStorage.removeItem(browserKey(owner, mutation.space, key))
                        } else {
                            await pluginDeviceStorage.removeItem(
                                browserKey(owner, mutation.space, key),
                            )
                        }
                    }
                    continue
                }
                const stored = browserKey(owner, mutation.space, mutation.key)
                if (mutation.type === 'delete') {
                    if (mutation.space === 'string') localStorage.removeItem(stored)
                    else await pluginDeviceStorage.removeItem(stored)
                    continue
                }
                if (mutation.space === 'string') localStorage.setItem(stored, mutation.value)
                else await pluginDeviceStorage.setItem(stored, mutation.value)
            }
        },
    }
}

/**
 * Each call waits for its own native transaction, so a resolved write is
 * already on disk and a later read of the same key cannot miss it.
 */
export function createNativePluginDeviceBackend(): PluginDeviceBackend {
    return {
        async hydrate(owner: string): Promise<PluginDeviceHydration> {
            return await invoke<PluginDeviceHydration>('pds_hydrate_plugin_device_storage', {
                owner,
            })
        },
        async read(
            owner: string,
            space: PluginDeviceSpace,
            key: string,
        ): Promise<string | null> {
            return (
                (await invoke<string | null>('pds_read_plugin_device_value', {
                    owner,
                    space,
                    key,
                })) ?? null
            )
        },
        async keys(owner: string, space: PluginDeviceSpace): Promise<string[]> {
            return await invoke<string[]>('pds_list_plugin_device_keys', { owner, space })
        },
        async write(owner: string, mutations: readonly PluginDeviceMutation[]): Promise<void> {
            if (mutations.length === 0) return
            await invoke('pds_write_plugin_device_values', { owner, mutations })
        },
    }
}

/**
 * One plugin's device keyspace. The whole keyspace is loaded once when it fits
 * under the native limit; a larger one is read key by key instead. Writes go
 * out as single changes and the cache follows the commit, never the other way
 * round.
 */
export class PluginDeviceKeyspace {
    readonly #owner: string
    readonly #backend: PluginDeviceBackend
    #cache: Map<PluginDeviceSpace, Map<string, string>> | null = null
    #hydration: Promise<void> | null = null

    constructor(owner: string, backend: PluginDeviceBackend) {
        this.#owner = owner
        this.#backend = backend
    }

    async #hydrate(): Promise<void> {
        if (this.#hydration !== null) {
            await this.#hydration
            return
        }
        const started = (async () => {
            const loaded = await this.#backend.hydrate(this.#owner)
            if (!loaded.complete) {
                this.#cache = null
                return
            }
            const cache = new Map<PluginDeviceSpace, Map<string, string>>([
                ['string', new Map()],
                ['json', new Map()],
            ])
            for (const entry of loaded.entries) {
                cache.get(entry.space)?.set(entry.key, entry.value)
            }
            this.#cache = cache
        })()
        this.#hydration = started
        try {
            await started
        } catch (error) {
            this.#hydration = null
            throw error
        }
    }

    async getItem(space: PluginDeviceSpace, key: string): Promise<string | null> {
        await this.#hydrate()
        const cached = this.#cache?.get(space)
        if (cached) return cached.get(key) ?? null
        return await this.#backend.read(this.#owner, space, key)
    }

    async keys(space: PluginDeviceSpace): Promise<string[]> {
        await this.#hydrate()
        const cached = this.#cache?.get(space)
        const keys = cached ? [...cached.keys()] : await this.#backend.keys(this.#owner, space)
        return keys.sort()
    }

    async setItem(space: PluginDeviceSpace, key: string, value: string): Promise<void> {
        await this.#hydrate()
        await this.#backend.write(this.#owner, [{ type: 'set', space, key, value }])
        this.#cache?.get(space)?.set(key, value)
    }

    async removeItem(space: PluginDeviceSpace, key: string): Promise<void> {
        await this.#hydrate()
        await this.#backend.write(this.#owner, [{ type: 'delete', space, key }])
        this.#cache?.get(space)?.delete(key)
    }

    async clear(space: PluginDeviceSpace): Promise<void> {
        await this.#hydrate()
        await this.#backend.write(this.#owner, [{ type: 'clear', space }])
        this.#cache?.get(space)?.clear()
    }
}

let backend: PluginDeviceBackend | null = null
const keyspaces = new Map<string, PluginDeviceKeyspace>()

function activeBackend(): PluginDeviceBackend {
    backend ??= isTauri ? createNativePluginDeviceBackend() : createBrowserPluginDeviceBackend()
    return backend
}

export function getPluginDeviceKeyspace(owner: string): PluginDeviceKeyspace {
    let keyspace = keyspaces.get(owner)
    if (!keyspace) {
        keyspace = new PluginDeviceKeyspace(owner, activeBackend())
        keyspaces.set(owner, keyspace)
    }
    return keyspace
}

/**
 * Drops the cached keyspaces so the next plugin load reads what the plugin data
 * screen left behind. A plugin already running keeps the values it holds in its
 * own memory, which the host cannot reach.
 */
export function invalidatePluginDeviceKeyspaces(owner?: string): void {
    if (owner === undefined) keyspaces.clear()
    else keyspaces.delete(owner)
}
