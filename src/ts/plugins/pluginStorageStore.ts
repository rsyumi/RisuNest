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

export const PLUGIN_STORAGE_CACHE_BYTE_BUDGET = 64 * 1024 * 1024

interface PluginStorageStoreDependencies {
    store: PersistentDataStore | (() => PersistentDataStore)
    mutate(mutations: PluginStorageMutation[]): Promise<void>
    /** Hydrated maximum-compatibility state, or null for revisioned/cache reads. */
    readCompatibilityStorage?(): Record<string, unknown> | null
}

export interface PluginStorageStore {
    getItem(key: string): Promise<unknown | null>
    setItem(key: string, value: unknown): Promise<void>
    removeItem(key: string): Promise<void>
    clear(): Promise<void>
    key(index: number): Promise<string | null>
    keys(): Promise<string[]>
    length(): Promise<number>
    snapshot(): Promise<Record<string, unknown>>
    mutate(mutations: readonly PluginStorageMutation[]): Promise<void>
    invalidate(): void
    preloadCompatibility(): Promise<void>
    preloadCompatibilityValues(storage: Record<string, unknown>): void
    setEvictionAllowed(allowed: boolean): void
    synchronizeCompatibilityStorage(storage: Record<string, unknown>): void
    synchronizeCompatibilityMutation(mutation: PluginStorageMutation): void
    synchronizeCompatibilityOrder(keys: readonly string[]): void
}

let lifecycleStore: PluginStorageStore | null = null

export function registerPluginStorageLifecycle(store: PluginStorageStore): () => void {
    lifecycleStore = store
    return () => {
        if (lifecycleStore === store) lifecycleStore = null
    }
}

export function notifyPluginStorageAuthorityReplacement(
    compatibilityStorage: Record<string, unknown> | null,
): void {
    if (compatibilityStorage === null) lifecycleStore?.invalidate()
    else lifecycleStore?.synchronizeCompatibilityStorage(compatibilityStorage)
}

export function notifyPluginStorageCompatibilityMutation(
    mutation: PluginStorageMutation,
): void {
    lifecycleStore?.synchronizeCompatibilityMutation(mutation)
}

