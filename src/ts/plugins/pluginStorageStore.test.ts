const OWNER = 'test-plugin'
import { describe, expect, it, vi } from 'vitest'
import {
    RevisionConflictError,
    type PersistentDataStore,
    type PluginStorageMutation,
} from '../storage/persistentDataStore'
import {
    createPluginStorageStore,
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
    let authorityEpoch = 0
    const assertPersistentMutationAllowed = (expected = authorityEpoch) => {
        if (expected !== authorityEpoch) throw new Error('Persistent mutation fenced')
    }
    const readPluginStorage = vi.fn(async (_owner: string, key: string) => {
        const entry = entries[key]
        return entry ? { revision, value: structuredClone(entry.value) } : null
    })
    const store = {
        open: vi.fn(async () => undefined),
        queryPluginStorage: vi.fn(async () => ({
            revision,
            items: Object.entries(entries)
                .sort(([left], [right]) => left.localeCompare(right))
                .map(([key, entry]) => ({ owner: OWNER, key, byteSize: entry.byteSize })),
        })),
        readPluginStorage,
        acquireRevision: vi.fn(async (requestedRevision: number) => ({
            revision: requestedRevision,
            queryPluginStorage: async () => ({
                revision: requestedRevision,
                items: Object.entries(entries)
                    .sort(([left], [right]) => left.localeCompare(right))
                    .map(([key, entry]) => ({ owner: OWNER, key, byteSize: entry.byteSize })),
            }),
            readPluginStorage: async (_owner: string, key: string) => {
                const entry = entries[key]
                return entry
                    ? { revision: requestedRevision, value: structuredClone(entry.value) }
                    : null
            },
            release: vi.fn(async () => undefined),
        })),
    } as unknown as PersistentDataStore
    const mutate = vi.fn(async (mutations: readonly PluginStorageMutation[]) => {
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
        storage: createPluginStorageStore({
            store, mutate,
            getStorageAuthorityEpoch: () => authorityEpoch,
            assertPersistentMutationAllowed,
        }, budget),
        advanceAuthorityEpoch() { authorityEpoch++ },
    }
}

describe('plugin storage V3 residency', () => {
    it('freezes mutations before waiting for the first storage index', async () => {
        const { storage, store, mutate } = harness({}, 100)
        const opening = deferred<void>()
        vi.mocked(store.open).mockReturnValueOnce(opening.promise)
        const value = { text: 'captured' }
        const mutations = [{ type: 'set' as const, key: 'before', value }]
        const writing = storage.forOwner(OWNER).mutate(mutations)
        value.text = 'later edit'
        mutations[0].key = 'after'
        opening.resolve()
        await writing
        expect(mutate).toHaveBeenCalledWith([
            { type: 'set', owner: OWNER, key: 'before', value: { text: 'captured' } },
        ])
    })

    it('late_plugin_result_cannot_cross_replacement while its storage index loads', async () => {
        const { storage, store, mutate, advanceAuthorityEpoch } = harness({}, 100)
        const opening = deferred<void>()
        vi.mocked(store.open).mockReturnValueOnce(opening.promise)
        const writing = storage.forOwner(OWNER).setItem('old', { text: 'old library' })
        const rejected = expect(writing).rejects.toThrow('Persistent mutation fenced')
        advanceAuthorityEpoch()
        storage.invalidate()
        opening.resolve()
        await rejected
        expect(mutate).not.toHaveBeenCalled()
    })

    it('preserves exact JSON byte budgets for strings including escaped and unpaired UTF-16', async () => {
        const values = [
            '',
            'ASCII "quoted" \\ text',
            '한글🙂é\u2028\u2029',
            '\ud800a\udc00\ud800\ud800\udc00',
            Array.from({ length: 0x10000 }, (_, code) => String.fromCharCode(code)).join(''),
        ]
        for (const value of values) {
            const byteSize = new TextEncoder().encode(JSON.stringify(value)).byteLength
            for (const budget of [byteSize, byteSize - 1]) {
                const { storage, readPluginStorage } = harness(
                    { alpha: { value, byteSize } },
                    budget,
                )
                await storage.forOwner(OWNER).keys()
                readPluginStorage.mockClear()
                storage.synchronizeCommittedMutation({
                type: 'set',
                owner: OWNER,
                key: 'alpha',
                value,
            })
                await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe(value)
                expect(readPluginStorage).toHaveBeenCalledTimes(budget < byteSize ? 1 : 0)
            }
        }
    })

    it('adopts large immutable strings without serialization, encoding or cloning copies', async () => {
        const value = 'x'.repeat(16 * 1024 * 1024)
        const { storage, readPluginStorage } = harness({}, 32 * 1024 * 1024)
        await storage.forOwner(OWNER).keys()
        const stringify = vi.spyOn(JSON, 'stringify')
        const encode = vi.spyOn(TextEncoder.prototype, 'encode')
        const clone = vi.spyOn(globalThis, 'structuredClone')
        try {
            storage.synchronizeCommittedMutation({
                type: 'set',
                owner: OWNER,
                key: 'alpha',
                value,
            })
            expect(stringify).not.toHaveBeenCalled()
            expect(encode).not.toHaveBeenCalled()
            expect(clone).not.toHaveBeenCalled()
        } finally {
            stringify.mockRestore()
            encode.mockRestore()
            clone.mockRestore()
        }
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe(value)
        expect(readPluginStorage).not.toHaveBeenCalled()
    })

    it('boots from the key and size index and reads values only on demand', async () => {
        const { storage, store, readPluginStorage } = harness({
            alpha: { byteSize: 6, value: 'alpha' },
            beta: { byteSize: 6, value: 'beta' },
        })

        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(['alpha', 'beta'])
        expect(store.queryPluginStorage).toHaveBeenCalledOnce()
        expect(readPluginStorage).not.toHaveBeenCalled()

        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe('alpha')
        await expect(storage.forOwner(OWNER).getItem('beta')).resolves.toBe('beta')
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe('alpha')
        expect(readPluginStorage.mock.calls.map(([, key]) => key)).toEqual([
            'alpha',
            'beta',
            'alpha',
        ])
    })

    it('applies mutations through the authoritative revision path and updates the cache', async () => {
        const { storage, mutate, readPluginStorage } = harness(
            { alpha: { byteSize: 6, value: 'alpha' } },
            100,
        )

        await storage.forOwner(OWNER).setItem('beta', { enabled: true })
        await expect(storage.forOwner(OWNER).getItem('beta')).resolves.toEqual({ enabled: true })
        await storage.forOwner(OWNER).removeItem('alpha')
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBeNull()
        await storage.forOwner(OWNER).clear()
        await expect(storage.forOwner(OWNER).length()).resolves.toBe(0)

        expect(mutate.mock.calls.map(([mutations]) => mutations)).toEqual([
            [{ type: 'set', owner: 'test-plugin', key: 'beta', value: { enabled: true } }],
            [{ type: 'delete', owner: 'test-plugin', key: 'alpha' }],
            [{ type: 'clear', owner: 'test-plugin' }],
        ])
        expect(readPluginStorage).not.toHaveBeenCalled()
    })

    it('normalizes an undefined set value to a deletion before commit and cache update', async () => {
        const { storage, mutate, readPluginStorage } = harness(
            { alpha: { byteSize: 6, value: 'alpha' } },
            100,
        )

        await storage.forOwner(OWNER).setItem('alpha', undefined)

        expect(mutate).toHaveBeenCalledWith([{ type: 'delete', owner: 'test-plugin', key: 'alpha' }])
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBeNull()
        expect(readPluginStorage).not.toHaveBeenCalled()
    })

    it('keeps the V3 cache coherent with published storage mutations', async () => {
        const { storage, readPluginStorage } = harness({
            alpha: { byteSize: 6, value: 'alpha' },
        }, 1024)
        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(['alpha'])
        storage.synchronizeCommittedMutation({ type: 'set', owner: OWNER, key: 'alpha', value: 'alpha' })
        readPluginStorage.mockClear()

        storage.synchronizeCommittedMutation({
            type: 'set',
            owner: 'test-plugin',
            key: 'beta',
            value: { enabled: true },
        })
        storage.synchronizeCommittedMutation({ type: 'delete', owner: 'test-plugin', key: 'alpha' })

        await expect(storage.forOwner(OWNER).getItem('beta')).resolves.toEqual({ enabled: true })
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBeNull()
        expect(readPluginStorage).not.toHaveBeenCalled()

        storage.synchronizeCommittedMutation({ type: 'clear', owner: 'test-plugin' })
        await expect(storage.forOwner(OWNER).length()).resolves.toBe(0)

        storage.synchronizeCommittedMutation({ type: 'set', owner: OWNER, key: 'gamma', value: 'replacement' })
        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(['gamma'])
        await expect(storage.forOwner(OWNER).getItem('gamma')).resolves.toBe('replacement')
    })

    it('adopts published storage values without rereading records', async () => {
        const { storage, store, readPluginStorage } = harness({}, 1024)

        await storage.forOwner(OWNER).keys()
        storage.synchronizeCommittedMutation({
            type: 'set',
            owner: OWNER,
            key: 'alpha',
            value: 'materialized',
        })
        storage.synchronizeCommittedMutation({
            type: 'set',
            owner: OWNER,
            key: 'beta',
            value: { nested: true },
        })

        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe('materialized')
        await expect(storage.forOwner(OWNER).getItem('beta')).resolves.toEqual({ nested: true })
        expect(store.queryPluginStorage).toHaveBeenCalledOnce()
        expect(readPluginStorage).not.toHaveBeenCalled()
    })

    it('does not let an older in-flight read overwrite a completed mutation', async () => {
        const { storage, readPluginStorage } = harness(
            { alpha: { byteSize: 5, value: 'old' } },
            100,
        )
        const oldRead = deferred<{ revision: number; value: unknown }>()
        readPluginStorage.mockImplementationOnce(() => oldRead.promise)

        const readingOldValue = storage.forOwner(OWNER).getItem('alpha')
        await vi.waitFor(() => expect(readPluginStorage).toHaveBeenCalledOnce())
        await storage.forOwner(OWNER).setItem('alpha', 'new')
        oldRead.resolve({ revision: 4, value: 'old' })

        await expect(readingOldValue).resolves.toBe('old')
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe('new')
        expect(readPluginStorage).toHaveBeenCalledOnce()
    })

    it('does not reuse or apply a stale null read after an oversized set', async () => {
        const entries = { alpha: { byteSize: 5, value: 'old' as unknown } }
        const { storage, readPluginStorage } = harness(entries, 4)
        const oldRead = deferred<null>()
        readPluginStorage.mockImplementationOnce(() => oldRead.promise)

        const readingOldValue = storage.forOwner(OWNER).getItem('alpha')
        await vi.waitFor(() => expect(readPluginStorage).toHaveBeenCalledOnce())
        const newValue = 'larger than the cache budget'
        await storage.forOwner(OWNER).setItem('alpha', newValue)

        const readingNewValue = storage.forOwner(OWNER).getItem('alpha')
        await vi.waitFor(() => expect(readPluginStorage).toHaveBeenCalledTimes(2))
        await expect(readingNewValue).resolves.toBe(newValue)
        oldRead.resolve(null)

        await expect(readingOldValue).resolves.toBeNull()
        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(['alpha'])
    })

    it('rebinds its catalog and cache after an authoritative replacement', async () => {
        const first = harness({ alpha: { byteSize: 5, value: 'first' } }, 100)
        const second = harness({ beta: { byteSize: 6, value: 'second' } }, 100)
        let authority = first.store
        const storage = createPluginStorageStore({
            store: () => authority,
            getStorageAuthorityEpoch: () => authority === first.store ? 0 : 1,
            assertPersistentMutationAllowed: vi.fn(),
            mutate: first.mutate,
        }, 100)

        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe('first')
        authority = second.store
        storage.invalidate()

        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(['beta'])
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBeNull()
        await expect(storage.forOwner(OWNER).getItem('beta')).resolves.toBe('second')
        expect(second.store.queryPluginStorage).toHaveBeenCalledOnce()
    })

    it('reloads instead of returning an empty catalog when invalidate races initialization', async () => {
        const entries = { alpha: { byteSize: 5, value: 'first' as unknown } }
        const { storage, store } = harness(entries, 100)
        const firstCatalog = deferred<{
            revision: number
            items: Array<{ owner: string; key: string; byteSize: number }>
        }>()
        vi.mocked(store.queryPluginStorage).mockImplementationOnce(() => firstCatalog.promise)

        const keys = storage.forOwner(OWNER).keys()
        await vi.waitFor(() => expect(store.queryPluginStorage).toHaveBeenCalledOnce())
        delete entries.alpha
        Object.assign(entries, { beta: { byteSize: 6, value: 'second' } })
        storage.invalidate()
        firstCatalog.resolve({
            revision: 4,
            items: [{ owner: 'test-plugin', key: 'alpha', byteSize: 5 }],
        })

        await expect(keys).resolves.toEqual(['beta'])
        expect(store.queryPluginStorage).toHaveBeenCalledTimes(2)
    })

    it('invalidates the registered production store after a scalable replacement', async () => {
        const entries = { alpha: { byteSize: 5, value: 'first' as unknown } }
        const { storage, store } = harness(entries, 100)
        const unregister = registerPluginStorageLifecycle(storage)
        await expect(storage.forOwner(OWNER).getItem('alpha')).resolves.toBe('first')
        delete entries.alpha
        Object.assign(entries, { beta: { byteSize: 6, value: 'second' } })

        notifyPluginStorageAuthorityReplacement()

        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(['beta'])
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

        await storage.forOwner(OWNER).mutate([
            { type: 'set', key: 'zeta', value: 1 },
            { type: 'set', key: '10', value: 10 },
            { type: 'set', key: '2', value: 0 },
            { type: 'set', key: '01', value: 1 },
            { type: 'set', key: '4294967294', value: 1 },
            { type: 'set', key: '4294967295', value: 1 },
            { type: 'set', key: '\uffffx', value: 1 },
        ])

        const expected = ['2', '10', '4294967294', 'zeta', '01', '4294967295', '\uffffx']
        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual(expected)
        await expect(Promise.all(expected.map((_, index) => storage.forOwner(OWNER).key(index)))).resolves.toEqual(
            expected,
        )
        await storage.forOwner(OWNER).removeItem('zeta')
        await storage.forOwner(OWNER).setItem('zeta', 2)
        await expect(storage.forOwner(OWNER).keys()).resolves.toEqual([
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

        await expect(storage.forOwner(OWNER).snapshot()).resolves.toEqual({
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
                items: [{ owner: 'test-plugin', key: 'zero', byteSize: 1 }],
            }),
            readPluginStorage: async () => ({ revision: 4, value: 0 }),
            release,
        } as any)

        await expect(storage.forOwner(OWNER).snapshot()).resolves.toEqual({ zero: 0 })
        expect(release).toHaveBeenCalledTimes(2)
    })

    it('preserves an own proto key and catalog order in a pinned snapshot', async () => {
        const { storage, store } = harness({}, 100)
        vi.mocked(store.acquireRevision).mockResolvedValueOnce({
            revision: 4,
            queryPluginStorage: async () => ({
                revision: 4,
                items: [
                    { owner: 'test-plugin', key: 'zeta', byteSize: 1 },
                    { owner: 'test-plugin', key: '0', byteSize: 1 },
                    { owner: 'test-plugin', key: '__proto__', byteSize: 1 },
                    { owner: 'test-plugin', key: 'alpha', byteSize: 1 },
                ],
            }),
            readPluginStorage: async (_owner: string, key: string) => ({
                revision: 4,
                value: key === '__proto__' ? false : key === '0' ? 0 : '',
            }),
            release: vi.fn(async () => undefined),
        } as any)

        const snapshot = await storage.forOwner(OWNER).snapshot()

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
                items: [{ owner: 'test-plugin', key: 'zero', byteSize: 1 }],
            })
            .mockResolvedValueOnce({
                revision: 5,
                items: [{ owner: 'test-plugin', key: 'zero', byteSize: 1 }],
            })
        vi.mocked(store.acquireRevision)
            .mockRejectedValueOnce(new RevisionConflictError(4, 5))

        await expect(storage.forOwner(OWNER).snapshot()).resolves.toEqual({ zero: 0 })
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

        await expect(storage.forOwner(OWNER).snapshot()).rejects.toBeInstanceOf(RevisionConflictError)
        expect(store.acquireRevision).toHaveBeenCalledTimes(3)
    })
})
