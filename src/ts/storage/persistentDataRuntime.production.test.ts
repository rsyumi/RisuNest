import { assetGalleryItems, deleteGalleryAssets, replaceGalleryAsset, prepareGalleryModuleMutation, readPinnedAssetOwner } from '../gallery/assetGallery'
import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import { flushSync } from 'svelte'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
const displaySettings = vi.hoisted(() => ({ apply: vi.fn(async () => undefined) }))
vi.mock('../gui/receivedDisplaySettings', () => ({ applyReceivedDisplaySettings: displaySettings.apply }))
const pluginRuntime = vi.hoisted(() => ({ load: vi.fn(async () => undefined) }))
vi.mock('../plugins/plugins.svelte', async (importOriginal) => ({ ...await importOriginal<typeof import('../plugins/plugins.svelte')>(), requestPluginReloadAfterSync: pluginRuntime.load }))
import { createBrowserPluginDeviceBackend, getPluginDeviceKeyspace } from '../plugins/pluginDeviceKeyspace'
import { selectedCharID } from '../stores.svelte'
import { doingChat } from '../process/generationState'
import { getRuntimePerformanceBudgets } from '../runtimePerformanceProfile'
import {
    createPluginStorageStore,
    registerPluginStorageLifecycle,
} from '../plugins/pluginStorageStore'
import { getV2PluginAPIs } from '../plugins/plugins.svelte'
import type { Database } from './database.svelte'
import type { PersistentDataStore, PersistentUnitMutation, WorkingSetCommit } from './persistentDataStore'
import { createStoreStub } from './persistentDataStore.testSupport'
import { captureCurrentPreset, fillRootFieldDefaults, getDatabase, getEffectivePresetId, normalizeDatabaseDefaults, setDatabase, setDatabaseLite, setEffectivePresetOverride, setPreset } from './database.svelte'
import { createPresetWorkingSetController } from './presetWorkingSetOperations'
import {
    configurePersistentDataRuntime,
    createProductionStateAdapter,
} from './persistentDataRuntime.svelte'
import {
    createCatalogCharacterStub,
    isCatalogCharacterStub,
    isCatalogPresetWorkingSet,
    projectCompleteScalableWorkingSet,
} from './workingSetCatalog'
import { workingSetResidency } from './workingSetResidency'
import { canonicalJson, SaveCoordinator, type PersistentCharacterAssetOwnerHead } from './saveCoordinator'
import { capturePersistentRoot, createPersistentDataRuntime } from './persistentDataRuntime'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { observePersistentSaveChanges } from './persistentSaveObserver.svelte'
import { appendCharacterIdToOrder, removeCharacterIdFromOrder } from './characterOrderMutation'
import { clearStreamingConversation, setStreamingConversation } from './streamingConversationRegistry'

afterEach(() => {
    configurePersistentDataRuntime({ projectWorkingSet: undefined })
    workingSetResidency.clear()
    workingSetResidency.setEvictionAllowed(true)
    doingChat.set(false)
})

