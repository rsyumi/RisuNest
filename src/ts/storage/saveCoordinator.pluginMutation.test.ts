import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
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
import {
    createPluginStorageStore,
    registerPluginStorageLifecycle,
} from '../plugins/pluginStorageStore'
import {
    pluginStorageStore as productionPluginStorageStore,
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

/** A durable authority the V3 store can read, standing apart from DBState. */
function durablePluginStore(entries: Record<string, unknown>) {
    const values = { ...entries }
    const backing = {
        open: async () => undefined,
        queryPluginStorage: async () => ({
            revision: 1,
            items: Object.keys(values).map((key) => ({
                owner: UNOWNED_PLUGIN_OWNER,
                key,
                byteSize: JSON.stringify(values[key]).length,
            })),
        }),
        readPluginStorage: async (_owner: string, key: string) =>
            Object.hasOwn(values, key)
                ? { revision: 1, value: JSON.parse(JSON.stringify(values[key])) }
                : null,
    } as never
    return createPluginStorageStore({
        store: backing,
        getStorageAuthorityEpoch: () => 0,
        assertPersistentMutationAllowed: vi.fn(),
        mutate: async (mutations) => {
            for (const mutation of mutations) {
                if (mutation.type === 'clear') {
                    for (const key of Object.keys(values)) delete values[key]
                } else if (mutation.type === 'delete') delete values[mutation.key]
                else values[mutation.key] = mutation.value
            }
        },
    })
}

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
                    { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'counter', value: counter },
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
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'nested', value: { count: 1 } },
            ])
        } finally {
            stringify.mockRestore()
            parse.mockRestore()
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
            const durable = durablePluginStore({ durableOnly: 'saved' })
            const unregisterDurable = registerPluginStorageLifecycle(durable)
            try {
                await expect(
                    durable.forOwner(UNOWNED_PLUGIN_OWNER).getItem('durableOnly'),
                ).resolves.toBe('saved')
                await expect(
                    durable.forOwner(UNOWNED_PLUGIN_OWNER).getItem('liveOnly'),
                ).resolves.toBeNull()
                await expect(
                    durable.forOwner(UNOWNED_PLUGIN_OWNER).keys(),
                ).resolves.toEqual(['durableOnly'])
                await expect(durable.forOwner(UNOWNED_PLUGIN_OWNER).key(0)).resolves.toBe(
                    'durableOnly',
                )
                await expect(
                    durable.forOwner(UNOWNED_PLUGIN_OWNER).length(),
                ).resolves.toBe(1)
            } finally {
                unregisterDurable()
            }
        },
    )

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
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'target', value: { after: true } },
            ]),
        ).rejects.toThrow('synthetic commit failure')
        expect(coordinator.revision).toBe(1)
        expect(storage.target).toEqual({ before: true })
        expect(publish).not.toHaveBeenCalled()
        await coordinator.flushPendingData('unchanged-after-failure')
        expect(commit).toHaveBeenCalledOnce()
        await coordinator.mutatePersistentPluginStorage('retry', [
            { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'target' },
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
                    [{ type: 'set', owner: 'test-plugin', key, value: { nested: { count: 1 } } }],
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
            [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'new', value: 4 }],
            [
                { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'first' },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'first', value: { count: 2 } },
            ],
            [{ type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'first' }],
            [{ type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'last' }],
            [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'first', value: { count: 3 } }],
            [
                { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'middle' },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'middle', value: 2 },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'other', value: 5 },
            ],
            [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '__proto__', value: false }],
            [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '1', value: false }],
            [{ type: 'clear', owner: UNOWNED_PLUGIN_OWNER }],
            [
                { type: 'clear', owner: UNOWNED_PLUGIN_OWNER },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'first', value: { count: 9 } },
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
                { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'first' },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'first', value: { a: 3, z: 1 } },
            ],
            [
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '__proto__', value: false },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '1', value: 'integer' },
            ],
            [{ type: 'clear', owner: UNOWNED_PLUGIN_OWNER }, { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'after', value: null }],
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
            const cache = durablePluginStore(storage as Record<string, unknown>)
            await cache.forOwner(UNOWNED_PLUGIN_OWNER).keys()
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
                              { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'target' },
                              { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'target', value: 2 },
                          ]
                        : [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'target', value: 2 }]
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
                await expect(cache.forOwner(UNOWNED_PLUGIN_OWNER).getItem('target')).resolves.toBe(2)
                const cachedKeys = await cache.forOwner(UNOWNED_PLUGIN_OWNER).keys()
                // The cache follows published order for the keys it observed and
                // does not invent one for an unobserved concurrent write.
                const publishedKeys = Object.keys(published)
                expect(cachedKeys.filter((key) => publishedKeys.includes(key))).toEqual(
                    publishedKeys.filter((key) => cachedKeys.includes(key)),
                )
                expect(cachedKeys).toContain('target')
                expect(cachedKeys).not.toContain('concurrent')
                await expect(cache.forOwner(UNOWNED_PLUGIN_OWNER).key(-1)).resolves.toBeNull()
                await expect(cache.forOwner(UNOWNED_PLUGIN_OWNER).key(cachedKeys.length)).resolves.toBeNull()
                await expect(cache.forOwner(UNOWNED_PLUGIN_OWNER).length()).resolves.toBe(cachedKeys.length)
                const detached = (await cache.forOwner(UNOWNED_PLUGIN_OWNER).getItem('first')) as {
                    count: number
                }
                detached.count = 99
                expect((published.first as any).count).toBe(2)
                await coordinator.flushPendingData('unmarked-concurrent-write')
                expect(commit).toHaveBeenCalledTimes(2)
            } finally {
                unregister()
                cache.invalidate()
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
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'target', value: 2 },
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
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'target', value: 'after' },
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
                    ? [{ type: 'clear', owner: UNOWNED_PLUGIN_OWNER }]
                    : [
                          {
                              type: 'set',
                              owner: UNOWNED_PLUGIN_OWNER,
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
            { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'first' },
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'first', value: 1 },
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '__proto__', value: 0 },
        ])
        expect(Object.keys(storage)).toEqual(['second', 'first', '__proto__'])
        expect(storage.__proto__).toBe(0)
        expect(Object.getPrototypeOf(storage)).toBe(Object.prototype)
        await coordinator.flushPendingData('already-persisted')
        expect(commit).toHaveBeenCalledOnce()
    })
})
