import { describe, expect, it, vi } from 'vitest'
import {
    RevisionConflictError,
    type PersistentDataStore,
    type PluginStorageMutation,
} from '../storage/persistentDataStore'
import {
    createPluginStorageStore,
    readCompatibilityPluginStorageValue,
    observePluginStorageValue,
    notifyPluginStorageAuthorityReplacement,
    registerPluginStorageLifecycle,
} from './pluginStorageStore'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function harness(entries: Record<string, { byteSize: number; value: unknown }>, budget = 10) {
    let revision = 4
    const readPluginStorage = vi.fn(async (key: string) => {
        const entry = entries[key]
        return entry ? { revision, value: structuredClone(entry.value) } : null
    })
    const store = {
        open: vi.fn(async () => undefined),
        queryPluginStorage: vi.fn(async () => ({
            revision,
            items: Object.entries(entries)
                .sort(([left], [right]) => left.localeCompare(right))
                .map(([key, entry]) => ({ key, byteSize: entry.byteSize })),
        })),
        readPluginStorage,
        acquireRevision: vi.fn(async (requestedRevision: number) => ({
            revision: requestedRevision,
            queryPluginStorage: async () => ({
                revision: requestedRevision,
                items: Object.entries(entries)
                    .sort(([left], [right]) => left.localeCompare(right))
                    .map(([key, entry]) => ({ key, byteSize: entry.byteSize })),
            }),
            readPluginStorage: async (key: string) => {
                const entry = entries[key]
                return entry
                    ? { revision: requestedRevision, value: structuredClone(entry.value) }
                    : null
            },
            release: vi.fn(async () => undefined),
        })),
    } as unknown as PersistentDataStore
    const mutate = vi.fn(async (mutations: PluginStorageMutation[]) => {
        for (const mutation of mutations) {
            if (mutation.type === 'clear') {
                for (const key of Object.keys(entries)) delete entries[key]
            } else if (mutation.type === 'delete') {
                delete entries[mutation.key]
            } else {
                entries[mutation.key] = {
                    byteSize: new TextEncoder().encode(JSON.stringify(mutation.value) ?? 'null')
                        .byteLength,
                    value: structuredClone(mutation.value),
                }
            }
        }
        revision++
    })
    return {
        store,
        mutate,
        readPluginStorage,
        storage: createPluginStorageStore({ store, mutate }, budget),
    }
}