export function notifyPluginStorageCompatibilityOrder(
    keys: readonly string[],
): void {
    lifecycleStore?.synchronizeCompatibilityOrder(keys)
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

export function readCompatibilityPluginStorageValue(
    storage: Record<string, unknown>,
    key: string,
): unknown | null {
    // Svelte retains the original descriptor after deletion, while its `has`
    // trap reports the current live membership.
    return Object.prototype.hasOwnProperty.call(storage, key) && key in storage
        ? storage[key]
        : null
}

interface CacheEntry {
    value: unknown
    byteSize: number
}

function serializedByteSize(value: unknown): number {
    return new TextEncoder().encode(JSON.stringify(value) ?? 'null').byteLength
}

function clonePluginStorageValue<T>(value: T): T {
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
    const index = new Map<string, PluginStorageSummary>()
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
                    for (const item of catalog.items) index.set(item.key, item)
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

    const putCached = (key: string, value: unknown, byteSize: number) => {
        cache.set(key, { value: clonePluginStorageValue(value), byteSize })
    }

    const replaceCachedStorage = (storage: Record<string, unknown>) => {
        authorityGeneration++
        lifecycleGeneration++
        pendingReads.clear()
        keyGenerations.clear()
        initialized = true
        index.clear()
        cache.clear()
        for (const [key, value] of Object.entries(storage)) {
            const byteSize = serializedByteSize(value)
            index.set(key, { key, byteSize })
            putCached(key, value, byteSize)
        }
    }

    const bumpKeyGeneration = (key: string): number => {
        const generation = (keyGenerations.get(key) ?? 0) + 1
        keyGenerations.set(key, generation)
        pendingReads.delete(key)
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

    const read = async (key: string): Promise<unknown | null> => {
        const live = dependencies.readCompatibilityStorage?.()
        if (live != null) {
            // Arbitrary live edits can bypass the cache notifier. Detach only
            // this value, preserving the full compatibility API's read behavior.
            return clonePluginStorageValue(
                readCompatibilityPluginStorageValue(live, key),
            )
        }
        await initialize()
        const cached = cache.get(key)
        if (cached) {
            return structuredClone(cached.value)
        }
        if (!index.has(key)) return null
        const keyGeneration = keyGenerations.get(key) ?? 0
        const pending = pendingReads.get(key)
        if (pending?.generation === keyGeneration) return pending.promise
        const readLifecycleGeneration = lifecycleGeneration
        const reading = getStore().readPluginStorage(key).then((record) => {
            const isCurrent =
                readLifecycleGeneration === lifecycleGeneration &&
                keyGeneration === (keyGenerations.get(key) ?? 0)
            if (!record) {
                if (isCurrent) index.delete(key)
                return null
            }
            const byteSize = index.get(key)?.byteSize ?? serializedByteSize(record.value)
            if (isCurrent) {
                putCached(key, record.value, byteSize)
            }
            return structuredClone(record.value)
        }).finally(() => {
            const pending = pendingReads.get(key)
            if (pending?.promise === reading) pendingReads.delete(key)
        })
        pendingReads.set(key, { generation: keyGeneration, promise: reading })
        return reading
    }

    const applyCommittedMutation = (mutation: PluginStorageMutation) => {
        if (mutation.type === 'clear') {
            resetCachedState()
            initialized = true
            return
        }
        bumpKeyGeneration(mutation.key)
        if (mutation.type === 'delete') {
            index.delete(mutation.key)
            cache.delete(mutation.key)
            return
        }
        const byteSize = serializedByteSize(mutation.value)
        index.set(mutation.key, { key: mutation.key, byteSize })
        putCached(mutation.key, mutation.value, byteSize)
    }

    const mutate = async (mutations: readonly PluginStorageMutation[]) => {
        await initialize()
        if (mutations.length === 0) return
        const expectedAuthorityGeneration = authorityGeneration
        await dependencies.mutate([...mutations])
        if (expectedAuthorityGeneration !== authorityGeneration) return
        for (const mutation of mutations) applyCommittedMutation(mutation)
    }

    const orderedKeys = (): string[] => Object.keys(
        Object.fromEntries([...index.keys()].map((key) => [key, true])),
    )

    return {
        getItem: read,
        async setItem(key, value) {
            await mutate([{ type: 'set', key, value }])
        },
        async removeItem(key) {
            await mutate([{ type: 'delete', key }])
        },
        async clear() {
            await mutate([{ type: 'clear' }])
        },
        async key(position) {
            const live = dependencies.readCompatibilityStorage?.()
            if (live != null) return Object.keys(live)[position] ?? null
            await initialize()
            return orderedKeys()[position] ?? null
        },
        async keys() {
            const live = dependencies.readCompatibilityStorage?.()
            if (live != null) return Object.keys(live)
            await initialize()
            return orderedKeys()
        },
        async length() {
            const live = dependencies.readCompatibilityStorage?.()
            if (live != null) return Object.keys(live).length
            await initialize()
            return index.size
        },
        async snapshot() {
            const lease = await acquirePinnedPluginStorageLease()
            return withPersistentRevisionLease(lease, async (reader) => {
                const pinnedCatalog = await reader.queryPluginStorage()
                const storage: Record<string, unknown> = {}
                for (const item of pinnedCatalog.items) {
                    const record = await reader.readPluginStorage(item.key)
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
        mutate,
        invalidate,
        async preloadCompatibility() {
            await initialize()
            authorityGeneration++
            lifecycleGeneration++
            pendingReads.clear()
            keyGenerations.clear()
            cache.setBudgetEnforcement(false)
            const lease = await acquirePinnedPluginStorageLease()
            await withPersistentRevisionLease(lease, async (reader) => {
                const pinnedCatalog = await reader.queryPluginStorage()
                index.clear()
                cache.clear()
                for (const item of pinnedCatalog.items) {
                    index.set(item.key, item)
                    const record = await reader.readPluginStorage(item.key)
                    if (record) putCached(item.key, record.value, item.byteSize)
                }
            })
        },
        preloadCompatibilityValues(storage) {
            cache.setBudgetEnforcement(false)
            replaceCachedStorage(storage)
        },
        setEvictionAllowed(allowed) {
            cache.setBudgetEnforcement(allowed)
        },
        synchronizeCompatibilityStorage(storage) {
            replaceCachedStorage(storage)
        },
        synchronizeCompatibilityMutation(mutation) {
            authorityGeneration++
            applyCommittedMutation(mutation)
        },
        synchronizeCompatibilityOrder(keys) {
            const entries = keys.flatMap((key) => {
                const entry = index.get(key)
                return entry ? [entry] : []
            })
            index.clear()
            for (const entry of entries) index.set(entry.key, entry)
        },
    }
}
