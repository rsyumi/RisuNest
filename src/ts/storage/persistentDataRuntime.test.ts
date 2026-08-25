import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import {
    capturePersistentPluginStorage,
    capturePersistentPresets,
    capturePersistentRoot,
    captureResidentPersistentCharacter,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    restoreStableWorkingSetSelection,
} from './persistentDataRuntime'
import type { PersistentDataStore } from './persistentDataStore'
import {
    createCatalogCharacterStub,
    createCatalogPresetWorkingSet,
    isCatalogCharacterStub,
    isCatalogPresetWorkingSet,
    projectCompleteScalableWorkingSet,
} from './workingSetCatalog'
import { WorkingSetResidencyRegistry } from './workingSetResidency'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function makeDatabase(username: string): Database {
    return {
        username,
        botPresets: [],
        characters: [{
            type: 'character',
            chaId: 'char-a',
            name: 'Alpha',
            chats: [],
        }],
    } as unknown as Database
}

describe('persistent preset capture', () => {
    it('returns null for a selected-only scalable preset working set', () => {
        const botPresets = createCatalogPresetWorkingSet(
            {
                revision: 3,
                items: [
                    { id: '0', configuredIndex: 0, name: 'Inactive' },
                    { id: '1', configuredIndex: 1, name: 'Active' },
                ],
            },
            {
                summary: { id: '1', configuredIndex: 1, name: 'Active' },
                value: { name: 'Active', mainPrompt: 'full body' } as Database['botPresets'][number],
            },
        )
        const database = { botPresets } as Database

        expect(capturePersistentPresets(database)).toBeNull()
    })

    it('returns the full array for maximum compatibility state', () => {
        const botPresets = [
            { name: 'First', mainPrompt: 'one' },
            { name: 'Second', mainPrompt: 'two' },
        ] as Database['botPresets']

        expect(capturePersistentPresets({ botPresets } as Database)).toBe(botPresets)
    })
})

describe('persistent plugin storage capture', () => {
    it('does not capture the empty compatibility object from a scalable working set', () => {
        const database = makeDatabase('Scalable')
        database.botPresets = createCatalogPresetWorkingSet(
            { revision: 3, items: [] },
            null,
        )
        database.pluginCustomStorage = {}

        expect(capturePersistentPluginStorage(database)).toBeNull()
    })

    it('captures all values from a maximum-compatibility working set', () => {
        const database = makeDatabase('Maximum')
        database.pluginCustomStorage = { memory: { entries: [1, 2, 3] } }

        expect(capturePersistentPluginStorage(database)).toBe(database.pluginCustomStorage)
    })
})