describe('plugin storage V3 residency', () => {
    it('boots from the key and size index and reads values only on demand', async () => {
        const { storage, store, readPluginStorage } = harness({
            alpha: { byteSize: 6, value: 'alpha' },
            beta: { byteSize: 6, value: 'beta' },
        })

        await expect(storage.keys()).resolves.toEqual(['alpha', 'beta'])
        expect(store.queryPluginStorage).toHaveBeenCalledOnce()
        expect(readPluginStorage).not.toHaveBeenCalled()

        await expect(storage.getItem('alpha')).resolves.toBe('alpha')
        await expect(storage.getItem('beta')).resolves.toBe('beta')
        await expect(storage.getItem('alpha')).resolves.toBe('alpha')
        expect(readPluginStorage.mock.calls.map(([key]) => key)).toEqual([
            'alpha',
            'beta',
            'alpha',
        ])
    })

    it('preloads every key for compatibility and keeps values while eviction is disabled', async () => {
        const { storage, store, readPluginStorage } = harness({
            alpha: { byteSize: 6, value: 'alpha' },
            beta: { byteSize: 6, value: 'beta' },
        })

        await storage.preloadCompatibility()
        readPluginStorage.mockClear()

        await expect(storage.getItem('alpha')).resolves.toBe('alpha')
        await expect(storage.getItem('beta')).resolves.toBe('beta')
        expect(readPluginStorage).not.toHaveBeenCalled()
        expect(store.acquireRevision).toHaveBeenCalledWith(4)

        storage.setEvictionAllowed(true)
        await storage.getItem('alpha')
        expect(readPluginStorage).toHaveBeenCalledWith('alpha')
    })

    it('applies mutations through the authoritative revision path and updates the cache', async () => {
        const { storage, mutate, readPluginStorage } = harness(
            { alpha: { byteSize: 6, value: 'alpha' } },
            100,
        )

        await storage.setItem('beta', { enabled: true })
        await expect(storage.getItem('beta')).resolves.toEqual({ enabled: true })
        await storage.removeItem('alpha')
        await expect(storage.getItem('alpha')).resolves.toBeNull()
        await storage.clear()
        await expect(storage.length()).resolves.toBe(0)

        expect(mutate.mock.calls.map(([mutations]) => mutations)).toEqual([
            [{ type: 'set', key: 'beta', value: { enabled: true } }],
            [{ type: 'delete', key: 'alpha' }],
            [{ type: 'clear' }],
        ])
        expect(readPluginStorage).not.toHaveBeenCalled()
    })

    it('keeps the preloaded V3 cache coherent with synchronous compatibility mutations', async () => {
        const { storage, readPluginStorage } = harness({
            alpha: { byteSize: 6, value: 'alpha' },
        })
        await storage.preloadCompatibility()
        readPluginStorage.mockClear()

        storage.synchronizeCompatibilityMutation({
            type: 'set',
            key: 'beta',
            value: { enabled: true },
        })
        storage.synchronizeCompatibilityMutation({ type: 'delete', key: 'alpha' })

        await expect(storage.keys()).resolves.toEqual(['beta'])
        await expect(storage.getItem('beta')).resolves.toEqual({ enabled: true })
        await expect(storage.getItem('alpha')).resolves.toBeNull()
        expect(readPluginStorage).not.toHaveBeenCalled()

        storage.synchronizeCompatibilityMutation({ type: 'clear' })
        await expect(storage.length()).resolves.toBe(0)

        storage.synchronizeCompatibilityStorage({ gamma: 'replacement' })
        await expect(storage.keys()).resolves.toEqual(['gamma'])
        await expect(storage.getItem('gamma')).resolves.toBe('replacement')
    })

    it('adopts values materialized for maximum compatibility without rereading records', async () => {
        const { storage, store, readPluginStorage } = harness({})

        storage.preloadCompatibilityValues({
            alpha: 'materialized',
            beta: { nested: true },
        })

        await expect(storage.keys()).resolves.toEqual(['alpha', 'beta'])
        await expect(storage.getItem('beta')).resolves.toEqual({ nested: true })
        expect(store.queryPluginStorage).not.toHaveBeenCalled()
        expect(readPluginStorage).not.toHaveBeenCalled()
    })

    it('does not let an older in-flight read overwrite a completed mutation', async () => {
        const { storage, readPluginStorage } = harness(
            { alpha: { byteSize: 5, value: 'old' } },
            100,
        )
        const oldRead = deferred<{ revision: number; value: unknown }>()
        readPluginStorage.mockImplementationOnce(() => oldRead.promise)

        const readingOldValue = storage.getItem('alpha')
        await vi.waitFor(() => expect(readPluginStorage).toHaveBeenCalledOnce())
        await storage.setItem('alpha', 'new')
        oldRead.resolve({ revision: 4, value: 'old' })

        await expect(readingOldValue).resolves.toBe('old')
        await expect(storage.getItem('alpha')).resolves.toBe('new')
        expect(readPluginStorage).toHaveBeenCalledOnce()
    })

    it('does not reuse or apply a stale null read after an oversized set', async () => {
        const entries = { alpha: { byteSize: 5, value: 'old' as unknown } }
        const { storage, readPluginStorage } = harness(entries, 4)
        const oldRead = deferred<null>()
        readPluginStorage.mockImplementationOnce(() => oldRead.promise)

        const readingOldValue = storage.getItem('alpha')
        await vi.waitFor(() => expect(readPluginStorage).toHaveBeenCalledOnce())
        const newValue = 'larger than the cache budget'
        await storage.setItem('alpha', newValue)

        const readingNewValue = storage.getItem('alpha')
        await vi.waitFor(() => expect(readPluginStorage).toHaveBeenCalledTimes(2))
        await expect(readingNewValue).resolves.toBe(newValue)
        oldRead.resolve(null)

        await expect(readingOldValue).resolves.toBeNull()
        await expect(storage.keys()).resolves.toEqual(['alpha'])
    })

    it('rebinds its catalog and cache after an authoritative replacement', async () => {
        const first = harness({ alpha: { byteSize: 5, value: 'first' } }, 100)
        const second = harness({ beta: { byteSize: 6, value: 'second' } }, 100)
        let authority = first.store
        const storage = createPluginStorageStore({
            store: () => authority,
            mutate: first.mutate,
        }, 100)

        await expect(storage.getItem('alpha')).resolves.toBe('first')
        authority = second.store
        storage.invalidate()

        await expect(storage.keys()).resolves.toEqual(['beta'])
        await expect(storage.getItem('alpha')).resolves.toBeNull()
        await expect(storage.getItem('beta')).resolves.toBe('second')
        expect(second.store.queryPluginStorage).toHaveBeenCalledOnce()
    })

    it('reloads instead of returning an empty catalog when invalidate races initialization', async () => {
        const entries = { alpha: { byteSize: 5, value: 'first' as unknown } }
        const { storage, store } = harness(entries, 100)
        const firstCatalog = deferred<{
            revision: number
            items: Array<{ key: string; byteSize: number }>
        }>()
        vi.mocked(store.queryPluginStorage).mockImplementationOnce(() => firstCatalog.promise)

        const keys = storage.keys()
        await vi.waitFor(() => expect(store.queryPluginStorage).toHaveBeenCalledOnce())
        delete entries.alpha
        Object.assign(entries, { beta: { byteSize: 6, value: 'second' } })
        storage.invalidate()
        firstCatalog.resolve({
            revision: 4,
            items: [{ key: 'alpha', byteSize: 5 }],
        })

        await expect(keys).resolves.toEqual(['beta'])
        expect(store.queryPluginStorage).toHaveBeenCalledTimes(2)
    })

    it('invalidates the registered production store after a scalable replacement', async () => {
        const entries = { alpha: { byteSize: 5, value: 'first' as unknown } }
        const { storage, store } = harness(entries, 100)
        const unregister = registerPluginStorageLifecycle(storage)
        await expect(storage.getItem('alpha')).resolves.toBe('first')
        delete entries.alpha
        Object.assign(entries, { beta: { byteSize: 6, value: 'second' } })

        notifyPluginStorageAuthorityReplacement(null)

        await expect(storage.keys()).resolves.toEqual(['beta'])
        expect(store.queryPluginStorage).toHaveBeenCalledTimes(2)
        unregister()
    })

    it('reports nested compatibility mutations against the owning plugin value', () => {
        const value = {
            nested: { count: 1, removable: true },
            items: ['first'],
        }
        const synchronize = vi.fn()
        const observed = observePluginStorageValue(value, synchronize)

        observed.nested.count = 2
        delete observed.nested.removable
        observed.items.push('second')

        expect(value).toEqual({ nested: { count: 2 }, items: ['first', 'second'] })
        expect(synchronize.mock.calls.length).toBeGreaterThanOrEqual(3)
        expect(synchronize).toHaveBeenLastCalledWith(value)
    })

    it('uses legacy Object.keys ordering for key and keys after mutations', async () => {
        const { storage } = harness({}, 100)

        await storage.mutate([
            { type: 'set', key: 'zeta', value: 1 },
            { type: 'set', key: '10', value: 10 },
            { type: 'set', key: '2', value: 0 },
            { type: 'set', key: '01', value: 1 },
            { type: 'set', key: '4294967294', value: 1 },
            { type: 'set', key: '4294967295', value: 1 },
            { type: 'set', key: '\uffffx', value: 1 },
        ])

        const expected = ['2', '10', '4294967294', 'zeta', '01', '4294967295', '\uffffx']
        await expect(storage.keys()).resolves.toEqual(expected)
        await expect(Promise.all(expected.map((_, index) => storage.key(index)))).resolves.toEqual(
            expected,
        )
        await storage.removeItem('zeta')
        await storage.setItem('zeta', 2)
        await expect(storage.keys()).resolves.toEqual([
            '2',
            '10',
            '4294967294',
            '01',
            '4294967295',
            '\uffffx',
            'zeta',
        ])
    })

    it('builds a pinned compatibility snapshot without losing zero values', async () => {
        const { storage, store } = harness({
            zero: { byteSize: 1, value: 0 },
            memory: { byteSize: 8, value: { ok: true } },
        }, 1)

        await expect(storage.snapshot()).resolves.toEqual({
            memory: { ok: true },
            zero: 0,
        })
        expect(store.acquireRevision).toHaveBeenCalledWith(4)
    })

    it('retries a transient pinned snapshot release without duplicating successful cleanup', async () => {
        const { storage, store } = harness({ zero: { byteSize: 1, value: 0 } }, 1)
        const release = vi.fn()
            .mockRejectedValueOnce(new Error('release unavailable'))
            .mockResolvedValueOnce(undefined)
        vi.mocked(store.acquireRevision).mockResolvedValueOnce({
            revision: 4,
            queryPluginStorage: async () => ({
                revision: 4,
                items: [{ key: 'zero', byteSize: 1 }],
            }),
            readPluginStorage: async () => ({ revision: 4, value: 0 }),
            release,
        } as any)

        await expect(storage.snapshot()).resolves.toEqual({ zero: 0 })
        expect(release).toHaveBeenCalledTimes(2)
    })

    it('preserves a compatibility preload failure when both release attempts fail', async () => {
        const { storage, store } = harness({ broken: { byteSize: 1, value: 1 } }, 1)
        const primaryError = new Error('plugin value unavailable')
        const release = vi.fn(async () => {
            throw new Error('release unavailable')
        })
        vi.mocked(store.acquireRevision).mockResolvedValueOnce({
            revision: 4,
            queryPluginStorage: async () => ({
                revision: 4,
                items: [{ key: 'broken', byteSize: 1 }],
            }),
            readPluginStorage: async () => {
                throw primaryError
            },
            release,
        } as any)

        await expect(storage.preloadCompatibility()).rejects.toBe(primaryError)
        expect(release).toHaveBeenCalledTimes(2)
    })

    it('preserves an own proto key and catalog order in a pinned snapshot', async () => {
        const { storage, store } = harness({}, 100)
        vi.mocked(store.acquireRevision).mockResolvedValueOnce({
            revision: 4,
            queryPluginStorage: async () => ({
                revision: 4,
                items: [
                    { key: 'zeta', byteSize: 1 },
                    { key: '0', byteSize: 1 },
                    { key: '__proto__', byteSize: 1 },
                    { key: 'alpha', byteSize: 1 },
                ],
            }),
            readPluginStorage: async (key: string) => ({
                revision: 4,
                value: key === '__proto__' ? false : key === '0' ? 0 : '',
            }),
            release: vi.fn(async () => undefined),
        } as any)

        const snapshot = await storage.snapshot()

        expect(Object.keys(snapshot)).toEqual(['0', 'zeta', '__proto__', 'alpha'])
        expect(Object.hasOwn(snapshot, '__proto__')).toBe(true)
        expect(snapshot.__proto__).toBe(false)
        expect(Object.getPrototypeOf(snapshot)).toBe(Object.prototype)
        expect(snapshot['0']).toBe(0)
        expect(snapshot.alpha).toBe('')
    })

    it('retries a pinned compatibility snapshot when the selected revision races a commit', async () => {
        const { storage, store } = harness({
            zero: { byteSize: 1, value: 0 },
        }, 1)
        vi.mocked(store.queryPluginStorage)
            .mockResolvedValueOnce({
                revision: 4,
                items: [{ key: 'zero', byteSize: 1 }],
            })
            .mockResolvedValueOnce({
                revision: 5,
                items: [{ key: 'zero', byteSize: 1 }],
            })
        vi.mocked(store.acquireRevision)
            .mockRejectedValueOnce(new RevisionConflictError(4, 5))

        await expect(storage.snapshot()).resolves.toEqual({ zero: 0 })
        expect(vi.mocked(store.acquireRevision).mock.calls.map(([revision]) => revision)).toEqual([
            4,
            5,
        ])
    })

    it('bounds pinned compatibility snapshot revision retries', async () => {
        const { storage, store } = harness({
            zero: { byteSize: 1, value: 0 },
        }, 1)
        vi.mocked(store.acquireRevision).mockRejectedValue(
            new RevisionConflictError(4, 5),
        )

        await expect(storage.snapshot()).rejects.toBeInstanceOf(RevisionConflictError)
        expect(store.acquireRevision).toHaveBeenCalledTimes(3)
    })

    it('distinguishes a stored zero from a missing compatibility key', () => {
        const storage = { zero: 0, disabled: false, empty: '' }

        expect(readCompatibilityPluginStorageValue(storage, 'zero')).toBe(0)
        expect(readCompatibilityPluginStorageValue(storage, 'disabled')).toBe(false)
        expect(readCompatibilityPluginStorageValue(storage, 'empty')).toBe('')
        expect(readCompatibilityPluginStorageValue(storage, 'missing')).toBeNull()
    })
})
