import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { RevisionConflictError, type PersistentDataStore } from '../persistentDataStore'
import { bootstrapPersistentDatabase } from '../persistentBootstrap'
import {
    createCatalogPresetWorkingSet,
    getCatalogConversationCount,
    isCatalogCharacterStub,
    projectCatalogWorkingSet,
} from '../workingSetCatalog'
import { canonicalJson } from '../saveCoordinator'
import { fixtureDatabase } from './persistentDataFixtures'

const scheduling = vi.hoisted(() => ({
    yieldToMainThread: vi.fn(async () => undefined),
}))

vi.mock('../../ui/yieldToUi', () => ({
    yieldToMainThread: scheduling.yieldToMainThread,
}))

function preparedResult(input: Database, output: Database) {
    return {
        database: structuredClone(output),
        changed: canonicalJson(input) !== canonicalJson(output),
    }
}

function plugin(version: '2.1', enabled: boolean): Database['plugins'][number] {
    return { enabled, version } as Database['plugins'][number]
}

function createStore(input?: {
    revision?: number
    database?: Database
    replacementRevision?: number
}): PersistentDataStore {
    const revision = input?.revision ?? 0
    const database = structuredClone(input?.database ?? fixtureDatabase)
    return {
        open: vi.fn(async () => undefined),
        queryPresets: vi.fn(async () => ({ revision: input?.revision ?? 7, items: [] })),
        readPreset: vi.fn(async () => null),
        readRoot: vi.fn(async () => ({
            revision,
            value: Object.fromEntries(
                Object.entries(database).filter(
                    ([key]) => key !== 'characters' && key !== 'botPresets',
                ),
            ) as Omit<Database, 'characters' | 'botPresets'>,
        })),
        materializeDatabase: vi.fn(async () => structuredClone(database)),
        replaceFromDatabase: vi.fn(async () => ({
            revision: input?.replacementRevision ?? revision + 1,
        })),
        queryCharacters: vi.fn(),
        readCharacter: vi.fn(),
        queryConversations: vi.fn(),
        readConversation: vi.fn(),
        readConversationMetadata: vi.fn(),
        readConversationWindow: vi.fn(),
        queryPluginStorage: vi.fn(async () => ({ revision, items: [] })),
        readPluginStorage: vi.fn(async () => null),
        readAssetAlias: vi.fn(async () => null),
        readAssetAliasesByKeys: vi.fn(async () => ({ revision, value: [] })),
        listAssetAliases: vi.fn(async () => ({ revision, items: [] })),
        readAssetRepositoryAuthority: vi.fn(async () => ({
            revision,
            value: { format: 'legacy' as const },
        })),
        readAssetOwnerHead: vi.fn(async () => null),
        readColdPayloadAuthority: vi.fn(async () => ({
            revision,
            value: { format: 'legacy' as const },
        })),
        readColdAlias: vi.fn(async () => null),
        listColdAliases: vi.fn(async () => ({ revision, value: [] })),
        commitAssetAlias: vi.fn(),
        deleteAssetAlias: vi.fn(),
        activateAssetRepositoryMigration: vi.fn(),
        commitColdAlias: vi.fn(),
        deleteColdAlias: vi.fn(),
        activateColdPayloadMigration: vi.fn(),
        commit: vi.fn(),
        acquireRevision: vi.fn(),
    }
}

