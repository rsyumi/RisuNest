import {
    type PersistentDataStore,
    type PluginStorageMutation,
    type PluginStorageSummary,
} from '../storage/persistentDataStore'
import { defineOwnEnumerableProperty } from '../storage/ownEnumerableProperty'
import { ByteBudgetLru } from '../util/byteBudgetLru'
import {
    acquireCurrentRevisionWithRetry,
    withPersistentRevisionLease,
} from '../storage/persistentRecordIterator'
import { UNOWNED_PLUGIN_OWNER } from './pluginOwner'

export const PLUGIN_STORAGE_CACHE_BYTE_BUDGET = 64 * 1024 * 1024

interface PluginStorageStoreDependencies {
    store: PersistentDataStore | (() => PersistentDataStore)
    getStorageAuthorityEpoch(): number
    assertPersistentMutationAllowed(expectedAuthorityEpoch?: number): void
    mutate(mutations: PluginStorageMutation[]): Promise<void>
}

/** What a plugin sees. Every call is confined to the plugin that made it. */
export interface PluginOwnerStorage {
    getItem(key: string): Promise<unknown | null>
    setItem(key: string, value: unknown): Promise<void>
    removeItem(key: string): Promise<void>
    clear(): Promise<void>
    key(index: number): Promise<string | null>
    keys(): Promise<string[]>
    length(): Promise<number>
    snapshot(): Promise<Record<string, unknown>>
    mutate(mutations: readonly OwnerScopedStorageMutation[]): Promise<void>
}

export type OwnerScopedStorageMutation =
    | { type: 'set'; key: string; value: unknown }
    | { type: 'delete'; key: string }
    | { type: 'clear' }

export interface PluginStorageStore {
    forOwner(owner: string): PluginOwnerStorage
    /** The single plugin holding a key, or the sentinel when that is unclear. */
    ownerOf(key: string): string
    invalidate(): void
    invalidateOwner(owner: string): void
    synchronizeCommittedMutation(mutation: PluginStorageMutation): void
}

let lifecycleStore: PluginStorageStore | null = null

export function registerPluginStorageLifecycle(store: PluginStorageStore): () => void {
    lifecycleStore = store
    return () => {
        if (lifecycleStore === store) lifecycleStore = null
    }
}

/**
 * The flat working set cannot carry ownership, so an authority replacement drops
 * the caches instead of seeding them. Each plugin reloads its own rows on the
 * next call.
 */
export function notifyPluginStorageAuthorityReplacement(): void {
    lifecycleStore?.invalidate()
}

/** One owner's rows changed outside this WebView; that owner reloads on demand. */
export function notifyPluginStorageOwnerChanged(owner: string): void {
    lifecycleStore?.invalidateOwner(owner)
}

export function notifyPluginStorageCompatibilityMutation(
    mutation: PluginStorageMutation,
): void {
    lifecycleStore?.synchronizeCommittedMutation(mutation)
}

/** The owner of a key as the loaded index sees it. */
export function resolveLifecyclePluginStorageOwner(key: string): string {
    return lifecycleStore?.ownerOf(key) ?? UNOWNED_PLUGIN_OWNER
}

export function observePluginStorageValue<T>(value: T, onMutation: (value: T) => void): T {
    if (!value || typeof value !== 'object') return value
    const proxies = new WeakMap<object, object>()
    const observe = (candidate: object): object => {
        const prototype = Object.getPrototypeOf(candidate)
        if (!Array.isArray(candidate) && prototype !== Object.prototype && prototype !== null) {
            return candidate
        }
        const existing = proxies.get(candidate)
        if (existing) return existing
        const proxy = new Proxy(candidate, {
            get(target, property, receiver) {
                const nested = Reflect.get(target, property, receiver)
                return nested && typeof nested === 'object' ? observe(nested) : nested
            },
            set(target, property, nextValue) {
                const changed = Reflect.set(target, property, nextValue)
                if (changed) onMutation(value)
                return changed
            },
            deleteProperty(target, property) {
                const changed = Reflect.deleteProperty(target, property)
                if (changed) onMutation(value)
                return changed
            },
        })
        proxies.set(candidate, proxy)
        return proxy
    }
    return observe(value) as T
}

