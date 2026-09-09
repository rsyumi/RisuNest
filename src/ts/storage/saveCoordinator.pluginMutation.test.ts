import { describe, expect, it, vi } from 'vitest'
import {
    SaveCoordinator,
    captureRoot,
    deferred,
    makeDatabase,
    makeStore,
} from './saveCoordinator.testSupport'
import type { PluginStorageMutation } from './persistentDataStore'
import { createProductionStateAdapter } from './persistentDataRuntime.svelte'
import * as persistentRuntime from './persistentDataRuntime.svelte'
import { getDatabase, setDatabaseLite } from './database.svelte'
import { registerPluginStorageLifecycle } from '../plugins/pluginStorageStore'
import {
    pluginStorageStore as productionPluginStorageStore,
    pluginCompatibility,
    getV2PluginAPIs,
} from '../plugins/plugins.svelte'
import { createCatalogPresetWorkingSet } from './workingSetCatalog'
import {
    applyPluginStorageMutations,
    orderPluginStorageKeys,
    PluginStorageBaseline,
    rebaseConcurrentPluginStorage,
} from './saveCoordinatorHelpers'
import {
    capturePluginMutationScope,
    rebasePluginMutationPublication,
} from './pluginMutationPublication'
import { flushSync } from 'svelte'
import { effect_root, render_effect } from 'svelte/internal/client'

vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))

function apply(
    storage: Record<string, unknown>,
    mutations: readonly PluginStorageMutation[],
) {
    for (const mutation of mutations) {
        if (mutation.type === 'clear') {
            for (const key of Object.keys(storage)) delete storage[key]
        } else if (mutation.type === 'delete') delete storage[mutation.key]
        else
            Object.defineProperty(storage, mutation.key, {
                value: structuredClone(mutation.value),
                writable: true,
                configurable: true,
                enumerable: true,
            })
    }
}

