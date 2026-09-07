import { afterEach, describe, expect, it, vi } from 'vitest'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'

const authority = vi.hoisted(() => ({ store: undefined as any }))
vi.mock('../storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: () => authority.store,
}))
vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))
vi.mock('./apiV3/v3.svelte', () => ({
    loadV3Plugins: vi.fn(async () => undefined),
}))

import { loadV3Plugins } from './apiV3/v3.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import { createPersistentDataRuntime } from '../storage/persistentDataRuntime'
import {
    configurePersistentDataRuntime,
    createProductionStateAdapter,
} from '../storage/persistentDataRuntime.svelte'
import {
    getDatabase,
    setDatabaseLite,
    type Database,
} from '../storage/database.svelte'
import {
    isCatalogCharacterStub,
    projectCompleteScalableWorkingSet,
} from '../storage/workingSetCatalog'
import { workingSetResidency } from '../storage/workingSetResidency'
import { selectedCharID } from '../stores.svelte'
import {
    loadPluginsAfterAuthoritativeRestore,
    pluginCompatibility,
    pluginStorageStore,
} from './plugins.svelte'
import {
    shouldProjectScalableWorkingSet,
    type PluginCompatibilityProfile,
} from './pluginCompatibility'

afterEach(() => {
    configurePersistentDataRuntime({ projectWorkingSet: undefined })
    pluginCompatibility.initialize('scalable-v3')
    pluginStorageStore.invalidate()
    workingSetResidency.clear()
    vi.restoreAllMocks()
})

async function restoreFixture(
    previous: PluginCompatibilityProfile,
    maximum: boolean,
) {
    const initial = {
        characters: [],
        botPresets: [],
        botPresetsId: 0,
        plugins: [],
        pluginCustomStorage: {
            pm_store: { version: 5, models: [{ id: 'previous' }], keys: [] },
        },
    } as unknown as Database
    const restored = {
        ...initial,
        plugins: [
            ...(maximum
                ? [
                      {
                          name: 'synthetic-v2',
                          version: '2.1',
                          enabled: true,
                          script: '',
                      },
                  ]
                : []),
            { name: 'synthetic-v3', version: '3.0', enabled: true, script: '' },
        ],
        pluginCustomStorage: {
            pm_store: { version: 5, models: [{ id: 'restored' }], keys: [] },
        },
        characters: [
            {
                type: 'character',
                chaId: 'synthetic',
                name: 'Synthetic',
                chats: [],
                desc: 'restored detail',
            },
        ],
    } as unknown as Database
    const store = new IndexedDbPersistentDataStore(
        'synthetic-restore',
        new IDBFactory(),
        IDBKeyRange,
    )
    authority.store = store
    await store.open()
    await store.replaceFromDatabase(initial, 0)
    setDatabaseLite(initial)
    selectedCharID.set(-1)
    pluginCompatibility.initialize(previous)
    pluginStorageStore.preloadCompatibilityValues(initial.pluginCustomStorage)
    configurePersistentDataRuntime({
        projectWorkingSet(
            database,
            selectedId,
            conversationId,
            activeIds,
            force,
        ) {
            return shouldProjectScalableWorkingSet(pluginCompatibility, force)
                ? projectCompleteScalableWorkingSet(
                      database,
                      selectedId,
                      2,
                      activeIds,
                      conversationId,
                  )
                : database
        },
    })
    const runtime = createPersistentDataRuntime({
        store,
        state: createProductionStateAdapter(),
        prepareDatabase: async (d) => d,
    })
    await runtime.initializeActiveWorkingSet(initial)
    const materialize = vi.spyOn(store, 'materializeDatabase')
    const { revision } = await store.replaceFromDatabase(restored, 1)
    await runtime.refreshActiveWorkingSetFromStore(revision)
    return { store, restored, materialize }
}

describe('authoritative restore plugin initialization', () => {
    it.each<PluginCompatibilityProfile>([
        'scalable-v3',
        'maximum-compatibility',
    ])(
        'installs restored compatibility data before loading plugins from %s',
        async (previous) => {
            const { store, restored } = await restoreFixture(previous, true)
            expect(isCatalogCharacterStub(getDatabase().characters[0])).toBe(
                false,
            )
            expect(getDatabase().characters[0]).toMatchObject({
                desc: 'restored detail',
            })
            let observed: unknown
            vi.mocked(loadV3Plugins).mockImplementationOnce(async () => {
                observed = await pluginStorageStore.getItem('pm_store')
            })
            await loadPluginsAfterAuthoritativeRestore()
            expect(pluginCompatibility.profile).toBe('maximum-compatibility')
            expect(observed).toEqual({
                version: 5,
                models: [{ id: 'restored' }],
                keys: [],
            })
            expect((await store.readPluginStorage('pm_store'))?.value).toEqual(
                restored.pluginCustomStorage.pm_store,
            )
        },
    )

    it('invalidates old cached values and reads restored v3 storage without materializing the database', async () => {
        const { materialize } = await restoreFixture(
            'maximum-compatibility',
            false,
        )
        expect(isCatalogCharacterStub(getDatabase().characters[0])).toBe(true)
        await loadPluginsAfterAuthoritativeRestore()
        expect(pluginCompatibility.profile).toBe('scalable-v3')
        await expect(pluginStorageStore.getItem('pm_store')).resolves.toEqual({
            version: 5,
            models: [{ id: 'restored' }],
            keys: [],
        })
        expect(materialize).not.toHaveBeenCalled()
    })
})