interface CacheEntry {
    value: unknown
    byteSize: number
}

function serializedByteSize(value: unknown): number {
    if (typeof value === 'string') {
        // Count UTF-8 JSON bytes without allocating a second large string and buffer.
        let bytes = 2
        for (let index = 0; index < value.length; index++) {
            const code = value.charCodeAt(index)
            if (
                code === 0x22 ||
                code === 0x5c ||
                code === 8 ||
                code === 9 ||
                code === 10 ||
                code === 12 ||
                code === 13
            ) {
                bytes += 2
            } else if (code < 0x20) bytes += 6
            else if (code < 0x80) bytes++
            else if (code < 0x800) bytes += 2
            else if (code >= 0xd800 && code <= 0xdfff) {
                const next = value.charCodeAt(index + 1)
                if (code <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) {
                    bytes += 4
                    index++
                } else bytes += 6
            } else bytes += 3
        }
        return bytes
    }
    return new TextEncoder().encode(JSON.stringify(value) ?? 'null').byteLength
}

function clonePluginStorageValue<T>(value: T): T {
    if (typeof value === 'string') return value
    try {
        return structuredClone(value)
    } catch (error) {
        if (
            !error ||
            typeof error !== 'object' ||
            !('name' in error) ||
            error.name !== 'DataCloneError'
        ) {
            throw error
        }
    }

    const clones = new WeakMap<object, object>()
    const detachReactiveValue = (candidate: unknown): unknown => {
        if (candidate === null) return candidate
        if (typeof candidate === 'symbol' || typeof candidate === 'function') {
            return structuredClone(candidate)
        }
        if (typeof candidate !== 'object') return candidate

        const existing = clones.get(candidate)
        if (existing) return existing

        const isArray = Array.isArray(candidate)
        const prototype = Object.getPrototypeOf(candidate)
        if (!isArray && prototype !== Object.prototype && prototype !== null) {
            return structuredClone(candidate)
        }

        const clone: Record<string, unknown> | unknown[] = isArray
            ? new Array(candidate.length)
            : Object.create(prototype)
        clones.set(candidate, clone)
        for (const key of Object.keys(candidate)) {
            defineOwnEnumerableProperty(
                clone,
                key,
                detachReactiveValue((candidate as Record<string, unknown>)[key]),
            )
        }
        return clone
    }

    return detachReactiveValue(value) as T
}