describe('production persistent working-set publication', () => {
    it('commits a large gallery deletion once and drops its stale owner manifest without changing the greeting', async () => {
        const assets = Array.from({length: 5000}, (_, i) => [`synthetic ${i}`, `assets/${i}.png`, 'png'])
        const initial = {
            username: 'Synthetic', botPresets: [], botPresetsId: 0, plugins: [],
            characters: [{type: 'character', chaId: 'gallery-owner', name: 'Synthetic owner', chatPage: 0, chatFolders: [],
                additionalAssets: assets, prebuiltAssetExclude: ['assets/0.png'],
                chats: [{id: 'gallery-chat', name: 'Synthetic chat', fmIndex: 3, message: []}],
            }],
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore('gallery-bulk-commit', new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async value => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const owner = {kind: 'character' as const, id: 'gallery-owner'}
        const locator = {kind: 'character-additional-assets' as const, characterId: owner.id}
        await runtime.upsertPersistentCompleteCharacter(owner.id, 'prepare-gallery-owner', current => current!, {
            assetOwnerHeads: [{owner: locator, present: true, manifestHash: 'ab'.repeat(32), entryCount: assets.length}],
        })
        const before = await store.readRoot()
        const snapshots = [...assetGalleryItems(owner, getDatabase().characters.find(value => value.chaId === owner.id)!, 'assets')]
        await runtime.mutatePersistentCharacterDetail(owner.id, 'gallery-delete-assets', ({character}) => deleteGalleryAssets(owner, character, snapshots))
        expect((await store.readRoot()).revision).toBe(before.revision + 1)
        const persisted = (await store.readCharacter(owner.id))!.value
        expect(persisted.additionalAssets).toEqual([])
        expect(persisted.prebuiltAssetExclude).toEqual([])
        expect((await store.readConversation(owner.id, 'gallery-chat'))!.value.fmIndex).toBe(3)
        expect(await store.readAssetOwnerHead(locator)).toBeNull()
    })

    it('commits the module gallery tuple with no stale owner manifest', async () => {
        const initial = {
            username: 'Synthetic', botPresets: [], botPresetsId: 0, plugins: [], characters: [],
            modules: [{id: 'gallery-module', name: 'Synthetic module', description: '', assets: [['background', 'assets/old.png', 'png']]}],
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore('gallery-module-commit', new IDBFactory(), IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(initial)
        const locator = {kind: 'root-module-assets' as const, moduleId: 'gallery-module'}
        await store.commit({expectedRevision: imported.revision, root: capturePersistentRoot(initial), assetOwnerHeads: [{owner: locator, present: true, manifestHash: 'ab'.repeat(32), entryCount: 1}]})
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async value => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const owner = {kind: 'module' as const, id: 'gallery-module'}
        const module = getDatabase().modules[0]
        const [snapshot] = assetGalleryItems(owner, module, 'assets')
        await runtime.commitPreparedUnitIntent('gallery-replace-asset', reader => prepareGalleryModuleMutation(reader, owner.id,
            value => replaceGalleryAsset(owner, value, snapshot, 'assets/new.webp', 'webp')))
        expect((await store.readRoot()).value.modules[0].assets).toEqual([['background', 'assets/new.webp', 'webp']])
        expect(await store.readAssetOwnerHead(locator)).toBeNull()
    })

    it.each(['absent', 'empty', 'populated'] as const)('persists a prepared owner head when an existing character has unchanged %s assets', async (shape) => {
        const initial = {
            username: 'Before', botPresets: [], botPresetsId: 0, plugins: [],
            characters: [{ type: 'character', chaId: 'prepared-owner', name: 'Owner', chatPage: 0, chatFolders: [], chats: [],
                ...(shape === 'absent' ? {} : { additionalAssets: shape === 'empty' ? [] : [['synthetic', 'assets/imported', 'bin']] }),
            }],
        } as unknown as Database
        const indexedDB = new IDBFactory()
        const name = `prepared-existing-owner-${shape}`
        const store = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({ store, state: createProductionStateAdapter(), prepareDatabase: async value => value })
        await runtime.initializeActiveWorkingSet(getDatabase())
        const owner: PersistentCharacterAssetOwnerHead['owner'] = { kind: 'character-additional-assets', characterId: 'prepared-owner' }
        const head: PersistentCharacterAssetOwnerHead = shape === 'absent'
            ? { owner, present: false, manifestHash: null, entryCount: 0 }
            : { owner, present: true, manifestHash: 'ab'.repeat(32), entryCount: shape === 'populated' ? 1 : 0 }
        await runtime.upsertPersistentCompleteCharacter('prepared-owner', 'prepared-existing-import', current => current!, { assetOwnerHeads: [head] })
        expect((await store.readAssetOwnerHead(owner))?.value).toEqual(head)
        await runtime.mutatePersistentCharacterDetail('prepared-owner', 'edit-after-import', ({ character }) => { character.name = 'Edited after import' })
        const reopened = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
        await reopened.open()
        expect((await reopened.readCharacter('prepared-owner'))?.value.name).toBe('Edited after import')
        expect((await reopened.readAssetOwnerHead(owner))?.value).toEqual(head)
    })

    it('keeps a failed module gallery mutation out of the working copy and later unrelated saves', async () => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'gallery-failure-preset'
        initial.personas[0].id = 'gallery-failure-persona'
        initial.modules = [{id: 'gallery-failure', name: 'Synthetic module', description: '', assets: [['background', 'assets/old.png', 'png']]}]
        const store = new IndexedDbPersistentDataStore('gallery-module-failure', new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async value => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('gallery-initial-settle')
        const owner = {kind: 'module' as const, id: 'gallery-failure'}
        const [snapshot] = assetGalleryItems(owner, getDatabase().modules[0], 'assets')
        const committed = store.commit.bind(store)
        const failure = vi.spyOn(store, 'commit').mockImplementationOnce(async () => { throw new Error('Synthetic gallery commit failure') })
        await expect(runtime.commitPreparedUnitIntent('gallery-replace-asset', reader => prepareGalleryModuleMutation(reader, owner.id,
            value => replaceGalleryAsset(owner, value, snapshot, 'assets/failed.webp', 'webp')))).rejects.toThrow('Synthetic gallery commit failure')
        expect(getDatabase().modules[0].assets).toEqual([['background', 'assets/old.png', 'png']])
        expect((await store.readRoot()).value.modules[0].assets).toEqual([['background', 'assets/old.png', 'png']])
        failure.mockImplementation(committed)
        getDatabase().loreBookDepth = 17
        await runtime.flushPendingDataLocally('gallery-later-save')
        expect((await store.readRoot()).value.loreBookDepth).toBe(17)
        expect((await store.readRoot()).value.modules[0].assets).toEqual([['background', 'assets/old.png', 'png']])
    })

    it.each(['character', 'module'] as const)('reads %s gallery entries from the export lease despite later changes', async (kind) => {
        const initial = {
            username: 'Synthetic', botPresets: [], botPresetsId: 0, plugins: [],
            characters: [{type: 'character', chaId: 'pinned-owner', name: 'Synthetic', chats: [], chatPage: 0,
                additionalAssets: [['old', 'assets/old.png', 'png']]}],
            modules: [{id: 'pinned-owner', name: 'Synthetic module', description: '', assets: [['old', 'assets/old.png', 'png']]}],
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore(`gallery-lease-${kind}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        const before = await store.replaceFromDatabase(initial)
        const lease = await store.acquireRevision(before.revision)
        try {
            const changed = structuredClone(initial)
            changed.characters[0].additionalAssets = [['new', 'assets/new.webp', 'webp']]
            changed.modules[0].assets = [['new', 'assets/new.webp', 'webp']]
            await store.replaceFromDatabase(changed, before.revision)
            const owner = {kind, id: 'pinned-owner'}
            const pinned = await readPinnedAssetOwner(lease, owner)
            expect([...assetGalleryItems(owner, pinned, 'assets')].map(entry => [entry.name, entry.key])).toEqual([['old', 'assets/old.png']])
        } finally { await lease.release() }
    })

    it('invalidates the actual shared device keyspace after a remote plugin-local change', async () => {
        const owner = `received-device-plugin-${crypto.randomUUID()}`
        const keyspace = getPluginDeviceKeyspace(owner)
        const backend = createBrowserPluginDeviceBackend()
        await backend.write(owner, [{type:'set', space:'string', key:'key', value:'before'}])
        expect(await keyspace.getItem('string', 'key')).toBe('before')
        await backend.write(owner, [{type:'set', space:'string', key:'key', value:'received'}])
        createProductionStateAdapter().onPluginDeviceStorageChanged!(owner)
        expect(await keyspace.getItem('string', 'key')).toBe('received')
        await backend.write(owner, [{type:'clear', space:'string'}])
    })
    it('applies received root changes as display settings', () => {
        createProductionStateAdapter().afterRemoteRootChange!(new Set(['colorSchemeName']))

        expect(displaySettings.apply).toHaveBeenCalledExactlyOnceWith(new Set(['colorSchemeName']))
    })

    it.each([true, false])('adopts a complete added preset without a follow-up publication (catalog=%s)', async (catalog) => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'existing-preset'
        initial.personas[0].id = 'existing-persona'
        const added = {...structuredClone(initial.botPresets[0]), id: 'added-preset', name: 'Added complete preset'}
        const store = new IndexedDbPersistentDataStore(`complete-added-preset-${catalog}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        const {revision} = await store.replaceFromDatabase(initial)
        setDatabaseLite(catalog ? projectCompleteScalableWorkingSet(initial, null, revision, new Set()) : initial)
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const commit = vi.spyOn(store, 'commit')
        const projectionReads: string[] = []
        const acquire = store.acquireRevision.bind(store)
        vi.spyOn(store, 'acquireRevision').mockImplementation(async (revision) => {
            const lease = await acquire(revision)
            const read = lease.readPreset.bind(lease)
            vi.spyOn(lease, 'readPreset').mockImplementation((id: string) => { projectionReads.push(id); return read(id) })
            return lease
        })
        await runtime.mutatePersistentPresets('add-complete-preset', (state) => { state.presets = [...state.presets, added] })
        expect((await store.readPreset('added-preset'))!.value).toEqual(added)
        if (!catalog) expect(getDatabase().botPresets.find((value) => value.id === added.id)).toEqual(added)
        await runtime.flushPendingDataLocally('added-preset-no-echo')
        expect(commit).toHaveBeenCalledOnce()
        expect(projectionReads).toEqual(catalog ? [] : ['added-preset'])
        expect(isCatalogPresetWorkingSet(getDatabase().botPresets)).toBe(catalog)
        const liveExisting = getDatabase().botPresets.find((value) => value.id === 'existing-preset')!
        liveExisting.mainPrompt = 'Retained loaded preset edit'
        await runtime.flushPendingDataLocally('loaded-preset-after-membership')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].unitMutations).toEqual([{key: '["preset","existing-preset","mainPrompt"]', type: 'set', value: 'Retained loaded preset edit'}])
        expect((await store.readPreset('existing-preset'))!.value.mainPrompt).toBe('Retained loaded preset edit')
    })

    it.each(['metadata', 'presets', 'delete', 'upsert', 'module'] as const)('keeps sparse activated root defaults local during the first targeted %s edit', async (scope) => {
        const initial = {username: 'Before', botPresets: [{id: 'initial-preset', name: 'Initial preset'}], botPresetsId: 0, characters: [], plugins: []} as unknown as Database
        const store = new IndexedDbPersistentDataStore(`sparse-activated-root-defaults-${scope}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.withPausedPersistentWrites('sparse-activation', async (token) => {
            const guard = runtime.beginActivatedLibraryGuard(token)
            await store.replaceFromDatabase({...initial, characters: [{type: 'character', chaId: 'sparse-owner', name: 'Owner', chatPage: 0,
                chatFolders: [], chats: [{id: 'sparse-chat', message: []}]}]} as unknown as Database, token.revision)
            await runtime.refreshActivatedLibraryUnderPause(token)
            guard.complete()
        })
        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('sparse-activated-no-op')
        expect(commit).not.toHaveBeenCalled()
        if (scope === 'metadata') await runtime.mutatePersistentCharacterDetail('sparse-owner', 'first-targeted-edit', ({character}) => { character.desc = 'Real targeted edit' })
        if (scope === 'presets') await runtime.mutatePersistentPresets('first-targeted-preset', (state) => { state.root.username = 'Explicit root edit' })
        if (scope === 'delete') await runtime.deletePersistentCharacter('sparse-owner', 'first-targeted-delete')
        if (scope === 'upsert') await runtime.upsertPersistentCompleteCharacter('new-owner', 'first-targeted-upsert', () => ({type: 'character', chaId: 'new-owner', name: 'New', chatPage: 0, chatFolders: [], chats: []}) as Database['characters'][number])
        if (scope === 'module') await runtime.appendPersistentRootModule('first-targeted-module', {module: {id: 'new-module', name: 'New module', description: ''}, assetAliases: [], ownerHead: {present: false, manifestHash: null, entryCount: 0}})
        await runtime.flushPendingDataLocally('after-targeted-edit')
        expect(commit).toHaveBeenCalledOnce()
        if (scope === 'metadata') expect(commit.mock.calls[0][0]).toMatchObject({unitMutations: [{key: '["character","sparse-owner","desc"]', type: 'set', value: 'Real targeted edit'}]})
        expect(commit.mock.calls.flatMap(([input]) => input.rootMutations ?? []).filter((value) => value.key !== 'characterOrder')).toEqual(scope === 'presets' ? [{type: 'set', key: 'username', value: 'Explicit root edit'}] : [])
        expect((await store.readRoot()).value).not.toHaveProperty('translatorMaxResponse')
    })

    it.each(['refresh', 'recovery'] as const)('installs an activated library with index selections and no stale character selection (%s)', async (path) => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.personas = [{...initial.personas[0], id: 'persona-a', name: 'A'}, {...initial.personas[0], id: 'persona-b', name: 'B'}]
        initial.selectedPersona = 1
        initial.botPresets = [{...initial.botPresets[0], id: 'preset-a', name: 'A'}, {...initial.botPresets[0], id: 'preset-b', name: 'B'}]
        initial.botPresetsId = 1
        const character = (chaId: string) => ({type: 'character', chaId, name: chaId, chatPage: 0, chatFolders: [], chats: [{id: `${chaId}-chat`, message: []}]})
        initial.characters = [character('first'), character('owner')] as unknown as Database['characters']
        const store = new IndexedDbPersistentDataStore(`activated-selection-${path}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const activated = {...structuredClone(initial), characters: [character('owner'), character('server-only')]} as unknown as Database
        const pause = runtime.withPausedPersistentWrites('activation', async (token) => {
            const guard = runtime.beginActivatedLibraryGuard(token)
            await store.replaceFromDatabase(activated, token.revision)
            if (path === 'recovery') throw new Error('Synthetic activation result lost')
            await runtime.refreshActivatedLibraryUnderPause(token)
            guard.complete()
        })
        if (path === 'recovery') {
            await expect(pause).rejects.toThrow('Synthetic activation result lost')
            await expect(runtime.retryCommittedWorkingSetRefresh()).resolves.toMatchObject({projection: 'applied'})
        } else await pause
        const database = getDatabase()
        expect(typeof database.selectedPersona).toBe('number')
        expect(database.personas[database.selectedPersona].id).toBe('persona-b')
        expect(typeof database.botPresetsId).toBe('number')
        expect(database.botPresets[database.botPresetsId].id).toBe('preset-b')
        expect(database.characters.map((value) => value.chaId)).toEqual(['owner', 'server-only'])
        expect(get(selectedCharID)).toBe(-1)
        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('activated-no-op')
        expect(commit).not.toHaveBeenCalled()
    })

    it.each(['refresh', 'recovery'] as const)('installs an activated library with the root fields boot prepares (%s)', async (path) => {
        const previousGlobals = {indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange}
        vi.resetModules()
        Object.assign(globalThis, {indexedDB: new IDBFactory(), IDBKeyRange})
        try {
            await import('../stores.svelte')
            const databaseModule = await import('./database.svelte')
            const runtimeModule = await import('./persistentDataRuntime.svelte')
            const factory = await import('./persistentDataStoreFactory')
            const preparation = await import('./databasePreparation')
            const catalog = await import('./workingSetCatalog')
            const { bootstrapPersistentDatabase } = await import('./persistentBootstrap')
            const character = (chaId: string) => ({type: 'character', chaId, name: chaId, chatPage: 0, chatFolders: [], chats: [{id: `${chaId}-chat`, message: []}]})
            // A stored root with neither field is valid input; only the working set receives them.
            const library = (username: string, ids: string[]) => ({apiType: 'openrouter', username, formatversion: 5,
                botPresets: [{id: 'preset-a', name: 'A'}], pluginCustomStorage: {}, characters: ids.map(character)}) as unknown as Database
            const raw = factory.getRawPersistentDataStore()
            await raw.open()
            await raw.replaceFromDatabase(library('Local', ['owner']), 0)
            const runtime = runtimeModule.getPersistentDataRuntime()
            const local = await bootstrapPersistentDatabase({
                store: runtime.store,
                prepareDatabase: preparation.prepareDatabaseForBootstrap,
                prepareRoot: preparation.preparePersistentRootForWorkingSet,
                projectScalableWorkingSet: (input) => catalog.projectCatalogWorkingSet(input.root, input.characters,
                    catalog.createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset)),
            })
            databaseModule.setDatabase(local.database)
            await runtime.initializeActiveWorkingSet(databaseModule.getDatabase())
            expect(databaseModule.getDatabase().characterOrder).toEqual([])
            expect(databaseModule.getDatabase().customSidebarItems).toEqual([])

            const pause = runtime.withPausedPersistentWrites('activation', async (token) => {
                const guard = runtime.beginActivatedLibraryGuard(token)
                await raw.replaceFromDatabase(library('Activated', ['owner', 'server-only']), token.revision)
                if (path === 'recovery') throw new Error('Synthetic activation result lost')
                await runtime.refreshActivatedLibraryUnderPause(token)
                guard.complete()
            })
            if (path === 'recovery') {
                await expect(pause).rejects.toThrow('Synthetic activation result lost')
                await expect(runtime.retryCommittedWorkingSetRefresh()).resolves.toMatchObject({projection: 'applied'})
            } else await pause

            const database = databaseModule.getDatabase()
            expect(database.username).toBe('Activated')
            expect(database.characters.map((value) => value.chaId)).toEqual(['owner', 'server-only'])
            expect(database.characterOrder).toEqual([])
            expect(database.customSidebarItems).toEqual([])
            const commit = vi.spyOn(raw, 'commit')
            await runtime.flushPendingDataLocally('activated-prepared-no-op')
            expect(commit).not.toHaveBeenCalled()
            expect((await raw.readRoot()).value).not.toHaveProperty('characterOrder')
            expect((await raw.readRoot()).value).not.toHaveProperty('customSidebarItems')
        } finally {
            Object.assign(globalThis, previousGlobals)
        }
    })

    it.each([false, true])('retains explicit and concurrent root deltas without copying defaults (failure=%s)', async (fail) => {
        const initial = {username: 'Before', translator: 'Before', botPresets: [{id: 'preset', name: 'Preset'}], botPresetsId: 0, personas: [{id: 'persona', name: 'Persona', prompt: ''}], selectedPersona: 0, plugins: [],
            opaqueRoot: {before: true}, removedRoot: 'Remove', characters: [{type: 'character', chaId: 'owner', name: 'Owner', chats: []}]} as unknown as Database
        const store = new IndexedDbPersistentDataStore(`sparse-concurrent-root-${fail}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const commit = vi.spyOn(store, 'commit')
        if (fail) commit.mockRejectedValueOnce(new Error('Synthetic root commit failure'))
        let attempts = 0
        const mutate = () => runtime.mutatePersistentCharacterDetail('owner', 'explicit-concurrent-root', async ({root, character}) => {
            const concurrent = ++attempts === 1 ? 'Concurrent edit' : 'Concurrent retry edit'
            root.translator = 'Callback edit'
            ;(root as unknown as Record<string, unknown>).opaqueRoot = {explicit: true}
            ;(root as unknown as Record<string, unknown>).nullableRoot = null
            character.desc = 'Explicit detail'
            await Promise.resolve()
            getDatabase().translator = concurrent
            const live = getDatabase() as unknown as Record<string, unknown>
            delete live.removedRoot
            live.addedRoot = {concurrent: true}
        })
        if (fail) {
            await expect(mutate()).rejects.toThrow('Synthetic root commit failure')
            expect((await store.readRoot()).value).toMatchObject({username: 'Before', translator: 'Before', opaqueRoot: {before: true}, removedRoot: 'Remove'})
            expect((await store.readCharacter('owner'))!.value).not.toHaveProperty('desc')
            await runtime.flushPendingDataLocally('retry-live-root-deltas')
            expect((await store.readRoot()).value.translator).toBe('Concurrent edit')
        }
        await mutate()
        const accepted = (await store.readRoot()).value
        expect(accepted).toMatchObject({translator: 'Callback edit', opaqueRoot: {explicit: true}, addedRoot: {concurrent: true}, nullableRoot: null})
        expect(accepted).not.toHaveProperty('removedRoot')
        expect(accepted).not.toHaveProperty('translatorMaxResponse')
        expect((await store.readCharacter('owner'))!.value).toMatchObject({desc: 'Explicit detail'})
        await runtime.flushPendingDataLocally('remaining-concurrent-view')
        expect((await store.readRoot()).value.translator).toBe(fail ? 'Concurrent retry edit' : 'Concurrent edit')
        const roots = commit.mock.calls.flatMap(([input]) => input.rootMutations ?? [])
        expect(roots.map((value) => value.key).filter((key) => !['translator', 'opaqueRoot', 'removedRoot', 'addedRoot', 'nullableRoot'].includes(key))).toEqual([])
    })

    it('observes and persists edits through a retained module after a preset operation', async () => {
        const initial = {
            username: 'Before', botPresets: [], botPresetsId: 0, characters: [], plugins: [],
            modules: [{ id: 'module', name: 'Synthetic module', description: '' }],
        } as unknown as Database
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore('root-publication-edit', indexedDB, IDBKeyRange)
        await store.open()
        const { revision } = await store.replaceFromDatabase(initial)
        setDatabaseLite(initial)
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const coordinator = new SaveCoordinator({ store, ...adapter })
        coordinator.initialize(revision)
        const markDirty = vi.fn((bytes: number) => coordinator.markPersistentDataDirty(bytes))
        const dispose = observePersistentSaveChanges({
            readDatabase: getDatabase, readSelectedCharacter: () => null, markDirty,
        })
        try {
            flushSync()
            const module = getDatabase().modules[0]
            await coordinator.mutatePersistentPresets('synthetic-preset-operation', ({ root }) => {
                root.username = 'After'
            })
            flushSync()
            markDirty.mockClear()
            module.description = 'Edited after publication'
            flushSync()
            expect(markDirty).toHaveBeenCalled()
            await coordinator.flushPendingDataLocally('synthetic-retained-editor')
            const reopened = new IndexedDbPersistentDataStore('root-publication-edit', indexedDB, IDBKeyRange)
            await reopened.open()
            expect((await reopened.readRoot()).value).toMatchObject({
                username: 'After', modules: [{ description: 'Edited after publication' }],
            })
        } finally {
            dispose()
        }
    })

    it.each(['root', 'preset', 'character'] as const)(
        'keeps unchanged editor data attached during %s publication', (kind) => {
            setDatabaseLite({
                username: 'Before', botPresets: [], characters: [], plugins: [],
                promptTemplate: [{ type: 'plain', text: 'Synthetic prompt', role: 'system' }],
                modules: [{ id: 'synthetic-module', name: 'Synthetic module', description: '' }],
                personas: [{ id: 'synthetic-persona', name: 'Synthetic persona', prompt: '' }],
                globalscript: [{ comment: 'Synthetic regex', in: '', out: '', type: 'editinput' }],
            } as unknown as Database)
            const database = getDatabase()
            const references = [database.promptTemplate, database.modules, database.personas, database.globalscript]
            const root = JSON.parse(JSON.stringify(capturePersistentRoot(database)))
            root.username = 'After'
            const adapter = createProductionStateAdapter()
            if (kind === 'root') adapter.publishRootWorkingSet!(root)
            if (kind === 'preset') adapter.publishPresetWorkingSet!({ revision: 2, root, presets: [] })
            if (kind === 'character') adapter.publishCharacterMutation!({
                revision: 2, root, kind: 'detail', characterId: 'absent', character: null,
            })
            expect(database.username).toBe('After')
            for (const [index, current] of [database.promptTemplate, database.modules, database.personas, database.globalscript].entries()) {
                expect(current).toBe(references[index])
            }
            const module = references[1][0] as Database['modules'][number]
            module.description = 'Edited after publication'
            expect(adapter.captureRoot().modules[0].description).toBe('Edited after publication')
        },
    )

    it.each(['character'])('tracks cold selection and subsequent %s edits in the same canonical capture', (type) => {
        setDatabaseLite({
            botPresets: [], plugins: [], pluginCustomStorage: {},
            characters: ['a', 'b'].map((id) => ({
                type, chaId: id, name: id, chatPage: 0,
                chats: [{ id: `chat-${id}`, note: '', message: [] }],
            })),
        } as unknown as Database)
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const capture = adapter.canonicalCapture!
        expect(capture.character()).toBeNull()
        for (const index of [0, 1, 0]) {
            selectedCharID.set(index)
            expect(capture.character()).toBe(canonicalJson(adapter.captureSelectedCharacter()))
            const character = getDatabase().characters[index]
            character.name = '수정 🙂'
            character.chats[0].note = 'Synthetic nested edit'
            expect(capture.character()).toBe(canonicalJson(adapter.captureSelectedCharacter()))
            character.name = ''
            expect(capture.character()).toBe(canonicalJson(adapter.captureSelectedCharacter()))
        }
    })

    it.each(['modules', 'loadouts', 'customModels', 'personas', 'protectedPresetValues', 'explicitGlobalChatVariables'] as const)(
        'converges an empty %s projection omitted from the accepted root without a durable write', async (field) => {
            const initial = {username: 'Before', botPresets: [], botPresetsId: 0, characters: [], plugins: [],
                globalChatVariables: {}, explicitGlobalChatVariables: {}} as unknown as Database
            delete initial[field]
            setDatabaseLite(initial)
            selectedCharID.set(-1)
            const adapter = createProductionStateAdapter()
            const commit = vi.fn(async ({expectedRevision}) => ({revision: expectedRevision + 1}))
            const coordinator = new SaveCoordinator({store: createStoreStub({commit}), ...adapter})
            coordinator.initialize(1)
            const accepted = adapter.captureRoot()
            delete accepted[field]
            coordinator.adoptAppliedUnitState(1, accepted, null, [])
            const current = getDatabase() as unknown as Record<string, unknown>
            current[field] = field.endsWith('Values') || field === 'explicitGlobalChatVariables' ? {} : []
            const capture = adapter.canonicalCapture!
            const root = capture.root.bind(capture)
            let captures = 0
            vi.spyOn(capture, 'root').mockImplementation(() => {
                if (++captures > 12) throw new Error('Non-converging empty split-root projection')
                return root()
            })
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingDataLocally('empty-split-root-projection')
            expect(commit).not.toHaveBeenCalled()
            expect(coordinator.revision).toBe(1)
            expect(coordinator.hasPendingPersistenceWork).toBe(false)
            expect(captures).toBeLessThanOrEqual(2)
            getDatabase().username = 'Real edit'
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingDataLocally('after-empty-projection')
            expect(commit).toHaveBeenCalledExactlyOnceWith({expectedRevision: 1,
                rootMutations: [{key: 'username', type: 'set', value: 'Real edit'}]})
        },
    )

    it.each(['explicitGlobalChatVariables', 'protectedPresetValues'] as const)(
        'persists real %s additions and deletions after empty projection convergence', async (field) => {
            setDatabaseLite({username: 'Before', botPresets: [], botPresetsId: 0, characters: [], plugins: [],
                globalChatVariables: {}, explicitGlobalChatVariables: {}, protectedPresetValues: {}} as unknown as Database)
            selectedCharID.set(-1)
            const adapter = createProductionStateAdapter()
            const commit = vi.fn(async ({expectedRevision}) => ({revision: expectedRevision + 1}))
            const coordinator = new SaveCoordinator({store: createStoreStub({commit}), ...adapter})
            coordinator.initialize(1)
            const accepted = adapter.captureRoot()
            delete accepted[field]
            coordinator.adoptAppliedUnitState(1, accepted, null, [])
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingDataLocally('empty-map-projection')
            expect(commit).not.toHaveBeenCalled()
            const map = getDatabase()[field] as Record<string, unknown>
            const key = field === 'explicitGlobalChatVariables' ? 'ordinary' : 'seperateModels'
            const unitKey = JSON.stringify([field === 'explicitGlobalChatVariables' ? 'variable' : 'preset-protected', key])
            const value = field === 'explicitGlobalChatVariables' ? 'Real value' : {memory: 'Real value'}
            map[key] = value
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingDataLocally('real-map-add')
            expect(commit).toHaveBeenCalledExactlyOnceWith({expectedRevision: 1, rootMutations: [],
                unitMutations: [{key: unitKey, type: 'set', value}]})
            delete map[key]
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingDataLocally('real-map-delete')
            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toEqual({expectedRevision: 2, rootMutations: [],
                unitMutations: [{key: unitKey, type: 'delete'}]})
            await coordinator.flushPendingDataLocally('empty-map-again')
            expect(commit).toHaveBeenCalledTimes(2)
            expect(coordinator.hasPendingPersistenceWork).toBe(false)
        },
    )

    it('retains genuine split-root units when an empty projection accompanies a failed commit', async () => {
        setDatabaseLite({username: 'Before', botPresets: [], botPresetsId: 0, characters: [], plugins: [],
            globalChatVariables: {}, explicitGlobalChatVariables: {},
            modules: [{id: 'module', name: 'Module', description: 'Before'}]} as unknown as Database)
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const commit = vi.fn().mockRejectedValueOnce(new Error('Synthetic failure'))
            .mockImplementation(async ({expectedRevision}) => ({revision: expectedRevision + 1}))
        const coordinator = new SaveCoordinator({store: createStoreStub({commit}), ...adapter})
        coordinator.initialize(1)
        const accepted = adapter.captureRoot()
        delete accepted.explicitGlobalChatVariables
        coordinator.adoptAppliedUnitState(1, accepted, null, [])
        getDatabase().modules[0].description = 'Real record edit'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingDataLocally('mixed-empty-real-unit')).rejects.toThrow('Synthetic failure')
        expect(coordinator.hasPendingPersistenceWork).toBe(true)
        await coordinator.flushPendingDataLocally('retry-mixed-empty-real-unit')
        expect(commit).toHaveBeenCalledTimes(2)
        for (const [input] of commit.mock.calls) expect(input).toEqual({expectedRevision: 1, rootMutations: [], unitMutations: [
            {key: '["record","modules","module"]', type: 'set', value: {id: 'module', name: 'Module', description: 'Real record edit'}},
        ]})
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it('uses canonical captures for immediate production edits and emits a small delta', async () => {
        setDatabaseLite({
            username: 'Before',
            customBackground: 'x'.repeat(6 * 1024 * 1024),
            botPresets: [],
            plugins: [],
            characters: [
                {
                    type: 'character',
                    chaId: 'synthetic-large',
                    name: 'Synthetic',
                    chatPage: 0,
                    chats: [
                        {
                            id: 'synthetic-chat',
                            message: [{ role: 'char', data: 'x'.repeat(2 * 1024 * 1024) }],
                        },
                    ],
                },
            ],
            pluginCustomStorage: {},
        } as unknown as Database)
        selectedCharID.set(0)
        const adapter = createProductionStateAdapter()
        expect(adapter.canonicalCapture).toBeDefined()
        const commit = vi.fn(async () => ({ revision: 2 }))
        const coordinator = new SaveCoordinator({
            ...adapter,
            captureRoot: () => {
                throw new Error('Must use the production canonical capture')
            },
            store: createStoreStub({ commit }),
        })
        coordinator.initialize(1)
        getDatabase().username = 'Immediately changed'
        coordinator.markPersistentDataDirty(1)
        const parse = vi.spyOn(JSON, 'parse')
        try {
            await coordinator.flushPendingData('immediate-production-mutation')
            expect(parse.mock.calls.some(([value]) => value.length > 1024 * 1024)).toBe(false)
        } finally {
            parse.mockRestore()
        }
        expect(commit).toHaveBeenCalledExactlyOnceWith({
            expectedRevision: 1,
            rootMutations: [{ type: 'set', key: 'username', value: 'Immediately changed' }],
        })
        expect(JSON.stringify(commit.mock.calls[0]).length).toBeLessThan(256)
    })

    it('adopts a small production root edit without decoding an unchanged large background in the renderer', async () => {
        const initial = {
            username: 'Before', customBackground: 'x'.repeat(6 * 1024 * 1024),
            botPresets: [], plugins: [], characters: [], pluginCustomStorage: {},
            modules: [{ id: 'module', name: 'Before', description: '' }],
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore('cached-root-adoption', new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabaseLite(initial)
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const runtime = createPersistentDataRuntime({ store, state: adapter, prepareDatabase: async (value) => value })
        await runtime.initializeActiveWorkingSet(getDatabase())
        const coordinator = new SaveCoordinator({ store, ...adapter })
        coordinator.initialize(runtime.revision)
        const originalParse = JSON.parse
        let inBackendCommit = false
        const rendererParseLengths: number[] = []
        const parse = vi.spyOn(JSON, 'parse').mockImplementation((value, reviver) => {
            if (!inBackendCommit) rendererParseLengths.push(value.length)
            return originalParse(value, reviver)
        })
        const originalCommit = store.commit.bind(store)
        // IndexedDB's backend root clone is outside the renderer capture/adoption path.
        const commit = vi.spyOn(store, 'commit').mockImplementation(async (request) => {
            inBackendCommit = true
            try { return await originalCommit(request) }
            finally { inBackendCommit = false }
        })
        try {
            const baseline = coordinator.capturePersistentBaselineRoot()
            baseline.modules[0].name = 'Detached baseline only'
            expect(coordinator.capturePersistentBaselineRoot().modules[0].name).toBe('Before')
            getDatabase().modules[0].name = 'After'
            runtime.markPersistentDataDirty(1)
            await runtime.flushPendingData('cached-root-adoption')
            runtime.markPersistentDataDirty(1)
            await runtime.flushPendingData('cached-root-no-echo')
            expect(rendererParseLengths.some((length) => length > 1024 * 1024)).toBe(false)
        } finally {
            parse.mockRestore()
        }
        expect(commit).toHaveBeenCalledTimes(1)
        const persisted = (await store.readRoot()).value
        expect(persisted.modules[0].name).toBe('After')
        expect(persisted.customBackground).toBe(initial.customBackground)
    })

    it.each(['root', 'preset', 'mirror', 'selection'] as const)('retains concurrent catalog preset edits during async %s unit projection', async (scope) => {
        const initial = {
            language: 'en', mainPrompt: 'Initial prompt', botPresetsId: 0,
            botPresets: [{ id: 'resident-preset', name: 'Initial preset', mainPrompt: 'Initial prompt' },
                { id: 'other-preset', name: 'Other preset', mainPrompt: 'Other prompt' }],
            plugins: [], characters: [], pluginCustomStorage: {},
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore(`catalog-preset-race-${scope}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        const { revision } = await store.replaceFromDatabase(initial)
        setDatabaseLite(projectCompleteScalableWorkingSet(initial, null, revision))
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const runtime = createPersistentDataRuntime({store, state: adapter, prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        expect(adapter.capturePresets?.()).toBeNull()
        expect(adapter.capturePresetRecords?.()).toHaveLength(1)
        let entered!: () => void
        const reading = new Promise<void>((resolve) => { entered = resolve })
        let release!: () => void
        const blocked = new Promise<void>((resolve) => { release = resolve })
        const acquire = store.acquireRevision.bind(store)
        vi.spyOn(store, 'acquireRevision').mockImplementationOnce(async (requested) => {
            const lease = await acquire(requested)
            if (scope === 'preset' || scope === 'selection') {
                const read = lease.readPreset.bind(lease)
                vi.spyOn(lease, 'readPreset').mockImplementationOnce(async (id) => {
                    const value = await read(id)
                    entered()
                    await blocked
                    return value
                })
            } else {
                const read = lease.readRoot.bind(lease)
                vi.spyOn(lease, 'readRoot').mockImplementationOnce(async () => {
                    const value = await read()
                    entered()
                    await blocked
                    return value
                })
            }
            return lease
        })
        const commit = vi.spyOn(store, 'commit')
        const mirrorEdit = scope === 'mirror' || scope === 'selection'
        const projected = runtime.commitPersistentUnitIntent('catalog-preset-race', [{
            key: scope === 'selection' ? '["root","botPresetsId"]' : scope === 'preset' ? '["preset","resident-preset","name"]' : '["root","language"]',
            type: 'set', value: scope === 'selection' ? 'other-preset' : scope === 'preset' ? 'Received preset' : 'ko',
        }])
        await reading
        if (mirrorEdit) getDatabase().mainPrompt = 'Concurrent prompt'
        else getDatabase().botPresets[0].name = 'Concurrent preset'
        runtime.markPersistentDataDirty(1)
        release()
        await projected
        if (mirrorEdit) {
            expect(getDatabase().mainPrompt).toBe(scope === 'selection' ? 'Other prompt' : 'Concurrent prompt')
            expect(getDatabase().botPresets.find((value) => value['id'] === 'resident-preset')?.mainPrompt).toBe('Concurrent prompt')
            expect((await store.readPreset('resident-preset'))?.value.mainPrompt).toBe('Initial prompt')
        } else {
            expect(getDatabase().botPresets[0].name).toBe('Concurrent preset')
            expect((await store.readPreset('resident-preset'))?.value.name).toBe(scope === 'root' ? 'Initial preset' : 'Received preset')
        }
        await runtime.flushPendingDataLocally('persist-concurrent-catalog-preset')
        if (mirrorEdit) {
            expect((await store.readPreset('resident-preset'))?.value.mainPrompt).toBe('Concurrent prompt')
            expect((await store.readPreset('other-preset'))?.value.mainPrompt).toBe('Other prompt')
        } else expect((await store.readPreset('resident-preset'))?.value.name).toBe('Concurrent preset')
        expect(commit).toHaveBeenCalledTimes(2)
        await runtime.flushPendingDataLocally('catalog-preset-no-echo')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('writes nothing to the working set for receives that affected nothing', async () => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'resident-preset'
        initial.personas[0].id = 'resident-persona'
        const store = new IndexedDbPersistentDataStore('receive-without-affected-keys', new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        const {revision} = await store.replaceFromDatabase(initial)
        setDatabaseLite(projectCompleteScalableWorkingSet(initial, null, revision))
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const runtime = createPersistentDataRuntime({store, state: adapter, prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('receive-without-affected-keys-settle')
        const flush = vi.spyOn(adapter, 'beforeCapture')
        const derive = vi.spyOn(adapter, 'afterRemoteApply')
        const lease = vi.spyOn(store, 'acquireRevision')
        store.lwwStageReceive = async () => undefined
        store.lwwApplyReceive = async () => ({revision: runtime.revision, affectedKeys: [], heldKeys: [], deferredKeys: []})
        store.lwwFinishReceive = async () => undefined
        const committedRevision = runtime.revision
        const markDirty = vi.fn()
        const dispose = observePersistentSaveChanges({readDatabase: getDatabase, readSelectedCharacter: () => null, markDirty})
        try {
            flushSync()
            markDirty.mockClear()
            const database = getDatabase()
            const keys = Object.keys(database)
            const variables = database.globalChatVariables
            const template = database.promptTemplate
            const commit = vi.spyOn(store, 'commit')
            for (const requestId of ['empty', 'echo']) {
                await runtime.applyLwwReceive({bindingAuthority: '1', requestId, changes: [], progress: {kind: 'server', cursor: '1'}, admittedTimeUpperMs: '100'})
            }
            flushSync()
            expect(flush).not.toHaveBeenCalled()
            expect(derive).not.toHaveBeenCalled()
            expect(lease).not.toHaveBeenCalled()
            expect(markDirty).not.toHaveBeenCalled()
            expect(getDatabase()).toBe(database)
            expect(Object.keys(database)).toEqual(keys)
            expect(database.globalChatVariables).toBe(variables)
            expect(database.promptTemplate).toBe(template)
            expect(runtime.revision).toBe(committedRevision)
            await runtime.flushPendingDataLocally('receive-without-affected-keys')
            expect(commit).not.toHaveBeenCalled()
        } finally {
            dispose()
        }
    })

    it('adopts received protected flags and explicit toggles without creating preset or toggle echoes', async () => {
        const initial = {
            botPresetsId: 0, doNotChangeSeperateModels: false, seperateModels: {memory: 'initial'},
            protectedPresetValues: {}, explicitGlobalChatVariables: {toggle_mode: 'initial'},
            globalChatVariables: {toggle_mode: 'initial'},
            botPresets: [{id: 'resident-preset', name: 'Preset', seperateModels: {memory: 'initial'}}],
            plugins: [], characters: [], pluginCustomStorage: {},
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore('catalog-protected-no-echo', new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        const {revision} = await store.replaceFromDatabase(initial)
        setDatabaseLite(projectCompleteScalableWorkingSet(initial, null, revision))
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        const runtime = createPersistentDataRuntime({store, state: adapter, prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const mutations = [
            {key: '["root","doNotChangeSeperateModels"]', type: 'set' as const, value: true},
            {key: '["preset-protected","seperateModels"]', type: 'set' as const, value: {memory: 'received'}},
            {key: '["toggle","toggle_mode"]', type: 'set' as const, value: 'received-toggle'},
        ]
        store.lwwStageReceive = async () => undefined
        store.lwwApplyReceive = async () => {
            const result = await store.commit({expectedRevision: runtime.revision, unitMutations: mutations})
            return {...result, affectedKeys: mutations.map((value) => value.key), heldKeys: [], deferredKeys: []}
        }
        store.lwwFinishReceive = async () => undefined
        const commit = vi.spyOn(store, 'commit')
        await runtime.applyLwwReceive({bindingAuthority: '1', requestId: 'protected-receive', changes: [], progress: {kind: 'server', cursor: '1'}, admittedTimeUpperMs: '100'})
        expect(getDatabase().doNotChangeSeperateModels).toBe(true)
        expect(getDatabase().protectedPresetValues?.seperateModels).toEqual({memory: 'received'})
        expect(getDatabase().seperateModels).toEqual({memory: 'received'})
        expect(getDatabase().globalChatVariables.toggle_mode).toBe('received-toggle')
        expect(getDatabase().explicitGlobalChatVariables?.toggle_mode).toBe('received-toggle')
        expect((await store.readPreset('resident-preset'))?.value.seperateModels).toEqual({memory: 'initial'})
        commit.mockClear()
        await runtime.flushPendingDataLocally('protected-receive-no-echo')
        expect(commit).not.toHaveBeenCalled()
    })

    it('restores only affected activation entries, selection and residency', () => {
        const selected = {
            type: 'character',
            chaId: 'char-a',
            name: 'Selected',
            chats: [
                {
                    id: 'chat-a',
                    name: 'Chat',
                    note: '',
                    localLore: [],
                    message: [],
                },
            ],
            chatPage: 0,
        }
        const related = createCatalogCharacterStub({
            id: 'char-b',
            configuredIndex: 1,
            conversationCount: 2,
            name: 'Related',
            type: 'character',
            recentAt: 0,
            trashed: false,
        })
        const unaffected = {
            type: 'character',
            chaId: 'char-c',
            name: 'Unaffected',
            chats: [],
        }
        setDatabaseLite({
            botPresets: [],
            plugins: [],
            characters: [selected, related, unaffected],
        } as unknown as Database)
        selectedCharID.set(0)
        const residentSelected = getDatabase().characters[0]
        const residentRelated = getDatabase().characters[1]
        workingSetResidency.markCharacterHydrated('char-a')
        workingSetResidency.reconcileConversationResidency(residentSelected)
        workingSetResidency.markCharacterReleased('char-b')
        const restore = createProductionStateAdapter()
            .captureActivationRollback!(['char-a', 'char-b'])

        getDatabase().characters[0] = {
            type: 'character',
            chaId: 'char-a',
            name: 'Tentative selected',
            chats: [],
        } as any
        getDatabase().characters[1] = {
            type: 'character',
            chaId: 'char-b',
            name: 'Tentative related',
            chats: [],
        } as any
        getDatabase().characters[2].name = 'Concurrent unaffected edit'
        selectedCharID.set(1)
        workingSetResidency.markCharacterReleased('char-a')
        workingSetResidency.markCharacterHydrated('char-b')

        restore()

        expect(getDatabase().characters[0]).toBe(residentSelected)
        expect(getDatabase().characters[1]).toBe(residentRelated)
        expect(getDatabase().characters[2].name).toBe(
            'Concurrent unaffected edit',
        )
        expect(get(selectedCharID)).toBe(0)
        expect(workingSetResidency.isCharacterReleased('char-a')).toBe(false)
        expect(
            workingSetResidency.canReleaseConversation(
                residentSelected,
                'chat-a',
            ),
        ).toBe(false)
        expect(workingSetResidency.isCharacterReleased('char-b')).toBe(true)
    })

    it.each([
        { released: 'char-b', eviction: true, selectedAfter: -1, stub: true },
        { released: 'char-a', eviction: true, selectedAfter: 1, stub: true },
        { released: 'char-b', eviction: false, selectedAfter: 1, stub: false },
    ])('never leaves the selection on a released character ($released, eviction=$eviction)', ({ released, eviction, selectedAfter, stub }) => {
        const character = (chaId: string) => ({
            type: 'character', chaId, name: chaId, chatPage: 0, chatFolders: [],
            chats: [{ id: `${chaId}-chat`, name: 'Chat', note: '', localLore: [], message: [] }],
        })
        setDatabaseLite({
            botPresets: [],
            plugins: [],
            characters: [character('char-a'), character('char-b')],
        } as unknown as Database)
        selectedCharID.set(1)
        workingSetResidency.setEvictionAllowed(eviction)

        createProductionStateAdapter().releaseInactiveCharacter!(released)

        const releasedCharacter = getDatabase().characters.find((value) => value.chaId === released)!
        expect(isCatalogCharacterStub(releasedCharacter)).toBe(stub)
        expect(get(selectedCharID)).toBe(selectedAfter)
        expect(workingSetResidency.isCharacterReleased(released)).toBe(stub)
    })

    it('reports selected lifecycle policy, operation and viewport budget', () => {
        setDatabaseLite({
            botPresets: [],
            characters: [],
            plugins: [],
        } as unknown as Database)
        workingSetResidency.setEvictionAllowed(true)
        doingChat.set(false)
        const adapter = createProductionStateAdapter()
        const operationTransitions: boolean[] = []
        const unsubscribeOperation = adapter.subscribeConversationOperationActive?.(
            (active) => operationTransitions.push(active),
        )

        expect(adapter.canUseWindowedSelectedConversation?.()).toBe(true)
        expect(adapter.isConversationOperationActive?.()).toBe(false)
        expect(adapter.conversationViewportRowBudget).toBe(
            getRuntimePerformanceBudgets().chatMountedMessageBudget,
        )

        workingSetResidency.setEvictionAllowed(false)
        doingChat.set(true)

        expect(adapter.canUseWindowedSelectedConversation?.()).toBe(false)
        expect(adapter.isConversationOperationActive?.()).toBe(true)
        doingChat.set(false)
        expect(operationTransitions).toEqual([false, true, false])
        unsubscribeOperation?.()
    })

    it('clears old residency before the synchronous projector records the replacement', () => {
        const initial = {
            botPresetsId: 0,
            botPresets: [{ name: 'Active' }],
            characters: [{
                type: 'character',
                chaId: 'selected-a',
                name: 'Selected',
                chats: [],
            }],
        } as unknown as Database
        const replacement = {
            ...initial,
            characters: [{
                type: 'character',
                chaId: 'member-a',
                name: 'Member',
                personality: 'resident detail',
                chats: [],
            }, initial.characters[0], {
                type: 'character',
                chaId: 'inactive',
                name: 'Inactive',
                personality: 'must be released',
                chats: [],
            }],
        } as unknown as Database
        setDatabaseLite(initial)
        selectedCharID.set(0)
        workingSetResidency.markCharacterReleased('stale')
        let oldResidencyClearedBeforeProjection = false
        configurePersistentDataRuntime({
            projectWorkingSet(database, selectedCharacterId, selectedConversationId, activeIds) {
                oldResidencyClearedBeforeProjection =
                    !workingSetResidency.isCharacterReleased('stale')
                const projected = projectCompleteScalableWorkingSet(
                    database,
                    selectedCharacterId,
                    2,
                    activeIds,
                    selectedConversationId,
                )
                for (const character of projected.characters) {
                    if (isCatalogCharacterStub(character)) {
                        workingSetResidency.markCharacterReleased(character.chaId)
                    }
                }
                return projected
            },
        })

        createProductionStateAdapter().replaceDatabase(
            replacement,
            new Set(['selected-a', 'member-a']),
            true,
        )

        expect(oldResidencyClearedBeforeProjection).toBe(true)
        expect(workingSetResidency.isCharacterReleased('inactive')).toBe(true)
        expect(getDatabase().characters[0].personality).toBe('resident detail')
        expect(isCatalogCharacterStub(getDatabase().characters[2])).toBe(true)
    })

    it.each([false, true])('takes streaming state across a whole replacement from running generations only (running: %s)', (running) => {
        const chat = (id: string) => ({ id, name: 'Synthetic chat', message: [] })
        setDatabaseLite({ botPresetsId: 0, botPresets: [{ name: 'Active' }], characters: [] } as unknown as Database)
        selectedCharID.set(-1)
        // The replacement carries a saved flag and mode; only a running generation decides either.
        const replacement = { botPresetsId: 0, botPresets: [{ name: 'Active' }], characters: [{
            type: 'character', chaId: 'streaming-owner', name: 'Owner',
            chats: [{ ...chat('streaming-chat'), isStreaming: true, activeStreamingDisplayOptimizationMode: 'strong' }, chat('idle-chat')],
        }] } as unknown as Database
        if (running) setStreamingConversation('streaming-chat', 'balanced')
        try {
            createProductionStateAdapter().replaceDatabase(replacement, new Set(['streaming-owner']), false)
            const [streamed, idle] = getDatabase().characters[0].chats
            expect([streamed.isStreaming, streamed.activeStreamingDisplayOptimizationMode]).toEqual(running ? [true, 'balanced'] : [false, undefined])
            expect([idle.isStreaming, idle.activeStreamingDisplayOptimizationMode]).toEqual([false, undefined])
        } finally {
            clearStreamingConversation('streaming-chat')
        }
    })

    it.each(['replaceDatabase', 'publishCharacter', 'publishConversation'] as const)('fills the lorebook text a stored entry lacks when it installs a character (%s)', (seam) => {
        const stored = () => ({ key: null, secondkey: null, comment: null, content: null, mode: 'normal', insertorder: 100, alwaysActive: true, selective: false })
        const chat = (localLore: unknown[]) => ({ id: 'lore-chat', name: 'Synthetic chat', message: [], localLore })
        const owner = (globalLore: unknown[], localLore: unknown[]) => ({ type: 'character', chaId: 'lore-owner', name: 'Owner', globalLore, chats: [chat(localLore)] })
        const database = (characters: unknown[]) => ({ botPresetsId: 0, botPresets: [{ name: 'Active' }], characters }) as unknown as Database
        setDatabaseLite(database(seam === 'replaceDatabase' ? [] : [owner([], [])]))
        selectedCharID.set(-1)
        const adapter = createProductionStateAdapter()
        // Each seam receives entries as a store or an older writer left them.
        if (seam === 'replaceDatabase') adapter.replaceDatabase(database([owner([stored()], [stored()])]), new Set(['lore-owner']), false)
        else if (seam === 'publishCharacter') adapter.publishCharacter(owner([stored()], [stored()]) as never)
        else adapter.publishConversation('lore-owner', chat([stored()]) as never)
        const installed = getDatabase().characters[0]
        const filled = expect.objectContaining({ key: '', secondkey: '', comment: '', content: '' })
        expect(installed.chats[0].localLore).toEqual([filled])
        expect(installed.globalLore).toEqual(seam === 'publishConversation' ? [] : [filled])
    })

    it('preloads nested maximum plugin values from the installed plain database', async () => {
        const nestedValue = {
            list: [{ enabled: true }],
            settings: { mode: 'maximum' },
        }
        const storage = createPluginStorageStore({
            getStorageAuthorityEpoch: () => 0,
            assertPersistentMutationAllowed: vi.fn(),
            store: createStoreStub({
                open: async () => undefined,
                queryPluginStorage: async () => ({
                    revision: 1,
                    items: [{ owner: UNOWNED_PLUGIN_OWNER, key: 'nested', byteSize: 1 }],
                }),
                readPluginStorage: async () => ({ revision: 1, value: nestedValue }),
            }),
            mutate: async () => undefined,
        })
        const unregister = registerPluginStorageLifecycle(storage)
        const complete = {
            botPresets: [],
            pluginCustomStorage: { nested: nestedValue },
            characters: [],
        } as unknown as Database
        try {
            createProductionStateAdapter().installCompleteDatabase!(complete)

            await expect(storage.forOwner(UNOWNED_PLUGIN_OWNER).keys()).resolves.toEqual(['nested'])
            await expect(storage.forOwner(UNOWNED_PLUGIN_OWNER).getItem('nested')).resolves.toEqual(nestedValue)
            expect(await storage.forOwner(UNOWNED_PLUGIN_OWNER).getItem('nested')).not.toBe(nestedValue)
        } finally {
            unregister()
        }
    })
})

describe('preset chain override across a whole working-set replacement', () => {
    function presetDatabase(): Database {
        const database = normalizeDatabaseDefaults({} as Database)
        database.botPresets[0].id = 'preset-a'
        database.botPresets[0].mainPrompt = 'Prompt A'
        database.botPresets.push({ ...structuredClone(database.botPresets[0]), id: 'preset-b', name: 'Preset B', mainPrompt: 'Prompt B' })
        database.botPresetsId = 0
        database.mainPrompt = 'Prompt A'
        database.personas[0].id = 'persona'
        return database
    }

    it.each([true, false])('keeps the override when its preset is still present (resident=%s)', async (resident) => {
        const initial = presetDatabase()
        const store = new IndexedDbPersistentDataStore(`override-carry-${resident}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        const { revision } = await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        setEffectivePresetOverride('preset-b')
        expect(getDatabase().mainPrompt).toBe('Prompt B')
        const replacement = resident ? structuredClone(initial) : projectCompleteScalableWorkingSet(initial, null, revision, new Set())
        createProductionStateAdapter({ readPreset: (id) => store.readPreset(id) }).replaceDatabase(replacement, new Set(), false)
        await vi.waitFor(() => expect(getEffectivePresetId()).toBe('preset-b'))
        expect(getDatabase().mainPrompt).toBe('Prompt B')
        expect(getDatabase().botPresets.find((preset) => preset.id === 'preset-a')!.mainPrompt).toBe('Prompt A')
    })

    it('clears the override when its preset is gone', () => {
        const initial = presetDatabase()
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        setEffectivePresetOverride('preset-b')
        const replacement = structuredClone(initial)
        replacement.botPresets.pop()
        createProductionStateAdapter().replaceDatabase(replacement, new Set(), false)
        expect(getEffectivePresetId()).toBe('preset-a')
    })
})

describe('received removal of the selected character or conversation', () => {
    const chat = (id: string) => ({ id, name: id, note: '', localLore: [], message: [{ role: 'char', data: `${id} message`, chatId: `${id}-message` }] })
    const character = (chaId: string, chats: string[]) => ({ type: 'character', chaId, name: chaId, chatPage: 0, chatFolders: [], chats: chats.map(chat) })

    async function receiveRuntime(windowed: boolean, selected: number, chatPage = 0, conversations = ['b-chat-1', 'b-chat-2']) {
        workingSetResidency.setEvictionAllowed(windowed)
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas[0].id = 'persona'
        initial.characters = [character('char-a', ['a-chat']), character('char-b', conversations)] as unknown as Database['characters']
        initial.characters[1].chatPage = chatPage
        const store = new IndexedDbPersistentDataStore(`received-selection-removal-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        const { revision } = await store.replaceFromDatabase(initial)
        const selectedId = initial.characters[selected].chaId
        setDatabaseLite(projectCompleteScalableWorkingSet(initial, selectedId, revision, new Set([selectedId])))
        selectedCharID.set(selected)
        const saveFailures: unknown[] = []
        const state = createProductionStateAdapter()
        const runtime = createPersistentDataRuntime({
            store, state, prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) },
        })
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('received-selection-removal-settle')
        expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
        // Mirror derivation runs in the same turn as the projection, before any view can render it.
        const atProjection: { selected: number; character: string | undefined; conversation: string | undefined }[] = []
        const deriveMirrors = state.afterRemoteApply!
        state.afterRemoteApply = () => {
            const current = getDatabase().characters[get(selectedCharID)]
            atProjection.push({ selected: get(selectedCharID), character: current?.chaId, conversation: current?.chats[current.chatPage ?? 0]?.id })
            deriveMirrors()
        }
        const receive = async (units: (string | PersistentUnitMutation)[]) => {
            const mutations = units.map((unit): PersistentUnitMutation => typeof unit === 'string' ? { key: unit, type: 'delete' } : unit)
            store.lwwStageReceive = async () => undefined
            store.lwwFinishReceive = async () => undefined
            store.lwwApplyReceive = async () => {
                const result = mutations.length === 0
                    ? { revision: runtime.revision }
                    : await store.commit({ expectedRevision: runtime.revision, unitMutations: mutations })
                return { ...result, affectedKeys: mutations.map((value) => value.key), heldKeys: [], deferredKeys: [] }
            }
            await runtime.applyLwwReceive({ bindingAuthority: '1', requestId: crypto.randomUUID(), changes: [], progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' })
        }
        return { store, runtime, receive, saveFailures, atProjection }
    }

    it.each([true, false])('returns to no selection when the selected character is removed (windowed=%s)', async (windowed) => {
        const { store, runtime, receive, saveFailures, atProjection } = await receiveRuntime(windowed, 1)
        await receive(['["exists","character","char-b"]'])

        expect(atProjection[0]).toEqual({ selected: -1, character: undefined, conversation: undefined })
        flushSync()
        expect(get(selectedCharID)).toBe(-1)
        expect(getDatabase().characters.map((value) => value.chaId)).toEqual(['char-a'])
        expect(runtime.captureSelectedConversationTarget()).toBeNull()
        expect(runtime.getSelectedConversationMode()).toBeNull()
        const commit = vi.spyOn(store, 'commit')
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingDataLocally('after-selected-character-removal')
        await receive([])
        expect(saveFailures).toEqual([])
        expect(JSON.stringify(commit.mock.calls)).not.toContain('char-b')
        expect(await store.readCharacterSummary('char-b')).toBeNull()
    })

    it.each([true, false])('keeps the selected character when an earlier one is removed (windowed=%s)', async (windowed) => {
        const { store, runtime, receive, saveFailures, atProjection } = await receiveRuntime(windowed, 1)
        await receive(['["exists","character","char-a"]'])

        expect(atProjection[0]).toEqual({ selected: 0, character: 'char-b', conversation: 'b-chat-1' })
        expect(runtime.captureSelectedConversationTarget()).toMatchObject({ characterId: 'char-b', conversationId: 'b-chat-1' })
        expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
        const commit = vi.spyOn(store, 'commit')
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingDataLocally('after-earlier-character-removal')
        expect(saveFailures).toEqual([])
        expect(commit).not.toHaveBeenCalled()
    })

    it.each([true, false])('keeps the open conversation when an earlier one is removed (windowed=%s)', async (windowed) => {
        const { store, runtime, receive, saveFailures, atProjection } = await receiveRuntime(windowed, 1, 1)
        await receive(['["exists","conversation","char-b","b-chat-1"]'])

        expect(atProjection[0]).toEqual({ selected: 1, character: 'char-b', conversation: 'b-chat-2' })
        expect(getDatabase().characters[1].chatPage).toBe(0)
        expect(runtime.captureSelectedConversationTarget()).toMatchObject({ characterId: 'char-b', conversationId: 'b-chat-2' })
        expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('after-earlier-conversation-removal')
        expect(saveFailures).toEqual([])
        // Only the local position of the open conversation follows it.
        expect(commit.mock.calls.flatMap(([input]) => input.unitMutations ?? [])).toEqual([{ key: '["character","char-b","chatPage"]', type: 'set', value: 0 }])
    })

    it.each([true, false])('keeps the open conversation when a received order moves it (windowed=%s)', async (windowed) => {
        const { store, runtime, receive, saveFailures, atProjection } = await receiveRuntime(windowed, 1)
        await receive([{ key: '["order","conversations","char-b"]', type: 'set', value: { ids: ['b-chat-2', 'b-chat-1'], folders: [] } }])

        expect(atProjection[0]).toEqual({ selected: 1, character: 'char-b', conversation: 'b-chat-1' })
        expect(getDatabase().characters[1].chats.map((value) => value.id)).toEqual(['b-chat-2', 'b-chat-1'])
        expect(runtime.captureSelectedConversationTarget()).toMatchObject({ characterId: 'char-b', conversationId: 'b-chat-1' })
        expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
        await runtime.flushPendingDataLocally('after-conversation-order')
        expect(saveFailures).toEqual([])
    })

    it.each([true, false])('opens the first remaining conversation when the open one is removed (windowed=%s)', async (windowed) => {
        const { store, runtime, receive, saveFailures, atProjection } = await receiveRuntime(windowed, 1, 1, ['b-chat-1', 'b-chat-2', 'b-chat-3'])
        const remaining = (await store.readConversation('char-b', 'b-chat-1'))!.value
        const commit = vi.spyOn(store, 'commit')
        await receive(['["exists","conversation","char-b","b-chat-2"]'])

        expect(atProjection[0]).toEqual({ selected: 1, character: 'char-b', conversation: 'b-chat-1' })
        await vi.waitFor(() => {
            expect(runtime.captureSelectedConversationTarget()).toMatchObject({ characterId: 'char-b', conversationId: 'b-chat-1' })
        })
        expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
        await runtime.flushPendingDataLocally('after-open-conversation-removal')
        await receive([])
        expect(saveFailures).toEqual([])
        expect((await store.readConversation('char-b', 'b-chat-1'))!.value.message).toEqual(remaining.message)
        expect(await store.readConversationMetadata('char-b', 'b-chat-2')).toBeNull()
        // Besides the received removal, only the local position of the newly opened conversation is written.
        expect(commit.mock.calls.map(([input]) => input.unitMutations)).toEqual([
            [{ key: '["exists","conversation","char-b","b-chat-2"]', type: 'delete' }],
            [{ key: '["character","char-b","chatPage"]', type: 'set', value: 0 }],
        ])
    })

    it.each([
        ['character', '["exists","character","char-b"]', -1, undefined],
        ['conversation', '["exists","conversation","char-b","b-chat-1"]', 1, 'b-chat-2'],
    ] as const)('installs a valid selection when a committed refresh removes the selected %s', async (_scope, key, selected, conversation) => {
        for (const [windowed, targeted] of [[true, true], [true, false], [false, true], [false, false]]) {
            const { store, runtime, saveFailures } = await receiveRuntime(windowed, 1)
            // A targeted pass patches only a working set that a refresh projected.
            const projection = await runtime.acquireCommittedWorkingSetRefreshFence()
            try {
                await projection.refreshCommittedWorkingSet(runtime.revision)
            } finally {
                projection.release()
            }
            const { revision } = await store.commit({ expectedRevision: runtime.revision, unitMutations: [{ key, type: 'delete' }] })
            const fence = await runtime.acquireCommittedWorkingSetRefreshFence()
            try {
                // Without a change set this store has no change window, so the working set is reprojected.
                const changeSet = { root: false, presets: false, pluginStorage: false, wholeLibrary: false, characterIds: ['char-b'], conversations: [] }
                await expect(fence.refreshCommittedWorkingSet(revision, targeted ? { changeSet } : undefined)).resolves.toMatchObject({ projection: 'applied' })
            } finally {
                fence.release()
            }

            expect(get(selectedCharID)).toBe(selected)
            const current = getDatabase().characters[get(selectedCharID)]
            expect(current?.chats[current.chatPage ?? 0]?.id).toBe(conversation)
            expect(runtime.captureSelectedConversationTarget()?.conversationId).toBe(conversation)
            await runtime.flushPendingDataLocally('after-refresh-removal')
            expect(saveFailures).toEqual([])
            workingSetResidency.clear()
        }
    })
})

describe('detail mutations of a catalog character', () => {
    const character = (chaId: string, extra: Record<string, unknown> = {}) => ({ type: 'character', chaId, name: chaId, chatPage: 0, chatFolders: [], chats: [{ id: `${chaId}-chat`, name: 'Chat', note: '', localLore: [], message: [] }], ...extra })

    async function catalogRuntime(eviction: boolean, resident: readonly string[] = ['live'], live: Record<string, unknown> = {}) {
        workingSetResidency.setEvictionAllowed(eviction)
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas[0].id = 'persona'
        initial.characters = [
            character('live', { creatorNotes: 'Live notes', ...live }),
            character('trashed', { trashTime: 123, creatorNotes: 'Trashed notes', image: 'trashed-image' }),
            character('other', { creatorNotes: 'Other notes' }),
        ] as unknown as Database['characters']
        initial.characterOrder = ['live', 'other']
        const store = new IndexedDbPersistentDataStore(`catalog-detail-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        const { revision } = await store.replaceFromDatabase(initial)
        setDatabaseLite(projectCompleteScalableWorkingSet(initial, 'live', revision, new Set(resident)))
        // Bootstrap marks every stub it installs as released.
        for (const value of getDatabase().characters) if (isCatalogCharacterStub(value)) workingSetResidency.markCharacterReleased(value.chaId)
        selectedCharID.set(0)
        const saveFailures: unknown[] = []
        const runtime = createPersistentDataRuntime({
            store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) },
        })
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('catalog-detail-settle')
        for (const value of getDatabase().characters) expect(isCatalogCharacterStub(value)).toBe(!resident.includes(value.chaId))
        return { store, runtime, saveFailures }
    }

    async function receive(store: PersistentDataStore, runtime: Awaited<ReturnType<typeof catalogRuntime>>['runtime'], commit: Omit<WorkingSetCommit, 'expectedRevision'>, affectedKeys: string[]) {
        store.lwwStageReceive = async () => undefined
        store.lwwFinishReceive = async () => undefined
        store.lwwApplyReceive = async () => ({ ...await store.commit({ expectedRevision: runtime.revision, ...commit }), affectedKeys, heldKeys: [], deferredKeys: [] })
        await runtime.applyLwwReceive({ bindingAuthority: '1', requestId: crypto.randomUUID(), changes: [], progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' })
    }

    const editOther = (runtime: Awaited<ReturnType<typeof catalogRuntime>>['runtime']) =>
        runtime.mutatePersistentCharacterDetail('other', 'character-removal', ({ root, character }) => {
            removeCharacterIdFromOrder(root, 'other')
            character.trashTime = 456
            character.name = 'Renamed'
            character.image = 'other-image'
            character.creatorNotes = 'Edited notes'
            character.desc = 'Detail only'
        })

    async function expectEditedCatalogCharacter(store: PersistentDataStore, runtime: Awaited<ReturnType<typeof catalogRuntime>>['runtime'], saveFailures: unknown[]) {
        const edited = getDatabase().characters.find((value) => value.chaId === 'other')!
        expect(edited).toMatchObject({ trashTime: 456, name: 'Renamed', image: 'other-image', creatorNotes: 'Edited notes' })
        expect(edited).not.toHaveProperty('desc')
        expect(isCatalogCharacterStub(edited)).toBe(true)
        expect(getDatabase().characterOrder).toEqual(['live'])
        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('after-catalog-edit')
        expect(commit).not.toHaveBeenCalled()
        expect(saveFailures).toEqual([])
    }

    it.each([true, false])('restores a trashed catalog character into the live list (eviction=%s)', async (eviction) => {
        const { store, runtime, saveFailures } = await catalogRuntime(eviction)
        await runtime.mutatePersistentCharacterDetail('trashed', 'character-restore', ({ root, character }) => {
            delete character.trashTime
            appendCharacterIdToOrder(root, 'trashed')
        })

        const restored = getDatabase().characters.find((value) => value.chaId === 'trashed')!
        expect(restored).not.toHaveProperty('trashTime')
        expect(isCatalogCharacterStub(restored)).toBe(true)
        expect(getDatabase().characterOrder).toEqual(['live', 'other', 'trashed'])
        expect((await store.readCharacter('trashed'))!.value).not.toHaveProperty('trashTime')
        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('after-catalog-restore')
        expect(commit).not.toHaveBeenCalled()
        expect(saveFailures).toEqual([])
    })

    it.each([true, false])('publishes catalog fields of a detail edit to a catalog character (eviction=%s)', async (eviction) => {
        const { store, runtime, saveFailures } = await catalogRuntime(eviction)
        await editOther(runtime)

        await expectEditedCatalogCharacter(store, runtime, saveFailures)
    })

    it('publishes only catalog fields to a released character that kept its baseline', async () => {
        const { store, runtime, saveFailures } = await catalogRuntime(true, ['live', 'other'])
        expect(workingSetResidency.releaseCharacterToCatalog(getDatabase(), 'other')).toBe(true)
        await editOther(runtime)

        await expectEditedCatalogCharacter(store, runtime, saveFailures)
    })

    it('patches a received trash restore into a catalog character', async () => {
        const { store, runtime, saveFailures } = await catalogRuntime(true)
        const key = '["character","trashed","trashTime"]'
        await receive(store, runtime, { unitMutations: [{ key, type: 'delete' }] }, [key])

        expect(getDatabase().characters.find((value) => value.chaId === 'trashed')).not.toHaveProperty('trashTime')
        expect(saveFailures).toEqual([])
    })

    it('keeps received conversations and folders out of a character released to the catalog', async () => {
        const { store, runtime, saveFailures } = await catalogRuntime(true, ['live'], { chatFolders: undefined })
        expect(await runtime.activateCharacter('other')).toBe(true)
        const released = () => getDatabase().characters.find((value) => value.chaId === 'live')!
        expect(isCatalogCharacterStub(released())).toBe(true)
        const folders = [{ id: 'folder', name: 'Folder', folded: false }]
        await receive(store, runtime, {
            conversations: [{ type: 'replace-range', characterId: 'live', conversationId: 'live-chat-2', start: 0, deleteCount: 0, messages: [],
                conversation: { id: 'live-chat-2', name: 'Second', note: '', localLore: [] } as unknown as Omit<Database['characters'][number]['chats'][number], 'message'>, configuredIndex: 1 }],
            unitMutations: [{ key: '["order","conversations","live"]', type: 'set', value: { ids: ['live-chat', 'live-chat-2'], folders } }],
        }, ['["exists","conversation","live","live-chat-2"]', '["order","conversations","live"]'])

        expect((await store.readCharacter('live'))!.value.chatFolders).toEqual(folders)
        expect(await store.readConversationMetadata('live', 'live-chat-2')).not.toBeNull()
        expect(isCatalogCharacterStub(released())).toBe(true)
        expect(released().chats).toEqual([])
        expect(released()).not.toHaveProperty('chatFolders')
        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('after-released-receive')
        expect(commit).not.toHaveBeenCalled()
        expect(saveFailures).toEqual([])
    })

    it.each(['released', 'added'])('keeps no baseline for a character %s to the catalog', async (route) => {
        const { runtime } = await catalogRuntime(true)
        if (route === 'released') {
            expect(await runtime.activateCharacter('other')).toBe(true)
            expect(isCatalogCharacterStub(getDatabase().characters.find((value) => value.chaId === 'live')!)).toBe(true)
        } else {
            await runtime.upsertPersistentCompleteCharacter('added', 'add-catalog-character', () => character('added', { creatorNotes: 'Added notes' }) as unknown as Database['characters'][number])
            expect(isCatalogCharacterStub(getDatabase().characters.find((value) => value.chaId === 'added')!)).toBe(true)
        }
        const materialized = vi.spyOn(SaveCoordinator.prototype, 'captureMaterializedBaseline')
        try {
            await runtime.mutatePersistentCharacterDetail('trashed', 'character-restore', ({ root, character }) => {
                delete character.trashTime
                appendCharacterIdToOrder(root, 'trashed')
            })

            expect(materialized).toHaveBeenCalled()
            const resident = getDatabase().characters.filter((value) => !isCatalogCharacterStub(value)).map((value) => value.chaId)
            expect(materialized.mock.results.at(-1)!.value.map((value: Database['characters'][number]) => value.chaId)).toEqual(resident)
        } finally {
            materialized.mockRestore()
        }
    })
})

describe('preset list operations', () => {
    it.each([true, false])('stores whole presets added, copied, renamed and removed in place (catalog=%s)', async (catalog) => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'first-preset'
        initial.botPresets[0].name = 'First'
        initial.personas[0].id = 'persona'
        const store = new IndexedDbPersistentDataStore(`preset-list-${catalog}-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        const {revision} = await store.replaceFromDatabase(initial)
        setDatabaseLite(catalog ? projectCompleteScalableWorkingSet(initial, null, revision, new Set()) : structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const controller = createPresetWorkingSetController({
            getDatabase, captureCurrentPreset, getEffectivePresetId,
            applyPreset: (root, preset) => setPreset(root as Database, preset),
            mutatePersistentPresets: (reason, mutate) => runtime.mutatePersistentPresets(reason, mutate),
        })
        const stored = async () => (await store.queryPresets()).items.sort((a, b) => a.configuredIndex - b.configuredIndex).map((item) => item.name)
        const live = () => getDatabase().botPresets.map((value) => value.name)

        expect(await controller.addPreset({...structuredClone(initial.botPresets[0]), name: 'Added'})).toBe(1)
        expect(await stored()).toEqual(['First', 'Added'])
        expect(live()).toEqual(['First', 'Added'])
        expect(await controller.copyPreset(1)).toBe(2)
        expect(await stored()).toEqual(['First', 'Added', 'Added Copy'])
        expect(live()).toEqual(['First', 'Added', 'Added Copy'])
        await controller.renamePreset(2, 'Renamed')
        expect(await stored()).toEqual(['First', 'Added', 'Renamed'])
        expect(live()).toEqual(['First', 'Added', 'Renamed'])
        await controller.removePreset(1)
        expect(await stored()).toEqual(['First', 'Renamed'])
        expect(live()).toEqual(['First', 'Renamed'])
        expect(getDatabase().botPresetsId).toBe(0)
        const ids = (await store.queryPresets()).items.map((item) => item.id)
        expect(new Set(ids).size).toBe(2)
        expect(ids).toContain('first-preset')

        const commit = vi.spyOn(store, 'commit')
        await runtime.flushPendingDataLocally('after-preset-list-operations')
        expect(commit).not.toHaveBeenCalled()
    })
})

async function receiveCommit(store: PersistentDataStore, runtime: ReturnType<typeof createPersistentDataRuntime>,
    commit: Omit<WorkingSetCommit, 'expectedRevision'>, affectedKeys: string[], whileApplying?: () => void) {
    store.lwwStageReceive = async () => undefined
    store.lwwFinishReceive = async () => undefined
    store.lwwApplyReceive = async () => {
        whileApplying?.()
        return { ...await store.commit({ expectedRevision: runtime.revision, ...commit }), affectedKeys, heldKeys: [], deferredKeys: [] }
    }
    await runtime.applyLwwReceive({ bindingAuthority: '1', requestId: crypto.randomUUID(), changes: [], progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' })
}

describe('received record deletions', () => {
    async function recordRuntime() {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas = [{...initial.personas[0], id: 'persona-keep', name: 'Keep'}, {...initial.personas[0], id: 'persona-gone', name: 'Gone'}]
        initial.selectedPersona = 0
        initial.modules = [{id: 'module-keep', name: 'Keep', description: ''}, {id: 'module-gone', name: 'Gone', description: ''}]
        initial.loadouts = [{id: 'loadout-keep', name: 'Keep'}, {id: 'loadout-gone', name: 'Gone'}] as unknown as Database['loadouts']
        initial.customModels = [{id: 'model-keep', name: 'Keep'}, {id: 'model-gone', name: 'Gone'}] as unknown as Database['customModels']
        const store = new IndexedDbPersistentDataStore(`record-deletions-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const saveFailures: unknown[] = []
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) }})
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('record-deletions-settle')
        return {store, runtime, saveFailures}
    }

    it.each([
        ['modules', 'modules', 'module'], ['loadouts', 'loadouts', 'loadout'], ['customModels', 'customModels', 'model'], ['personas', 'persona', 'persona'],
    ].flatMap(([collection, kind, prefix]) => [false, true].map((edited) => ({collection, kind, prefix, edited}))) as {
        collection: 'modules' | 'loadouts' | 'customModels' | 'personas', kind: string, prefix: string, edited: boolean,
    }[])('removes a $collection record deleted on another device from the working set (edited during receive=$edited)', async ({collection, kind, prefix, edited}) => {
        const {store, runtime, saveFailures} = await recordRuntime()
        const records = () => getDatabase()[collection] as unknown as {id: string, name: string}[]
        const keys = [JSON.stringify(['exists', kind, `${prefix}-gone`]), JSON.stringify(['order', collection])]
        await receiveCommit(store, runtime, {unitMutations: [{key: keys[0], type: 'delete'}, {key: keys[1], type: 'set', value: [`${prefix}-keep`]}]}, keys,
            edited ? () => { records().find((value) => value.id === `${prefix}-gone`)!.name = 'Edited here' } : undefined)

        expect(records().map((value) => value.id)).toEqual([`${prefix}-keep`])
        const commit = vi.spyOn(store, 'commit')
        records()[0].name = 'Later edit'
        await runtime.flushPendingDataLocally('after-received-record-deletion')
        expect(commit).toHaveBeenCalledOnce()
        expect(JSON.stringify(commit.mock.calls[0][0])).not.toContain(`${prefix}-gone`)
        expect(saveFailures).toEqual([])
    })
})

describe('received persona replacement', () => {
    const persona = (id: string, name: string) => ({id, name, icon: '', personaPrompt: `${name} prompt`, note: '', largePortrait: false})
    async function personaRuntime() {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas = [persona('persona-b-old', 'Old B'), persona('persona-d-old', 'Old D')]
        initial.selectedPersona = 1
        const store = new IndexedDbPersistentDataStore(`received-persona-replacement-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const saveFailures: unknown[] = []
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) }})
        await runtime.initializeActiveWorkingSet(getDatabase())
        getDatabase().classicMaxWidth = true
        await runtime.flushPendingDataLocally('received-persona-replacement-settle')
        return {store, runtime, saveFailures}
    }
    const selected = () => getDatabase().personas[getDatabase().selectedPersona]
    async function savedMutations(store: PersistentDataStore, runtime: ReturnType<typeof createPersistentDataRuntime>) {
        const commit = vi.spyOn(store, 'commit')
        getDatabase().classicMaxWidth = !getDatabase().classicMaxWidth
        await runtime.flushPendingDataLocally('after-received-personas')
        const mutations = commit.mock.calls.flatMap(([input]) => [...input.rootMutations ?? [], ...input.unitMutations ?? []])
        commit.mockRestore()
        return mutations
    }

    it('keeps a live persona selected and writes no selection while a replacement arrives in key-ordered receives', async () => {
        const {store, runtime, saveFailures} = await personaRuntime()
        const added = [persona('persona-c-new', 'New C'), persona('persona-a-new', 'New A')]
        const noSelectionWrite = async () => expect((await savedMutations(store, runtime)).map((value) => value.key)).toEqual(['classicMaxWidth'])
        // Units page in key order, so a replacement's membership units arrive in
        // an earlier receive than the fields and the selection.
        const membership = ['persona-a-new', 'persona-b-old', 'persona-c-new', 'persona-d-old'].map((id) => JSON.stringify(['exists', 'persona', id]))
        await receiveCommit(store, runtime, {unitMutations: membership.map((key) => key.includes('-old')
            ? {key, type: 'delete' as const} : {key, type: 'set' as const, value: true})}, membership)
        expect(getDatabase().personas.map((value) => value.id).sort()).toEqual(['persona-a-new', 'persona-c-new'])
        expect(selected()).toBeDefined()
        await noSelectionWrite()

        const fields = added.flatMap((value) => Object.entries(value).filter(([field]) => field !== 'id')
            .map(([field, fieldValue]) => ({key: JSON.stringify(['persona', value.id, field]), type: 'set' as const, value: fieldValue})))
        const order = {key: JSON.stringify(['order', 'personas']), type: 'set' as const, value: added.map((value) => value.id)}
        await receiveCommit(store, runtime, {unitMutations: [...fields, order]}, [order.key, ...fields.map((value) => value.key)].sort())
        expect(getDatabase().personas).toEqual(added)
        expect(added).toContainEqual(selected())
        await noSelectionWrite()

        const selection = {key: JSON.stringify(['root', 'selectedPersona']), type: 'set' as const, value: 'persona-a-new'}
        await receiveCommit(store, runtime, {unitMutations: [selection]}, [selection.key])
        expect(selected()).toEqual(added[1])
        expect(getDatabase().username).toBe('New A')
        await noSelectionWrite()
        expect((await store.readRoot()).value.selectedPersona).toBe('persona-a-new')
        expect(saveFailures).toEqual([])
    })

    it('does not write back a fallback for a received selection naming a persona this device does not hold', async () => {
        const {store, runtime, saveFailures} = await personaRuntime()
        const selection = {key: JSON.stringify(['root', 'selectedPersona']), type: 'set' as const, value: 'persona-elsewhere'}
        await receiveCommit(store, runtime, {unitMutations: [selection]}, [selection.key])
        expect(selected()).toBeDefined()
        const mutations = await savedMutations(store, runtime)
        expect(mutations.map((value) => value.key)).toEqual(['classicMaxWidth'])
        expect((await store.readRoot()).value.selectedPersona).toBe('persona-elsewhere')
        expect(saveFailures).toEqual([])
    })

    it('does not write back an index for a received selection after every persona was removed', async () => {
        const {store, runtime, saveFailures} = await personaRuntime()
        const membership = ['persona-b-old', 'persona-d-old'].map((id) => JSON.stringify(['exists', 'persona', id]))
        await receiveCommit(store, runtime, {unitMutations: membership.map((key) => ({key, type: 'delete' as const}))}, membership)
        const selection = {key: JSON.stringify(['root', 'selectedPersona']), type: 'set' as const, value: 'persona-elsewhere'}
        await receiveCommit(store, runtime, {unitMutations: [selection]}, [selection.key])
        expect(getDatabase().personas).toEqual([])
        const mutations = await savedMutations(store, runtime)
        expect(mutations.map((value) => value.key)).toEqual(['classicMaxWidth'])
        expect((await store.readRoot()).value.selectedPersona).toBe('persona-elsewhere')
        expect(saveFailures).toEqual([])
    })
})

describe('received preset selection', () => {
    async function presetRuntime(catalog: boolean) {
        const initial = normalizeDatabaseDefaults({} as Database)
        const preset = initial.botPresets[0]
        initial.botPresets = [{...structuredClone(preset), id: 'preset-a', name: 'A'}, {...structuredClone(preset), id: 'preset-b', name: 'B'}]
        initial.botPresetsId = 1
        initial.personas[0].id = 'persona'
        const store = new IndexedDbPersistentDataStore(`received-preset-selection-${catalog}-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        const {revision} = await store.replaceFromDatabase(initial)
        setDatabaseLite(catalog ? projectCompleteScalableWorkingSet(initial, null, revision) : structuredClone(initial))
        selectedCharID.set(-1)
        const saveFailures: unknown[] = []
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) }})
        await runtime.initializeActiveWorkingSet(getDatabase())
        getDatabase().classicMaxWidth = true
        await runtime.flushPendingDataLocally('received-preset-selection-settle')
        return {store, runtime, saveFailures}
    }
    async function savedMutations(store: PersistentDataStore, runtime: ReturnType<typeof createPersistentDataRuntime>) {
        const commit = vi.spyOn(store, 'commit')
        getDatabase().classicMaxWidth = !getDatabase().classicMaxWidth
        await runtime.flushPendingDataLocally('after-received-preset-selection')
        const mutations = commit.mock.calls.flatMap(([input]) => [...input.rootMutations ?? [], ...input.unitMutations ?? []])
        commit.mockRestore()
        return mutations
    }

    it.each([false, true])('does not write back a fallback for a received selection naming a preset this device does not hold (catalog=%s)', async (catalog) => {
        const {store, runtime, saveFailures} = await presetRuntime(catalog)
        const selection = {key: JSON.stringify(['root', 'botPresetsId']), type: 'set' as const, value: 'preset-elsewhere'}
        await receiveCommit(store, runtime, {unitMutations: [selection]}, [selection.key])
        expect(getDatabase().botPresets[getDatabase().botPresetsId]).toBeDefined()
        const mutations = await savedMutations(store, runtime)
        expect(mutations.map((value) => value.key)).toEqual(['classicMaxWidth'])
        expect((await store.readRoot()).value.botPresetsId).toBe('preset-elsewhere')
        expect(saveFailures).toEqual([])
    })

    it.each([false, true])('stores a local selection made after an unresolved received selection (catalog=%s)', async (catalog) => {
        const {store, runtime, saveFailures} = await presetRuntime(catalog)
        const selection = {key: JSON.stringify(['root', 'botPresetsId']), type: 'set' as const, value: 'preset-elsewhere'}
        await receiveCommit(store, runtime, {unitMutations: [selection]}, [selection.key])
        getDatabase().botPresetsId = getDatabase().botPresets.findIndex((value) => value.id === 'preset-b')
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingDataLocally('after-local-preset-selection')
        expect((await store.readRoot()).value.botPresetsId).toBe('preset-b')
        expect(saveFailures).toEqual([])
    })
})

describe('received unit projection', () => {
    it('reports no background error when the content change cursor cannot be committed', async () => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas[0].id = 'persona'
        const store = new IndexedDbPersistentDataStore(`cursor-commit-failure-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const backgroundErrors: unknown[] = []
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onBackgroundError: (error) => { backgroundErrors.push(error) }})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const cursor = vi.fn(async () => { throw new Error('database is locked') })
        store.commitWorkingSetChangeCursor = cursor
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
        try {
            const key = JSON.stringify(['root', 'language'])
            await receiveCommit(store, runtime, {unitMutations: [{key, type: 'set', value: 'ko'}]}, [key])
            expect(getDatabase().language).toBe('ko')
            expect(cursor).toHaveBeenCalled()
            expect(backgroundErrors).toEqual([])
            expect(warn).toHaveBeenCalled()
        } finally {
            warn.mockRestore()
        }
    })
})

describe('received root fields without a value', () => {
    async function rootRuntime(catalog: boolean) {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas[0].id = 'persona'
        initial.askRemoval = false
        initial.sendWithEnter = false
        initial.animationSpeed = 0.8
        const store = new IndexedDbPersistentDataStore(`received-root-defaults-${catalog}-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        const {revision} = await store.replaceFromDatabase(initial)
        setDatabase(catalog ? projectCompleteScalableWorkingSet(structuredClone(initial), null, revision) : structuredClone(initial))
        selectedCharID.set(-1)
        const saveFailures: unknown[] = []
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) }})
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('received-root-defaults-settle')
        return {store, runtime, saveFailures}
    }
    async function savedKeys(store: PersistentDataStore, runtime: ReturnType<typeof createPersistentDataRuntime>) {
        const commit = vi.spyOn(store, 'commit')
        getDatabase().classicMaxWidth = !getDatabase().classicMaxWidth
        await runtime.flushPendingDataLocally('after-received-root-defaults')
        const keys = commit.mock.calls.flatMap(([input]) => [...input.rootMutations ?? [], ...input.unitMutations ?? []]).map((value) => value.key)
        commit.mockRestore()
        return keys
    }

    it.each([false, true])('normalizes present received values like a load without writing corrections back (catalog=%s)', async (catalog) => {
        const {store, runtime, saveFailures} = await rootRuntime(catalog)
        const values = { promptInfoInsideChat: true, chatLoadInitialPages: -10, chatLoadAdditionalPages: 100000,
            risunestInlayFormat: 'invalid', risunestInlayWebpQuality: -1, risunestInlayMaxDimension: -100,
            risunestInlayAnimationMaxFps: -10, loreBookPage: 9999 }
        const expected = normalizeDatabaseDefaults({...JSON.parse(JSON.stringify(getDatabase())), ...values} as unknown as Database)
        const mutations = Object.entries(values).map(([field, value]) => ({key: JSON.stringify(['root', field]), type: 'set' as const, value}))
        await receiveCommit(store, runtime, {unitMutations: mutations}, mutations.map(({key}) => key))
        for (const field of Object.keys(values)) expect(getDatabase()[field]).toEqual(expected[field])
        expect(await savedKeys(store, runtime)).toEqual(['classicMaxWidth'])
        const stored = (await store.readRoot()).value
        for (const [field, value] of Object.entries(values)) expect(stored[field]).toEqual(value)
        expect(saveFailures).toEqual([])
    })

    it('normalizes an older string-shaped root provider while leaving unrelated fields unchanged', () => {
        const database = normalizeDatabaseDefaults({} as Database)
        Object.assign(database, {openrouterProvider: 'synthetic-provider', chatLoadInitialPages: -10, pluginCustomStorage: {synthetic: 'kept'}})
        fillRootFieldDefaults(database, ['openrouterProvider', 'pluginCustomStorage'])
        expect(database.openrouterProvider).toEqual({order: ['synthetic-provider'], only: [], ignore: []})
        expect(database.chatLoadInitialPages).toBe(-10)
        expect(database.pluginCustomStorage).toEqual({synthetic: 'kept'})
    })

    it.each([false, true].flatMap((catalog) => (['askRemoval', 'sendWithEnter'] as const).map((field) => ({catalog, field}))))(
        'gives a removed $field the value a load gives it without saving it (catalog=$catalog)', async ({catalog, field}) => {
            const {store, runtime, saveFailures} = await rootRuntime(catalog)
            const key = JSON.stringify(['root', field])
            await receiveCommit(store, runtime, {unitMutations: [{key, type: 'delete'}]}, [key])
            expect(getDatabase()[field]).toBe(normalizeDatabaseDefaults({} as Database)[field])
            expect(await savedKeys(store, runtime)).toEqual(['classicMaxWidth'])
            expect((await store.readRoot()).value[field]).toBeUndefined()
            expect(saveFailures).toEqual([])
        })

    it.each([false, true])('keeps a local edit made while a removal is received (catalog=%s)', async (catalog) => {
        const {store, runtime, saveFailures} = await rootRuntime(catalog)
        const key = JSON.stringify(['root', 'animationSpeed'])
        await receiveCommit(store, runtime, {unitMutations: [{key, type: 'delete'}]}, [key], () => { getDatabase().animationSpeed = 1.2 })
        expect(getDatabase().animationSpeed).toBe(1.2)
        expect(await savedKeys(store, runtime)).toEqual(['animationSpeed', 'classicMaxWidth'])
        expect((await store.readRoot()).value.animationSpeed).toBe(1.2)
        expect(saveFailures).toEqual([])
    })
})

describe('received fields the working set did not have', () => {
    async function fieldRuntime() {
        const initial = normalizeDatabaseDefaults({} as Database)
        const preset = initial.botPresets[0]
        initial.botPresets = [{...structuredClone(preset), id: 'preset-selected', name: 'Selected'}, {...structuredClone(preset), id: 'preset-other', name: 'Other'}]
        delete initial.botPresets[1].globalNote
        initial.botPresetsId = 0
        const persona = {id: 'persona-selected', name: 'Selected', icon: '', personaPrompt: '', note: ''}
        initial.personas = [persona, {...persona, id: 'persona-other', name: 'Other'}]
        initial.selectedPersona = 0
        const store = new IndexedDbPersistentDataStore(`received-new-fields-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const saveFailures: unknown[] = []
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value,
            onLocalSaveFailure: (error) => { if (error !== null) saveFailures.push(error) }})
        await runtime.initializeActiveWorkingSet(getDatabase())
        // A save reads every capture once, as the app does before any receive.
        getDatabase().classicMaxWidth = true
        await runtime.flushPendingDataLocally('received-new-fields-settle')
        return {store, runtime, saveFailures}
    }

    it.each([
        ['persona-selected', '["persona","persona-selected","largePortrait"]', true],
        ['persona-other', '["persona","persona-other","largePortrait"]', true],
        ['preset-other', '["preset","preset-other","globalNote"]', 'Received note'],
    ] as const)('keeps a field %s received for the first time', async (id, key, value) => {
        const {store, runtime, saveFailures} = await fieldRuntime()
        const field = JSON.parse(key)[2] as string
        const live = () => (id.startsWith('persona') ? getDatabase().personas : getDatabase().botPresets).find((item) => item.id === id) as unknown as Record<string, unknown>
        expect(live()).not.toHaveProperty(field)
        await receiveCommit(store, runtime, {unitMutations: [{key, type: 'set', value}]}, [key])

        expect(live()[field]).toEqual(value)
        const commit = vi.spyOn(store, 'commit')
        getDatabase().classicMaxWidth = false
        await runtime.flushPendingDataLocally('after-received-new-field')
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].unitMutations ?? []).toEqual([])
        expect(commit.mock.calls[0][0].rootMutations).toEqual([{type: 'set', key: 'classicMaxWidth', value: false}])
        const stored = id.startsWith('persona')
            ? (await store.readRoot()).value.personas!.find((item) => item.id === id) as unknown as Record<string, unknown>
            : (await store.readPreset(id))!.value as unknown as Record<string, unknown>
        expect(stored[field]).toEqual(value)
        expect(saveFailures).toEqual([])
    })
})

describe('local global toggle edits', () => {
    it.each(['toggle_new', 'toggle_existing'])('stores %s', async (name) => {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas[0].id = 'persona'
        initial.globalChatVariables = {toggle_existing: '1'}
        initial.explicitGlobalChatVariables = {toggle_existing: '1'}
        const store = new IndexedDbPersistentDataStore(`local-toggle-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        // A save reads every capture once, as the app does before the toggle changes.
        getDatabase().classicMaxWidth = true
        await runtime.flushPendingDataLocally('local-toggle-settle')

        getDatabase().globalChatVariables[name] = '2'
        await runtime.flushPendingDataLocally('local-toggle')

        expect((await store.readRoot()).value.explicitGlobalChatVariables?.[name]).toBe('2')
    })
})

describe('received plugin records', () => {
    const plugin = (name: string, extra: Record<string, unknown> = {}) => ({name, displayName: name, script: `// ${name}`,
        arguments: {limit: 'int'}, realArg: {limit: 1}, version: '3.0', customLink: [], argMeta: {}, enabled: true, ...extra})
    const record = (name: string, value?: ReturnType<typeof plugin>): PersistentUnitMutation =>
        value ? {key: JSON.stringify(['record', 'plugins', name]), type: 'set', value} : {key: JSON.stringify(['record', 'plugins', name]), type: 'delete'}

    async function pluginHarness() {
        const initial = normalizeDatabaseDefaults({} as Database)
        initial.botPresets[0].id = 'preset'
        initial.personas[0].id = 'persona'
        initial.plugins = [plugin('plugin-a'), plugin('plugin-b')] as unknown as Database['plugins']
        const store = new IndexedDbPersistentDataStore(`received-plugins-${crypto.randomUUID()}`, new IDBFactory(), IDBKeyRange) as PersistentDataStore
        await store.open()
        await store.replaceFromDatabase(initial)
        setDatabase(structuredClone(initial))
        selectedCharID.set(-1)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        await runtime.flushPendingDataLocally('received-plugins-settle')
        pluginRuntime.load.mockClear()
        return {store, runtime}
    }

    it.each([
        ['an added plugin', [record('plugin-c', plugin('plugin-c'))], ['plugin-a', 'plugin-b', 'plugin-c']],
        ['a removed plugin', [record('plugin-b')], ['plugin-a']],
        ['a disabled plugin', [record('plugin-a', plugin('plugin-a', {enabled: false}))], ['plugin-a', 'plugin-b']],
        ['a changed script', [record('plugin-a', plugin('plugin-a', {script: '// changed'}))], ['plugin-a', 'plugin-b']],
        ['a changed API version', [record('plugin-a', plugin('plugin-a', {version: '2.1'}))], ['plugin-a', 'plugin-b']],
        ['changed argument values', [record('plugin-a', plugin('plugin-a', {realArg: {limit: 2}}))], ['plugin-a', 'plugin-b']],
        ['changed arguments', [record('plugin-a', plugin('plugin-a', {arguments: {limit: 'string'}}))], ['plugin-a', 'plugin-b']],
    ] as const)('requests an idle plugin reload once after receiving %s', async (_, mutations, names) => {
        const {store, runtime} = await pluginHarness()
        await receiveCommit(store, runtime, {unitMutations: [...mutations]}, mutations.map((value) => value.key))
        await vi.dynamicImportSettled()

        expect(getDatabase().plugins.map((value) => value.name)).toEqual(names)
        for (const mutation of mutations) if (mutation.type === 'set') expect(getDatabase().plugins.find((value) => value.name === (mutation.value as {name: string}).name)).toEqual(mutation.value)
        expect(pluginRuntime.load).toHaveBeenCalledOnce()
    })

    it('does not reload plugins for receives that leave what plugins run unchanged', async () => {
        const {store, runtime} = await pluginHarness()
        const unchanged = async (commit: Omit<WorkingSetCommit, 'expectedRevision'>, keys: string[]) => {
            await receiveCommit(store, runtime, commit, keys)
            await vi.dynamicImportSettled()
            expect(pluginRuntime.load).not.toHaveBeenCalled()
        }

        const renamed = record('plugin-a', plugin('plugin-a', {displayName: 'Renamed'}))
        await unchanged({unitMutations: [renamed]}, [renamed.key])
        expect(getDatabase().plugins[0].displayName).toBe('Renamed')
        await unchanged({unitMutations: [{key: '["order","plugins"]', type: 'set', value: ['plugin-b', 'plugin-a']}]}, ['["order","plugins"]'])
        expect(getDatabase().plugins.map((value) => value.name)).toEqual(['plugin-b', 'plugin-a'])
        await unchanged({pluginStorage: [{type: 'set', owner: 'plugin-a', key: 'counter', value: 1}]}, ['["plugin","plugin-a","counter"]'])
        await unchanged({unitMutations: [{key: '["root","classicMaxWidth"]', type: 'set', value: true}]}, ['["root","classicMaxWidth"]'])
        expect(getDatabase().classicMaxWidth).toBe(true)
    })

    it('leaves a local plugin change to the toggle that made it', async () => {
        const {runtime} = await pluginHarness()
        getDatabase().plugins[0].enabled = false
        await runtime.flushPendingDataLocally('local-plugin-toggle')
        await vi.dynamicImportSettled()

        expect(pluginRuntime.load).not.toHaveBeenCalled()
    })
})