describe('key-scoped plugin storage publication', () => {
    it('does not repeatedly encode or parse a large unchanged string during explicit small writes', async () => {
        const payload = 'x'.repeat(2 * 1_048_576)
        const storage = { payload, counter: 0, nested: { count: 0 } }
        const database = makeDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => storage,
            captureSelectedCharacter: () => null,
            replaceDatabase: () => undefined,
            publishPluginStorageMutations: (mutations) => apply(storage, mutations),
        })
        coordinator.initialize(1)
        const stringify = vi.spyOn(JSON, 'stringify')
        const parse = vi.spyOn(JSON, 'parse')
        try {
            for (let counter = 1; counter <= 10; counter++) {
                await coordinator.mutatePersistentPluginStorage('small-write', [
                    { type: 'set', key: 'counter', value: counter },
                ])
            }
            await coordinator.flushPendingData('unchanged')
            expect(commit).toHaveBeenCalledTimes(10)
            const largeEncodings = stringify.mock.calls.filter(
                ([value]) =>
                    value === payload ||
                    (value && typeof value === 'object' && value.payload === payload),
            ).length
            const largeParses = parse.mock.calls.filter(
                ([value]) => typeof value === 'string' && value.length >= payload.length,
            ).length
            expect(largeEncodings).toBe(0)
            expect(largeParses).toBe(0)

            // A same-turn raw object edit still participates in the next flush.
            storage.nested.count = 1
            await coordinator.flushPendingData('raw-nested-write')
            expect(commit).toHaveBeenCalledTimes(11)
            expect(commit.mock.calls[10][0].pluginStorage).toEqual([
                { type: 'set', key: 'nested', value: { count: 1 } },
            ])
        } finally {
            stringify.mockRestore()
            parse.mockRestore()
        }
    })

    it('reads retained live storage through deferred persistence and release until eviction is allowed', async () => {
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: { entry: { count: 1 }, removed: 'old' },
        } as any)
        pluginCompatibility.initialize('maximum-compatibility')
        productionPluginStorageStore.preloadCompatibilityValues({
            entry: { count: 1 },
            removed: 'old',
        })
        const persistence = deferred<void>()
        const release = deferred<boolean>()
        const persistSpy = vi
            .spyOn(persistentRuntime, 'replacePersistentDatabase')
            .mockImplementation(() => persistence.promise)
        const releaseSpy = vi
            .spyOn(persistentRuntime, 'releaseInactiveWorkingSet')
            .mockImplementation(() => release.promise)
        const transition = pluginCompatibility.transition('scalable-v3')
        try {
            await vi.waitFor(() => expect(persistSpy).toHaveBeenCalledOnce())
            expect(pluginCompatibility.profile).toBe('scalable-v3')
            expect(pluginCompatibility.allowsEviction).toBe(false)
            const live = getDatabase().pluginCustomStorage
            live.entry.count = 2
            delete live.removed
            live.duringPersistence = true
            await expect(
                productionPluginStorageStore.getItem('entry'),
            ).resolves.toEqual({ count: 2 })
            await expect(
                productionPluginStorageStore.getItem('removed'),
            ).resolves.toBeNull()
            await expect(productionPluginStorageStore.keys()).resolves.toEqual([
                'entry',
                'duringPersistence',
            ])

            persistence.resolve()
            await vi.waitFor(() => expect(releaseSpy).toHaveBeenCalledOnce())
            expect(pluginCompatibility.allowsEviction).toBe(false)
            live.entry.count = 3
            live.duringRelease = true
            await expect(
                productionPluginStorageStore.getItem('entry'),
            ).resolves.toEqual({ count: 3 })
            await expect(productionPluginStorageStore.keys()).resolves.toEqual([
                'entry',
                'duringPersistence',
                'duringRelease',
            ])
            await expect(productionPluginStorageStore.key(2)).resolves.toBe(
                'duringRelease',
            )
            await expect(productionPluginStorageStore.length()).resolves.toBe(3)

            // Stand in for the committed authority installed by a successful release.
            productionPluginStorageStore.synchronizeCompatibilityStorage({
                durableOnly: 'released',
            })
            release.resolve(true)
            await transition
            expect(pluginCompatibility.allowsEviction).toBe(true)
            await expect(
                productionPluginStorageStore.getItem('durableOnly'),
            ).resolves.toBe('released')
            await expect(productionPluginStorageStore.keys()).resolves.toEqual([
                'durableOnly',
            ])
        } finally {
            persistence.resolve()
            release.resolve(true)
            await transition.catch(() => undefined)
            persistSpy.mockRestore()
            releaseSpy.mockRestore()
            pluginCompatibility.initialize('scalable-v3')
            productionPluginStorageStore.invalidate()
            productionPluginStorageStore.setEvictionAllowed(true)
        }
    })

    it.each(['scalable', 'catalog'] as const)(
        'keeps %s V3 reads on the existing revisioned cache',
        async (mode) => {
            setDatabaseLite({
                characters: [],
                plugins: [],
                botPresets:
                    mode === 'catalog'
                        ? createCatalogPresetWorkingSet(
                              { revision: 1, items: [] },
                              null,
                          )
                        : [],
                pluginCustomStorage: { liveOnly: 'not-authoritative' },
            } as any)
            pluginCompatibility.initialize(
                mode === 'scalable' ? 'scalable-v3' : 'maximum-compatibility',
            )
            productionPluginStorageStore.preloadCompatibilityValues({
                durableOnly: 'saved',
            })
            try {
                await expect(
                    productionPluginStorageStore.getItem('durableOnly'),
                ).resolves.toBe('saved')
                await expect(
                    productionPluginStorageStore.getItem('liveOnly'),
                ).resolves.toBeNull()
                await expect(
                    productionPluginStorageStore.keys(),
                ).resolves.toEqual(['durableOnly'])
                await expect(productionPluginStorageStore.key(0)).resolves.toBe(
                    'durableOnly',
                )
                await expect(
                    productionPluginStorageStore.length(),
                ).resolves.toBe(1)
            } finally {
                pluginCompatibility.initialize('scalable-v3')
                productionPluginStorageStore.invalidate()
                productionPluginStorageStore.setEvictionAllowed(true)
            }
        },
    )

    it('detaches a live V3 key without reading unrelated values and retains clone errors', async () => {
        const unrelated = vi.fn(() => ({ large: 'synthetic' }))
        const storage = {
            requested: { count: 1 },
            unsupported: Symbol('unsupported'),
        }
        Object.defineProperty(storage, 'unrelated', {
            enumerable: true,
            configurable: true,
            get: unrelated,
        })
        setDatabaseLite({
            characters: [],
            plugins: [],
            botPresets: [],
            pluginCustomStorage: storage,
        } as any)
        pluginCompatibility.initialize('maximum-compatibility')
        unrelated.mockClear()
        try {
            const value = (await productionPluginStorageStore.getItem(
                'requested',
            )) as { count: number }
            value.count = 2
            await expect(
                productionPluginStorageStore.getItem('requested'),
            ).resolves.toEqual({ count: 1 })
            await expect(
                productionPluginStorageStore.getItem('missing'),
            ).resolves.toBeNull()
            expect(unrelated).not.toHaveBeenCalled()
            await expect(
                productionPluginStorageStore.getItem('unsupported'),
            ).rejects.toMatchObject({ name: 'DataCloneError' })
        } finally {
            pluginCompatibility.initialize('scalable-v3')
            productionPluginStorageStore.invalidate()
        }
    })

    it('leaves revision, baseline and working state unchanged when a key commit fails', async () => {
        const storage = { target: { before: true }, unrelated: 1 }
        const commit = vi
            .fn()
            .mockRejectedValueOnce(new Error('synthetic commit failure'))
            .mockImplementation(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const publish = vi.fn((mutations) => apply(storage, mutations))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => ({}) as any,
            capturePluginStorage: () => storage,
            captureSelectedCharacter: () => null,
            replaceDatabase: () => undefined,
            publishPluginStorageMutations: publish,
        })
        coordinator.initialize(1)
        await expect(
            coordinator.mutatePersistentPluginStorage('failed', [
                { type: 'set', key: 'target', value: { after: true } },
            ]),
        ).rejects.toThrow('synthetic commit failure')
        expect(coordinator.revision).toBe(1)
        expect(storage.target).toEqual({ before: true })
        expect(publish).not.toHaveBeenCalled()
        await coordinator.flushPendingData('unchanged-after-failure')
        expect(commit).toHaveBeenCalledOnce()
        await coordinator.mutatePersistentPluginStorage('retry', [
            { type: 'delete', key: 'target' },
        ])
        expect(coordinator.revision).toBe(2)
        expect(Object.keys(storage)).toEqual(['unrelated'])
        await coordinator.flushPendingData('retry-is-baselined')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it.each(['target', '__proto__'])(
        'keeps published %s values subscribed to later direct nested edits',
        (key) => {
            setDatabaseLite({
                characters: [],
                plugins: [],
                botPresets: [],
                pluginCustomStorage: {},
            } as any)
            const adapter = createProductionStateAdapter()
            const observed: number[] = []
            const dispose = effect_root(() => {
                render_effect(() => {
                    const storage = adapter.capturePluginStorage!()!
                    for (const entry of Object.keys(storage))
                        observed.push((storage[entry] as any).nested.count)
                })
            })
            try {
                adapter.publishPluginStorageMutations!(
                    [{ type: 'set', key, value: { nested: { count: 1 } } }],
                    [key],
                )
                flushSync()
                const storage = adapter.capturePluginStorage!()!
                ;(storage[key] as any).nested.count = 2
                flushSync()
                expect(observed).toEqual([1, 2])
            } finally {
                dispose()
            }
        },
    )

    it('matches full publication for overlapping key edits and ordering changes', () => {
        const original = { first: { count: 1 }, middle: 2, last: 3 }
        const changes: PluginStorageMutation[][] = [
            [{ type: 'set', key: 'new', value: 4 }],
            [
                { type: 'delete', key: 'first' },
                { type: 'set', key: 'first', value: { count: 2 } },
            ],
            [{ type: 'delete', key: 'first' }],
            [{ type: 'delete', key: 'last' }],
            [{ type: 'set', key: 'first', value: { count: 3 } }],
            [
                { type: 'delete', key: 'middle' },
                { type: 'set', key: 'middle', value: 2 },
                { type: 'set', key: 'other', value: 5 },
            ],
            [{ type: 'set', key: '__proto__', value: false }],
            [{ type: 'set', key: '1', value: false }],
            [{ type: 'clear' }],
            [
                { type: 'clear' },
                { type: 'set', key: 'first', value: { count: 9 } },
            ],
        ]
        for (const committed of changes)
            for (const concurrent of changes) {
                const live = applyPluginStorageMutations(original, concurrent)
                const expected = rebaseConcurrentPluginStorage(
                    original,
                    live,
                    applyPluginStorageMutations(original, committed),
                )
                const publication = rebasePluginMutationPublication(
                    committed,
                    capturePluginMutationScope(original, committed),
                    capturePluginMutationScope(live, committed),
                )
                const actual = orderPluginStorageKeys(
                    applyPluginStorageMutations(live, publication.mutations),
                    publication.keys,
                )
                expect(
                    actual,
                    JSON.stringify({ committed, concurrent }),
                ).toEqual(expected)
                expect(
                    Object.keys(actual),
                    JSON.stringify({ committed, concurrent }),
                ).toEqual(Object.keys(expected))
            }
    })

    it('keeps the keyed baseline identical to ordered JSON across mutations', () => {
        let storage: Record<string, unknown> = {
            first: { z: 1, a: 2 },
            second: false,
        }
        const baseline = new PluginStorageBaseline(JSON.stringify(storage))
        for (const mutations of [
            [
                { type: 'delete', key: 'first' },
                { type: 'set', key: 'first', value: { a: 3, z: 1 } },
            ],
            [
                { type: 'set', key: '__proto__', value: false },
                { type: 'set', key: '1', value: 'integer' },
            ],
            [{ type: 'clear' }, { type: 'set', key: 'after', value: null }],
        ] as PluginStorageMutation[][]) {
            baseline.apply(mutations)
            storage = applyPluginStorageMutations(storage, mutations)
            expect(baseline.json).toBe(JSON.stringify(storage))
        }
    })
    it.each(['insert', 'reinsert'] as const)(
        'keeps concurrent key order and V3 cache aligned during %s',
        async (operation) => {
            setDatabaseLite({
                characters: [],
                plugins: [],
                botPresets: [],
                pluginCustomStorage: {
                    first: { keepIdentity: true, count: 1 },
                    removed: 'before',
                    ...(operation === 'reinsert' ? { target: 1 } : {}),
                },
            } as any)
            const adapter = createProductionStateAdapter()
            const storage = adapter.capturePluginStorage!()!
            const identity = storage.first
            const pending = deferred<{ revision: number }>()
            const commit = vi
                .fn()
                .mockImplementationOnce(() => pending.promise)
                .mockImplementation(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const store = makeStore(commit)
            const cache = productionPluginStorageStore
            pluginCompatibility.initialize('maximum-compatibility')
            cache.preloadCompatibilityValues(storage)
            const unregister = registerPluginStorageLifecycle(cache)
            try {
                const coordinator = new SaveCoordinator({
                    store,
                    captureRoot: () => ({}) as any,
                    capturePluginStorage: adapter.capturePluginStorage,
                    publishPluginStorageMutations:
                        adapter.publishPluginStorageMutations,
                    captureSelectedCharacter: () => null,
                    replaceDatabase: () => undefined,
                })
                coordinator.initialize(1)
                const mutations: PluginStorageMutation[] =
                    operation === 'reinsert'
                        ? [
                              { type: 'delete', key: 'target' },
                              { type: 'set', key: 'target', value: 2 },
                          ]
                        : [{ type: 'set', key: 'target', value: 2 }]
                const writing = coordinator.mutatePersistentPluginStorage(
                    'ordered-overlap',
                    mutations,
                )
                await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
                storage.concurrent = { later: true }
                ;(storage.first as any).count = 2
                delete storage.removed
                const concurrentIdentity = storage.concurrent
                pending.resolve({ revision: 2 })
                await writing
                const published = adapter.capturePluginStorage!()!
                expect(Object.keys(published)).toEqual([
                    'first',
                    'target',
                    'concurrent',
                ])
                expect(published.first).toBe(identity)
                expect(published.concurrent).toBe(concurrentIdentity)
                await expect(cache.keys()).resolves.toEqual(
                    Object.keys(published),
                )
                await expect(cache.getItem('target')).resolves.toBe(2)
                await expect(cache.getItem('first')).resolves.toEqual({
                    keepIdentity: true,
                    count: 2,
                })
                await expect(cache.getItem('concurrent')).resolves.toEqual({
                    later: true,
                })
                await expect(cache.getItem('removed')).resolves.toBeNull()
                expect(
                    getV2PluginAPIs().pluginStorage.getItem('removed'),
                ).toBeNull()
                await expect(cache.key(1)).resolves.toBe('target')
                await expect(cache.key(-1)).resolves.toBeNull()
                await expect(cache.key(3)).resolves.toBeNull()
                await expect(cache.length()).resolves.toBe(3)
                const detached = (await cache.getItem('first')) as {
                    count: number
                }
                detached.count = 99
                expect((published.first as any).count).toBe(2)
                await coordinator.flushPendingData('unmarked-concurrent-write')
                expect(commit).toHaveBeenCalledTimes(2)
            } finally {
                unregister()
                pluginCompatibility.initialize('scalable-v3')
                cache.invalidate()
                cache.setEvictionAllowed(true)
            }
        },
    )

    it('does not parse or serialize unrelated baseline values after the precommit flush', async () => {
        const payload = 'large-unrelated-baseline'.repeat(100)
        const storage = { untouched: { payload }, target: 1 }
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => ({}) as any,
            capturePluginStorage: () => storage,
            captureSelectedCharacter: () => null,
            replaceDatabase: () => undefined,
            publishPluginStorageMutations: (mutations) =>
                apply(storage, mutations),
        })
        coordinator.initialize(1)
        // The unchanged flush must still detect arbitrary V2 writes. Observe only
        // the work after that boundary, including baseline preparation/publication.
        const flush = (coordinator as any).flushIterations.bind(coordinator)
        let parses: ReturnType<typeof vi.spyOn> | undefined
        let strings: ReturnType<typeof vi.spyOn> | undefined
        ;(coordinator as any).flushIterations = async (...args: unknown[]) => {
            await flush(...args)
            parses = vi.spyOn(JSON, 'parse')
            strings = vi.spyOn(JSON, 'stringify')
        }
        try {
            await coordinator.mutatePersistentPluginStorage('small', [
                { type: 'set', key: 'target', value: 2 },
            ])
            expect(
                parses!.mock.calls.some(([value]) =>
                    String(value).includes(payload),
                ),
            ).toBe(false)
            expect(
                strings!.mock.calls.some(
                    ([value]) =>
                        value === payload ||
                        (value &&
                            typeof value === 'object' &&
                            (value as any).untouched?.payload === payload),
                ),
            ).toBe(false)
        } finally {
            parses?.mockRestore()
            strings?.mockRestore()
        }
    })
    it('does not repeat an expensive capture in a synchronous unchanged flush', async () => {
        const root = { username: 'Synthetic' } as any
        const captureRoot = vi.fn(() => root)
        const commit = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot,
            capturePluginStorage: () => ({ large: 'synthetic payload' }),
            captureSelectedCharacter: () => null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        captureRoot.mockClear()
        await coordinator.flushPendingData('unchanged')
        expect(commit).not.toHaveBeenCalled()
        expect(captureRoot).toHaveBeenCalledOnce()
    })

    it('does not traverse unrelated live values after a key commit starts', async () => {
        let committing = false
        const unrelated = { payload: 'large synthetic value' }
        const storage: Record<string, unknown> = { target: 'before' }
        Object.defineProperty(storage, 'unrelated', {
            enumerable: true,
            configurable: true,
            get() {
                if (committing)
                    throw new Error(
                        'Unrelated live value was traversed during key commit',
                    )
                return unrelated
            },
        })
        const commit = vi.fn(async ({ expectedRevision }) => {
            committing = true
            return { revision: expectedRevision + 1 }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => ({}) as any,
            capturePluginStorage: () => storage,
            captureSelectedCharacter: () => null,
            replaceDatabase: () => undefined,
            publishPluginStorageMutations: (mutations) =>
                apply(storage, mutations),
        })
        coordinator.initialize(1)

        await coordinator.mutatePersistentPluginStorage('tiny-key-write', [
            { type: 'set', key: 'target', value: 'after' },
        ])
        expect(storage.target).toBe('after')
        committing = false
        expect(storage.unrelated).toBe(unrelated)
        await coordinator.flushPendingData('already-persisted')
        expect(commit).toHaveBeenCalledOnce()
    })

    it.each(['set', 'clear'] as const)(
        'preserves nested and newly added concurrent values during %s',
        async (operation) => {
            const storage: Record<string, unknown> = {
                shared: { first: 1, later: 0 },
                untouched: 'keep',
            }
            const pending = deferred<{ revision: number }>()
            const commit = vi
                .fn()
                .mockImplementationOnce(() => pending.promise)
                .mockImplementation(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => ({}) as any,
                capturePluginStorage: () => storage,
                captureSelectedCharacter: () => null,
                replaceDatabase: () => undefined,
                publishPluginStorageMutations: (mutations) =>
                    apply(storage, mutations),
            })
            coordinator.initialize(1)
            const mutations: PluginStorageMutation[] =
                operation === 'clear'
                    ? [{ type: 'clear' }]
                    : [
                          {
                              type: 'set',
                              key: 'shared',
                              value: { first: 2, later: 0 },
                          },
                      ]
            const writing = coordinator.mutatePersistentPluginStorage(
                'overlap',
                mutations,
            )
            await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
            ;(storage.shared as any).later = 3
            storage.new = 'arrived during commit'
            coordinator.markPersistentDataDirty(1)
            pending.resolve({ revision: 2 })
            await writing
            expect(storage.shared).toEqual({
                first: operation === 'clear' ? 1 : 2,
                later: 3,
            })
            expect(storage.new).toBe('arrived during commit')
            expect(Object.hasOwn(storage, 'untouched')).toBe(
                operation !== 'clear',
            )
            await coordinator.flushPendingData('persist-later-values')
            expect(commit).toHaveBeenCalledTimes(2)
        },
    )

    it('preserves delete/reinsert ordering and safe prototype-named keys', async () => {
        const storage: Record<string, unknown> = { first: 1, second: 2 }
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => ({}) as any,
            capturePluginStorage: () => storage,
            captureSelectedCharacter: () => null,
            replaceDatabase: () => undefined,
            publishPluginStorageMutations: (mutations) =>
                apply(storage, mutations),
        })
        coordinator.initialize(1)
        await coordinator.mutatePersistentPluginStorage('ordered', [
            { type: 'delete', key: 'first' },
            { type: 'set', key: 'first', value: 1 },
            { type: 'set', key: '__proto__', value: 0 },
        ])
        expect(Object.keys(storage)).toEqual(['second', 'first', '__proto__'])
        expect(storage.__proto__).toBe(0)
        expect(Object.getPrototypeOf(storage)).toBe(Object.prototype)
        await coordinator.flushPendingData('already-persisted')
        expect(commit).toHaveBeenCalledOnce()
    })
})