describe('persistent character mutation publication', () => {
    it('prunes a deleted selected-group member and forgets its released stable ID', () => {
        const residency = new WorkingSetResidencyRegistry()
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        } as any
        const removed = {
            type: 'character',
            chaId: 'char-a',
            name: 'Removed',
            chats: [],
        } as any
        const database = {
            ...makeDatabase('Delete'),
            characters: [group, removed, {
                type: 'character',
                chaId: 'char-b',
                name: 'Remaining',
                chats: [],
            }],
        } as Database
        residency.markCharacterReleased('char-a')

        publishPersistentCharacterMutationToWorkingSet(
            database,
            {
                revision: 2,
                root: capturePersistentRoot(database),
                characterId: 'char-a',
                kind: 'delete',
                character: null,
            },
            residency,
            0,
            vi.fn(),
        )

        expect(group.characters).toEqual(['char-b'])
        expect(group.characterTalks).toEqual([0.75])
        expect(group.characterActive).toEqual([true])
        expect(residency.isCharacterReleased('char-a')).toBe(false)

        const readded = { ...removed, name: 'Re-added' }
        database.characters.push(readded)
        expect(captureResidentPersistentCharacter(database, 'char-a', residency)).toBe(readded)
    })

    it('keeps scalable add, released replace, and detail mutations bounded', () => {
        const residency = new WorkingSetResidencyRegistry()
        const database = makeDatabase('Catalog')
        database.characters = [createCatalogCharacterStub({
            id: 'char-a',
            name: 'Alpha',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 2,
            type: 'character',
        })]
        const select = vi.fn()
        const complete = {
            type: 'character',
            chaId: 'char-a',
            name: 'Replaced',
            personality: 'full body',
            chats: [{ id: 'chat-a', message: [{ role: 'user', data: 'hidden' }] }],
        } as any

        publishPersistentCharacterMutationToWorkingSet(
            database,
            {
                revision: 2,
                root: capturePersistentRoot(database),
                characterId: 'char-a',
                kind: 'replace',
                character: complete,
            },
            residency,
            -1,
            select,
        )
        publishPersistentCharacterMutationToWorkingSet(
            database,
            {
                revision: 3,
                root: capturePersistentRoot(database),
                characterId: '§temp',
                kind: 'add',
                character: { ...complete, chaId: '§temp', name: 'Temporary' },
            },
            residency,
            -1,
            select,
        )
        publishPersistentCharacterMutationToWorkingSet(
            database,
            {
                revision: 4,
                root: capturePersistentRoot(database),
                characterId: '§temp',
                kind: 'detail',
                character: {
                    type: 'character',
                    chaId: '§temp',
                    name: 'Renamed temporary',
                    personality: 'must remain absent',
                } as any,
            },
            residency,
            -1,
            select,
        )

        expect(database.characters).toHaveLength(2)
        expect(database.characters.every(isCatalogCharacterStub)).toBe(true)
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(database.characters[1]).toMatchObject({
            chaId: '§temp',
            name: 'Renamed temporary',
            chats: [],
        })
        expect(database.characters[1]).not.toHaveProperty('personality')
        expect(residency.isCharacterReleased('char-a')).toBe(true)
        expect(residency.isCharacterReleased('§temp')).toBe(true)
        expect(select).not.toHaveBeenCalled()
    })

    it('keeps maximum-compatibility character additions complete', () => {
        const residency = new WorkingSetResidencyRegistry()
        residency.setEvictionAllowed(false)
        const database = makeDatabase('Maximum')
        const complete = {
            type: 'character',
            chaId: 'char-b',
            name: 'Complete',
            personality: 'resident body',
            chats: [{ id: 'chat-b', message: [{ role: 'user', data: 'resident' }] }],
        } as any

        publishPersistentCharacterMutationToWorkingSet(
            database,
            {
                revision: 2,
                root: capturePersistentRoot(database),
                characterId: 'char-b',
                kind: 'add',
                character: complete,
            },
            residency,
            -1,
            vi.fn(),
        )

        expect(database.characters[1]).toEqual(complete)
        expect(isCatalogCharacterStub(database.characters[1])).toBe(false)
        expect(residency.isCharacterReleased('char-b')).toBe(false)
    })
})

describe('stable working-set selection', () => {
    it('restores a reordered character and its selected conversation by stable ID', () => {
        const database = {
            botPresets: [],
            characters: [
                {
                    type: 'character',
                    chaId: 'char-b',
                    name: 'Beta',
                    chatPage: 0,
                    chats: [
                        { id: 'chat-b2', message: [] },
                        { id: 'chat-b1', message: [] },
                    ],
                },
                {
                    type: 'character',
                    chaId: 'char-a',
                    name: 'Alpha',
                    chats: [],
                },
            ],
        } as unknown as Database
        const selectCharacterIndex = vi.fn()

        restoreStableWorkingSetSelection(
            database,
            'char-b',
            'chat-b1',
            selectCharacterIndex,
        )

        expect(selectCharacterIndex).toHaveBeenCalledWith(0)
        expect(database.characters[0].chatPage).toBe(1)
    })

    it('clears selection when the stable character no longer exists', () => {
        const database = makeDatabase('Replacement')
        const selectCharacterIndex = vi.fn()

        restoreStableWorkingSetSelection(
            database,
            'missing-character',
            'missing-chat',
            selectCharacterIndex,
        )

        expect(selectCharacterIndex).toHaveBeenCalledWith(-1)
    })
})