describe('bootstrapPersistentDatabase', () => {
    beforeEach(() => {
        scheduling.yieldToMainThread.mockReset()
        scheduling.yieldToMainThread.mockResolvedValue(undefined)
    })

    it('starts a blank store from one prepared empty database', async () => {
        const store = createStore({ revision: 0, replacementRevision: 1 })
        const prepared = structuredClone(fixtureDatabase)
        const prepareDatabase = vi.fn(async (input) =>
            preparedResult(input, prepared),
        )

        const result = await bootstrapPersistentDatabase({ store, prepareDatabase })

        expect(prepareDatabase).toHaveBeenCalledTimes(1)
        expect(prepareDatabase).toHaveBeenCalledWith({
            streamingDisplayOptimizationMode: 'balanced',
        })
        expect(store.materializeDatabase).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(prepared, 0)
        expect(result).toEqual({ database: prepared, revision: 1, profile: 'scalable-v3' })
    })

    it('reads a nonblank persistent revision without rewriting it', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.username = 'Persistent user'
        persistent.plugins = [plugin('2.1', true)]
        persistent.streamingDisplayOptimizationMode = 'off'
        const store = createStore({ revision: 7, database: persistent })

        const result = await bootstrapPersistentDatabase({
            store,
            prepareDatabase: async (database) =>
                preparedResult(database, database),
        })

        expect(store.materializeDatabase).toHaveBeenCalledWith(7)
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(result).toEqual({
            database: persistent,
            revision: 7,
            profile: 'maximum-compatibility',
        })
    })

    it('awaits visible storage and compatibility phases before their expensive work', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', true)]
        persistent.language = 'ko'
        const store = createStore({ revision: 7, database: persistent })
        const events: string[] = []
        vi.mocked(store.open).mockImplementation(async () => {
            events.push('open')
        })
        vi.mocked(store.materializeDatabase).mockImplementation(async () => {
            events.push('materialize')
            return persistent
        })
        await bootstrapPersistentDatabase({
            store,
            onPhase: async (phase, locale) => {
                await Promise.resolve()
                if (phase !== 'storage') expect(locale).toBe('ko')
                events.push(phase)
            },
            prepareDatabase: async (database) =>
                preparedResult(database, database),
        })
        expect(events).toEqual([
            'storage',
            'open',
            'data',
            'compatibility',
            'materialize',
        ])
    })

    it('stores exactly one preparation change to persistent data', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', true)]
        const changed = structuredClone(persistent)
        changed.username = 'Normalized user'
        const store = createStore({ revision: 4, database: persistent, replacementRevision: 5 })

        const result = await bootstrapPersistentDatabase({
            store,
            prepareDatabase: async (database) =>
                preparedResult(database, changed),
        })

        expect(store.replaceFromDatabase).toHaveBeenCalledTimes(1)
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(changed, 4)
        expect(result).toEqual({
            database: changed,
            revision: 5,
            profile: 'maximum-compatibility',
        })
    })

    it('boots a scalable nonblank revision from root, preset, and catalog queries only', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-scalable-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', false)]
        persistent.botPresetsId = 1
        persistent.pluginCustomStorage = {
            'plugin-memory': { entries: [1, 2, 3] },
        }
        await store.open()
        await store.replaceFromDatabase(persistent)
        const materializeDatabase = vi.spyOn(store, 'materializeDatabase')
        const readPreset = vi.spyOn(store, 'readPreset')
        const prepareDatabase = vi.fn(async () => {
            throw new Error('Full database preparation must not run in scalable boot')
        })
        const projectScalableWorkingSet = vi.fn((input) =>
            projectCatalogWorkingSet(
                input.root,
                input.characters,
                createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset),
            ),
        )

        const result = await bootstrapPersistentDatabase({
            store,
            now: () => 500,
            prepareDatabase,
            prepareRoot: async (root) => structuredClone(root),
            projectScalableWorkingSet,
        })

        expect(materializeDatabase).not.toHaveBeenCalled()
        expect(prepareDatabase).not.toHaveBeenCalled()
        expect(readPreset).toHaveBeenCalledOnce()
        expect(readPreset).toHaveBeenCalledWith('1')
        expect(projectScalableWorkingSet).toHaveBeenCalledWith(expect.objectContaining({
            activePreset: {
                summary: expect.objectContaining({ id: '1', configuredIndex: 1 }),
                value: persistent.botPresets[1],
            },
            presetCatalog: expect.objectContaining({
                items: expect.arrayContaining([
                    expect.objectContaining({ id: '0', configuredIndex: 0 }),
                    expect.objectContaining({ id: '1', configuredIndex: 1 }),
                ]),
            }),
        }))
        expect(result.revision).toBe(1)
        expect(result.profile).toBe('scalable-v3')
        expect(result.database.botPresets[0]).toEqual({
            name: persistent.botPresets[0].name,
            image: persistent.botPresets[0].image,
        })
        expect(result.database.botPresets[1]).toEqual(persistent.botPresets[1])
        expect(result.database.characters.map((character) => character.chaId)).toEqual([
            'char-b',
            'char-a',
            'char-c',
        ])
        expect(result.database.characters.every(isCatalogCharacterStub)).toBe(true)
        expect(result.database.characters.map((character) => character.chats)).toEqual([
            [],
            [],
            [],
        ])
        expect(result.database.characters.map(getCatalogConversationCount)).toEqual([1, 2, 1])
        expect(result.database.pluginCustomStorage).toEqual({})
        expect((await store.readPluginStorage('plugin-memory'))?.value).toEqual(
            persistent.pluginCustomStorage['plugin-memory'],
        )
    })

    it('yields between character catalog pages without delaying every summary', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', false)]
        persistent.botPresetsId = -1
        const store = createStore({ revision: 7, database: persistent })
        const events: string[] = []
        scheduling.yieldToMainThread.mockImplementation(async () => {
            events.push('yield')
        })
        vi.mocked(store.queryCharacters).mockImplementation(async (query) => {
            events.push(
                `${query.trash ? 'trash' : 'active'}:${query.cursor ?? 'first'}`,
            )
            if (!query.trash && query.cursor === undefined) {
                return { revision: 7, items: [], nextCursor: 'active-page-2' }
            }
            return { revision: 7, items: [] }
        })

        await bootstrapPersistentDatabase({
            store,
            prepareDatabase: async (database) =>
                preparedResult(database, database),
            prepareRoot: async (root) => structuredClone(root),
            projectScalableWorkingSet: () => structuredClone(fixtureDatabase),
        })

        expect(events).toEqual([
            'active:first',
            'yield',
            'active:active-page-2',
            'yield',
            'trash:first',
        ])
        expect(scheduling.yieldToMainThread).toHaveBeenCalledTimes(2)
    })

    it('rejects a stale catalog page returned after a cooperative yield', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', false)]
        persistent.botPresetsId = -1
        const store = createStore({ revision: 7, database: persistent })
        vi.mocked(store.queryCharacters).mockImplementation(async (query) => {
            if (!query.trash && query.cursor === undefined) {
                return { revision: 7, items: [], nextCursor: 'active-page-2' }
            }
            return { revision: 8, items: [] }
        })

        await expect(
            bootstrapPersistentDatabase({
                store,
                prepareDatabase: async (database) =>
                    preparedResult(database, database),
                prepareRoot: async (root) => structuredClone(root),
                projectScalableWorkingSet: () =>
                    structuredClone(fixtureDatabase),
            }),
        ).rejects.toBeInstanceOf(RevisionConflictError)

        expect(scheduling.yieldToMainThread).toHaveBeenCalledOnce()
    })

    it('canonicalizes an out-of-range scalable preset selection before hydration', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-scalable-invalid-preset-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', false)]
        persistent.botPresetsId = 99
        await store.open()
        await store.replaceFromDatabase(persistent)
        const readPreset = vi.spyOn(store, 'readPreset')

        const result = await bootstrapPersistentDatabase({
            store,
            now: () => 500,
            prepareDatabase: async () => {
                throw new Error('Scalable boot must not prepare a complete database')
            },
            prepareRoot: async (root) => structuredClone(root),
            projectScalableWorkingSet: (input) => projectCatalogWorkingSet(
                input.root,
                input.characters,
                createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset),
            ),
        })

        expect(result.revision).toBe(2)
        expect(result.database.botPresetsId).toBe(0)
        expect(result.database.botPresets[0]).toEqual(persistent.botPresets[0])
        expect(result.database.botPresets[1]).toEqual({
            name: persistent.botPresets[1].name,
            image: persistent.botPresets[1].image,
        })
        expect(readPreset).toHaveBeenCalledOnce()
        expect(readPreset).toHaveBeenCalledWith('0')
        expect((await store.readRoot()).value.botPresetsId).toBe(0)
    })

    it('projects a newly prepared scalable database after committing the complete import', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-new-scalable-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const prepared = structuredClone(fixtureDatabase)
        prepared.plugins = [plugin('2.1', false)]
        prepared.botPresetsId = 1

        const result = await bootstrapPersistentDatabase({
            store,
            now: () => 500,
            prepareDatabase: async (database) =>
                preparedResult(database, prepared),
            projectScalableWorkingSet: (input) => projectCatalogWorkingSet(
                input.root,
                input.characters,
                createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset),
            ),
        })

        expect(result.profile).toBe('scalable-v3')
        expect(await store.materializeDatabase(result.revision)).toEqual(prepared)
        expect(result.database.botPresets[0]).toEqual({
            name: prepared.botPresets[0].name,
            image: prepared.botPresets[0].image,
        })
        expect(result.database.botPresets[1]).toEqual(prepared.botPresets[1])
        expect(result.database.characters.every(isCatalogCharacterStub)).toBe(true)
    })

    it('expires scalable trash by stable ID and requeries the final catalog', async () => {
        const day = 24 * 60 * 60 * 1000
        const now = 10 * day
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            `bootstrap-trash-expiry-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', false)]
        persistent.characters[2].trashTime = now - 4 * day
        const recentTrash = structuredClone(persistent.characters[2])
        recentTrash.chaId = 'char-recent-trash'
        recentTrash.name = 'Recent trash'
        recentTrash.trashTime = now - 2 * day
        recentTrash.chats[0].id = 'conv-recent-trash'
        persistent.characters.push(recentTrash)
        persistent.characterOrder = [
            'char-b',
            'char-c',
            { id: 'folder', name: 'Folder', color: '', data: ['char-c', 'char-a'] },
        ]
        await store.open()
        await store.replaceFromDatabase(persistent)
        const queryCharacters = vi.spyOn(store, 'queryCharacters')

        const result = await bootstrapPersistentDatabase({
            store,
            now: () => now,
            prepareDatabase: async () => {
                throw new Error('Scalable boot must not materialize the database')
            },
            prepareRoot: async (root) => structuredClone(root),
            projectScalableWorkingSet: (input) => projectCatalogWorkingSet(
                input.root,
                input.characters,
                createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset),
            ),
        })

        expect(result.database.characters.map((character) => character.chaId))
            .not.toContain('char-c')
        expect(result.database.characters.map((character) => character.chaId))
            .toContain('char-recent-trash')
        expect(result.database.characterOrder).toEqual([
            'char-b',
            { id: 'folder', name: 'Folder', color: '', data: ['char-a'] },
        ])
        expect(queryCharacters).toHaveBeenCalledTimes(4)
        const stored = await store.materializeDatabase(result.revision)
        expect(stored.characters.map((character) => character.chaId)).not.toContain('char-c')
        expect(stored.characters.find((character) => character.chaId === 'char-a')?.chats)
            .toEqual(persistent.characters[1].chats)
    })

    it('reports a conflict when another connection writes the blank store first', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-blank-conflict-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const rival = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await rival.open()

        await expect(
            bootstrapPersistentDatabase({
                store,
                prepareDatabase: async (database) => {
                    await rival.replaceFromDatabase(structuredClone(fixtureDatabase))
                    return preparedResult(database, fixtureDatabase)
                },
            }),
        ).rejects.toBeInstanceOf(RevisionConflictError)
    })

    it('reports a conflict when another connection writes during normalization', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-normalize-conflict-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const rival = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await rival.open()
        const persistent = structuredClone(fixtureDatabase)
        persistent.plugins = [plugin('2.1', true)]
        await store.replaceFromDatabase(persistent)

        await expect(
            bootstrapPersistentDatabase({
                store,
                prepareDatabase: async (database) => {
                    const normalized = structuredClone(database)
                    normalized.username = 'Normalized user'
                    await rival.replaceFromDatabase(structuredClone(persistent))
                    return preparedResult(database, normalized)
                },
            }),
        ).rejects.toBeInstanceOf(RevisionConflictError)
    })
})
