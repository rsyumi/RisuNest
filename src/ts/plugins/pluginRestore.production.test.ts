import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
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
    pluginStorageStore,
} from './plugins.svelte'

afterEach(() => {
    configurePersistentDataRuntime({ projectWorkingSet: undefined })
    pluginStorageStore.invalidate()
    workingSetResidency.clear()
    vi.restoreAllMocks()
})

async function restoreFixture() {
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
    configurePersistentDataRuntime({
        projectWorkingSet(
            database,
            selectedId,
            conversationId,
            activeIds,
            force,
        ) {
            return force === false
                ? database
                : projectCompleteScalableWorkingSet(
                      database,
                      selectedId,
                      2,
                      activeIds,
                      conversationId,
                  )
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
    it('invalidates old cached values and reads restored v3 storage without materializing the database', async () => {
        const { store, restored, materialize } = await restoreFixture()
        expect(isCatalogCharacterStub(getDatabase().characters[0])).toBe(true)
        let observed: unknown
        vi.mocked(loadV3Plugins).mockImplementationOnce(async () => {
            observed = await pluginStorageStore.forOwner(UNOWNED_PLUGIN_OWNER).getItem('pm_store')
        })
        await loadPluginsAfterAuthoritativeRestore()
        expect(observed).toEqual({
            version: 5,
            models: [{ id: 'restored' }],
            keys: [],
        })
        expect((await store.readPluginStorage(UNOWNED_PLUGIN_OWNER, 'pm_store'))?.value).toEqual(
            restored.pluginCustomStorage.pm_store,
        )
        expect(materialize).not.toHaveBeenCalled()
    })
})