describe('prepared persistent replacement', () => {
    it('rejects a catalog working set before database preparation', async () => {
        let database = makeDatabase('Initial')
        const prepareDatabase = vi.fn(async (value: Database) => value)
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({ revision: 1, value: { username: 'Initial' } })),
            replaceFromDatabase: vi.fn(),
        } as unknown as PersistentDataStore
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => database.characters[0]?.chaId,
                replaceDatabase: (replacement) => {
                    database = replacement
                },
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase,
        })
        await runtime.initializeActiveWorkingSet(database)
        const catalog = makeDatabase('Catalog')
        catalog.characters = [createCatalogCharacterStub({
            id: 'char-a',
            name: 'Alpha',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 3,
            type: 'character',
        })]

        await expect(runtime.replacePersistentDatabase(catalog, 'unsafe-catalog'))
            .rejects.toThrow('incomplete persistent working set')

        expect(prepareDatabase).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('rebases live edits made while database preparation is pending', async () => {
        let database = makeDatabase('Initial')
        const preparation = deferred<Database>()
        const prepareDatabase = vi.fn(() => preparation.promise)
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({ revision: 1, value: { username: 'Initial' } })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 2 })),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => {
                    const { characters: _characters, botPresets: _botPresets, ...root } = database
                    return root
                },
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => database.characters[0]?.chaId,
                replaceDatabase: (replacement) => {
                    database = structuredClone(replacement)
                },
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase,
        })
        await runtime.initializeActiveWorkingSet(database)
        const replacement = runtime.replacePersistentDatabase(
            makeDatabase('Replacement'),
            'plugin-profile-change',
        )
        await vi.waitFor(() => expect(prepareDatabase).toHaveBeenCalledOnce())

        database.username = 'Edited during preparation'
        runtime.markPersistentDataDirty(1)
        preparation.resolve(makeDatabase('Prepared replacement'))
        await replacement

        expect(database.username).toBe('Edited during preparation')
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ username: 'Prepared replacement' }),
            1,
        )
    })

    it('returns a detached materialized snapshot without installing it', async () => {
        let database = makeDatabase('Initial')
        const snapshot = makeDatabase('Snapshot')
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({ revision: 4, value: { username: 'Initial' } })),
            materializeDatabase: vi.fn(async () => snapshot),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            database = replacement
        })
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => database.characters[0]?.chaId,
                replaceDatabase,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)

        const materialized = await runtime.materializePersistentDatabaseSnapshot('dataset-export')

        expect(materialized).toEqual(snapshot)
        expect(materialized).not.toBe(snapshot)
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('reprojects an authoritative complete snapshot into the scalable working set', async () => {
        const complete = {
            username: 'Authoritative',
            botPresetsId: 1,
            botPresets: [
                { name: 'Inactive', mainPrompt: 'inactive body' },
                { name: 'Active', mainPrompt: 'active body' },
            ],
            characters: [
                {
                    type: 'character',
                    chaId: 'char-a',
                    name: 'Alpha',
                    personality: 'inactive body',
                    chats: [{ id: 'chat-a', message: [{ role: 'user', data: 'hidden' }] }],
                },
                {
                    type: 'character',
                    chaId: 'char-b',
                    name: 'Beta',
                    personality: 'active body',
                    chatPage: 0,
                    chats: [{ id: 'chat-b', message: [{ role: 'user', data: 'visible' }] }],
                },
            ],
        } as unknown as Database
        let database = structuredClone(complete)
        let projectedActiveIds: string[] = []
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({ revision: 4, value: capturePersistentRoot(complete) })),
            materializeDatabase: vi.fn(async () => structuredClone(complete)),
        } as unknown as PersistentDataStore
        const restoreSelection = vi.fn()
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[1],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => 'char-b',
                getSelectedConversationId: () => 'chat-b',
                replaceDatabase: (replacement, activeCharacterIds, forceScalableProjection) => {
                    expect(forceScalableProjection).toBe(true)
                    projectedActiveIds = [...(activeCharacterIds ?? [])]
                    database = projectCompleteScalableWorkingSet(replacement, 'char-b', 4)
                },
                restoreSelection,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)

        await runtime.releaseInactiveWorkingSet()

        expect(store.materializeDatabase).toHaveBeenCalledWith(4)
        expect(database.username).toBe('Authoritative')
        expect(isCatalogCharacterStub(database.characters[0])).toBe(true)
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(isCatalogCharacterStub(database.characters[1])).toBe(false)
        expect(database.characters[1].personality).toBe('active body')
        expect(isCatalogPresetWorkingSet(database.botPresets)).toBe(true)
        expect(database.botPresets[0]).not.toHaveProperty('mainPrompt')
        expect(database.botPresets[1].mainPrompt).toBe('active body')
        expect(projectedActiveIds).toEqual(['char-b'])
        expect(restoreSelection).toHaveBeenCalledWith('char-b', 'chat-b')
    })

    it('does not publish a scalable snapshot when the final release guard becomes false', async () => {
        const database = makeDatabase('Initial')
        const snapshot = makeDatabase('Snapshot')
        const materialization = deferred<Database>()
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({
                revision: 4,
                value: capturePersistentRoot(database),
            })),
            materializeDatabase: vi.fn(() => materialization.promise),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn()
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => database.characters[0]?.chaId,
                replaceDatabase,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)

        const release = runtime.releaseInactiveWorkingSet(() => false)
        await vi.waitFor(() => expect(store.materializeDatabase).toHaveBeenCalledOnce())
        materialization.resolve(snapshot)

        await expect(release).resolves.toBe(false)
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('rechecks transition currency synchronously after an async release guard', async () => {
        const database = makeDatabase('Initial')
        const releasePermission = deferred<boolean>()
        let transitionCurrent = true
        const replaceDatabase = vi.fn()
        const runtime = createPersistentDataRuntime({
            store: {
                open: vi.fn(async () => undefined),
                readRoot: vi.fn(async () => ({
                    revision: 4,
                    value: capturePersistentRoot(database),
                })),
                materializeDatabase: vi.fn(async () => makeDatabase('Snapshot')),
            } as unknown as PersistentDataStore,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => database.characters[0]?.chaId,
                replaceDatabase,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)

        const release = runtime.releaseInactiveWorkingSet(
            () => releasePermission.promise,
            () => transitionCurrent,
        )
        transitionCurrent = false
        releasePermission.resolve(true)

        await expect(release).resolves.toBe(false)
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('preserves active group members when an explicit replacement is projected', async () => {
        const complete = {
            username: 'Initial',
            botPresetsId: 0,
            botPresets: [{ name: 'Active', mainPrompt: 'body' }],
            characters: [
                {
                    type: 'character',
                    chaId: 'member-a',
                    name: 'Alpha',
                    personality: 'alpha body',
                    chats: [],
                },
                {
                    type: 'character',
                    chaId: 'member-b',
                    name: 'Beta',
                    personality: 'beta body',
                    chats: [],
                },
                {
                    type: 'character',
                    chaId: 'member-c',
                    name: 'Gamma',
                    personality: 'gamma body',
                    chats: [],
                },
                {
                    type: 'group',
                    chaId: 'group-a',
                    name: 'Group',
                    characters: ['member-a', 'member-b'],
                    characterTalks: [1, 1],
                    characterActive: [true, true],
                    chats: [],
                },
                {
                    type: 'character',
                    chaId: 'inactive',
                    name: 'Inactive',
                    personality: 'release me',
                    chats: [],
                },
            ],
        } as unknown as Database
        let database = structuredClone(complete)
        let projectedActiveIds: string[] = []
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({ revision: 1, value: capturePersistentRoot(complete) })),
            readCharacter: vi.fn(async (id: string) => ({
                revision: 1,
                value: structuredClone(
                    complete.characters.find((character) => character.chaId === id),
                ),
            })),
            queryConversations: vi.fn(async () => ({ revision: 1, items: [] })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 2 })),
        } as unknown as PersistentDataStore
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () =>
                    database.characters.find((character) => character.chaId === 'group-a') ?? null,
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => 'group-a',
                replaceDatabase: (replacement, activeCharacterIds) => {
                    projectedActiveIds = [...(activeCharacterIds ?? [])]
                    database = projectCompleteScalableWorkingSet(
                        replacement,
                        'group-a',
                        2,
                        activeCharacterIds,
                    )
                },
                publishCharacter: vi.fn(),
                publishCharacterSet: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        await expect(runtime.activateCharacter('group-a')).resolves.toBe(true)
        const replacement = structuredClone(complete)
        replacement.username = 'Replacement'
        const replacementGroup = replacement.characters.find(
            (character) => character.chaId === 'group-a',
        ) as Database['characters'][number] & {
            characters: string[]
            characterTalks: number[]
            characterActive: boolean[]
        }
        replacementGroup.characters = ['member-b', 'member-c']
        replacementGroup.characterTalks = [1, 1]
        replacementGroup.characterActive = [true, true]

        await runtime.replacePersistentDatabase(replacement, 'explicit-replacement')

        expect(projectedActiveIds).toEqual(['group-a', 'member-b', 'member-c'])
        expect(isCatalogCharacterStub(database.characters[0])).toBe(true)
        expect(database.characters[1].personality).toBe('beta body')
        expect(database.characters[2].personality).toBe('gamma body')
        expect(database.characters[3].type).toBe('group')
        expect(isCatalogCharacterStub(database.characters[4])).toBe(true)
    })
})