export function createPluginStorageStore(
    dependencies: PluginStorageStoreDependencies,
    byteBudget = PLUGIN_STORAGE_CACHE_BYTE_BUDGET,
): PluginStorageStore {
    /** Owner to its own keys, in the order the store reports them. */
    const index = new Map<string, Map<string, PluginStorageSummary>>()
    const cache = new ByteBudgetLru<string, CacheEntry>(
        byteBudget,
        (_key, entry) => entry.byteSize,
    )
    const pendingReads = new Map<string, { generation: number; promise: Promise<unknown | null> }>()
    const keyGenerations = new Map<string, number>()
    let initialized = false
    let initializePromise: Promise<void> | null = null
    let lifecycleGeneration = 0
    let authorityGeneration = 0

    // Length prefixed so no owner or key pair can collide, and cheap enough to
    // run on every read of a large value.
    const cacheKey = (owner: string, key: string): string =>
        `${owner.length}:${owner}${key}`

    const getStore = (): PersistentDataStore =>
        typeof dependencies.store === 'function'
            ? dependencies.store()
            : dependencies.store

    const acquirePinnedPluginStorageLease = async () => {
        const store = getStore()
        await store.open()
        return acquireCurrentRevisionWithRetry(
            (revision) => store.acquireRevision(revision),
            async () => (await store.queryPluginStorage()).revision,
        )
    }

    const initialize = async (): Promise<void> => {
        while (!initialized) {
            let pending = initializePromise
            if (!pending) {
                const expectedGeneration = lifecycleGeneration
                const loading = (async () => {
                    const store = getStore()
                    await store.open()
                    const catalog = await store.queryPluginStorage()
                    if (expectedGeneration !== lifecycleGeneration) return
                    index.clear()
                    for (const item of catalog.items) {
                        let owned = index.get(item.owner)
                        if (!owned) {
                            owned = new Map()
                            index.set(item.owner, owned)
                        }
                        owned.set(item.key, item)
                    }
                    initialized = true
                })()
                const finalPromise = loading.finally(() => {
                    if (initializePromise === finalPromise) initializePromise = null
                })
                initializePromise = finalPromise
                pending = finalPromise
            }
            await pending
        }
    }

    const putCached = (owner: string, key: string, value: unknown, byteSize: number) => {
        cache.set(cacheKey(owner, key), { value: clonePluginStorageValue(value), byteSize })
    }

    const bumpKeyGeneration = (owner: string, key: string): number => {
        const identity = cacheKey(owner, key)
        const generation = (keyGenerations.get(identity) ?? 0) + 1
        keyGenerations.set(identity, generation)
        pendingReads.delete(identity)
        return generation
    }

    const resetCachedState = () => {
        lifecycleGeneration++
        initialized = false
        initializePromise = null
        index.clear()
        cache.clear()
        pendingReads.clear()
        keyGenerations.clear()
    }

    const invalidate = () => {
        authorityGeneration++
        resetCachedState()
    }

    const invalidateOwner = (owner: string) => {
        authorityGeneration++
        const owned = index.get(owner)
        if (owned) {
            for (const key of owned.keys()) {
                const identity = cacheKey(owner, key)
                cache.delete(identity)
                pendingReads.delete(identity)
                keyGenerations.set(identity, (keyGenerations.get(identity) ?? 0) + 1)
            }
        }
        index.delete(owner)
        initialized = false
        initializePromise = null
        lifecycleGeneration++
    }

    const read = async (owner: string, key: string): Promise<unknown | null> => {
        await initialize()
        const identity = cacheKey(owner, key)
        const cached = cache.get(identity)
        if (cached) {
            return structuredClone(cached.value)
        }
        const owned = index.get(owner)
        if (!owned?.has(key)) return null
        const keyGeneration = keyGenerations.get(identity) ?? 0
        const pending = pendingReads.get(identity)
        if (pending?.generation === keyGeneration) return pending.promise
        const readLifecycleGeneration = lifecycleGeneration
        const reading = getStore().readPluginStorage(owner, key).then((record) => {
            const isCurrent =
                readLifecycleGeneration === lifecycleGeneration &&
                keyGeneration === (keyGenerations.get(identity) ?? 0)
            if (!record) {
                if (isCurrent) index.get(owner)?.delete(key)
                return null
            }
            const byteSize = index.get(owner)?.get(key)?.byteSize
                ?? serializedByteSize(record.value)
            if (isCurrent) {
                putCached(owner, key, record.value, byteSize)
            }
            return structuredClone(record.value)
        }).finally(() => {
            const pending = pendingReads.get(identity)
            if (pending?.promise === reading) pendingReads.delete(identity)
        })
        pendingReads.set(identity, { generation: keyGeneration, promise: reading })
        return reading
    }

    const applyCommittedMutation = (mutation: PluginStorageMutation) => {
        const owner = mutation.owner
        if (mutation.type === 'clear') {
            const owned = index.get(owner)
            if (owned) {
                for (const key of owned.keys()) {
                    const identity = cacheKey(owner, key)
                    cache.delete(identity)
                    pendingReads.delete(identity)
                    keyGenerations.set(identity, (keyGenerations.get(identity) ?? 0) + 1)
                }
            }
            index.set(owner, new Map())
            return
        }
        bumpKeyGeneration(owner, mutation.key)
        let owned = index.get(owner)
        if (!owned) {
            owned = new Map()
            index.set(owner, owned)
        }
        if (mutation.type === 'delete') {
            owned.delete(mutation.key)
            cache.delete(cacheKey(owner, mutation.key))
            return
        }
        const byteSize = serializedByteSize(mutation.value)
        owned.set(mutation.key, { owner, key: mutation.key, byteSize })
        putCached(owner, mutation.key, mutation.value, byteSize)
    }

    const mutate = async (
        owner: string,
        mutations: readonly OwnerScopedStorageMutation[],
    ) => {
        if (mutations.length === 0) return
        const authorityEpoch = dependencies.getStorageAuthorityEpoch()
        dependencies.assertPersistentMutationAllowed(authorityEpoch)
        const normalized = mutations.map((mutation): PluginStorageMutation =>
            mutation.type === 'clear'
                ? { type: 'clear', owner }
                : mutation.type === 'delete' || mutation.value === undefined
                  ? { type: 'delete', owner, key: mutation.key }
                  : { type: 'set', owner, key: mutation.key, value: clonePluginStorageValue(mutation.value) },
        )
        await initialize()
        dependencies.assertPersistentMutationAllowed(authorityEpoch)
        const expectedAuthorityGeneration = authorityGeneration
        await dependencies.mutate(normalized)
        if (expectedAuthorityGeneration !== authorityGeneration) return
        for (const mutation of normalized) applyCommittedMutation(mutation)
    }

    // Object.keys supplies JavaScript's integer-key ordering, which legacy
    // plugin storage reproduces.
    const orderedKeys = (owner: string): string[] => Object.keys(
        Object.fromEntries([...(index.get(owner)?.keys() ?? [])].map((key) => [key, true])),
    )

    const forOwner = (owner: string): PluginOwnerStorage => ({
        getItem: (key) => read(owner, key),
        async setItem(key, value) {
            await mutate(owner, [{ type: 'set', key, value }])
        },
        async removeItem(key) {
            await mutate(owner, [{ type: 'delete', key }])
        },
        async clear() {
            await mutate(owner, [{ type: 'clear' }])
        },
        async key(position) {
            await initialize()
            return orderedKeys(owner)[position] ?? null
        },
        async keys() {
            await initialize()
            return orderedKeys(owner)
        },
        async length() {
            await initialize()
            return index.get(owner)?.size ?? 0
        },
        async snapshot() {
            const lease = await acquirePinnedPluginStorageLease()
            return withPersistentRevisionLease(lease, async (reader) => {
                const pinnedCatalog = await reader.queryPluginStorage()
                const storage: Record<string, unknown> = {}
                for (const item of pinnedCatalog.items) {
                    if (item.owner !== owner) continue
                    const record = await reader.readPluginStorage(owner, item.key)
                    if (record) {
                        defineOwnEnumerableProperty(
                            storage,
                            item.key,
                            structuredClone(record.value),
                        )
                    }
                }
                return storage
            })
        },
        mutate: (mutations) => mutate(owner, mutations),
    })

    return {
        forOwner,
        ownerOf(key) {
            let found: string | null = null
            for (const [owner, owned] of index) {
                if (!owned.has(key)) continue
                if (found !== null) return UNOWNED_PLUGIN_OWNER
                found = owner
            }
            return found ?? UNOWNED_PLUGIN_OWNER
        },
        invalidate,
        invalidateOwner,
        synchronizeCommittedMutation(mutation) {
            authorityGeneration++
            // Before the index is loaded there is nothing to keep in step, and
            // the first read takes the store's own answer.
            if (initialized) applyCommittedMutation(mutation)
        },
    }
}
