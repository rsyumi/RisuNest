import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import {
    capturePersistentPluginStorage,
    capturePersistentPresets,
    capturePersistentRoot,
    captureResidentPersistentCharacter,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    publishPersistentConversationReplacementToWorkingSet,
    restoreStableWorkingSetSelection,
} from './persistentDataRuntime'
import type { PersistentDataStore, PersistentRevisionLease } from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import {
    createCatalogCharacterStub,
    createCatalogPresetWorkingSet,
    isCatalogCharacterStub,
    isCatalogPresetWorkingSet,
    projectCompleteScalableWorkingSet,
} from './workingSetCatalog'
import { WorkingSetResidencyRegistry } from './workingSetResidency'
import {
    createConversationSummaryStubFromChat,
    isConversationSummaryStub,
} from './conversationResidency'

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

function makeConversationDatabase(username: string): Database {
    const database = makeDatabase(username)
    database.characters[0].chatPage = 0
    database.characters[0].chats = [{
        id: 'chat-a',
        name: 'Chat A',
        note: '',
        localLore: [],
        message: [{ role: 'user', data: `${username} message`, chatId: 'message-a' }],
    }]
    return database
}

async function createActiveSessionRuntimeHarness(
    prepareDatabase: (value: Database) => Promise<Database> = async (value) => value,
) {
    let database = makeConversationDatabase('Initial')
    let revision = 1
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({
            revision,
            value: capturePersistentRoot(database),
        })),
        readConversation: vi.fn(async (characterId: string, conversationId: string) => {
            const conversation = database.characters
                .find((character) => character.chaId === characterId)
                ?.chats.find((chat) => chat.id === conversationId)
            return conversation
                ? { revision, value: structuredClone(conversation) }
                : null
        }),
        commit: vi.fn(async () => ({ revision: ++revision })),
        replaceFromDatabase: vi.fn(async () => ({ revision: 2 })),
        acquireRevision: vi.fn(async (revision: number) =>
            makeDatabaseLease(structuredClone(database), revision)),
    } as unknown as PersistentDataStore
    const runtime = createPersistentDataRuntime({
        store,
        state: {
            captureRoot: () => capturePersistentRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0] ?? null,
            captureCharacter: (id) =>
                database.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => database.characters[0]?.chaId,
            getSelectedConversationId: () => database.characters[0]?.chats[0]?.id,
            replaceDatabase: (replacement) => {
                database = structuredClone(replacement)
            },
            installCompleteDatabase: (replacement) => {
                database = structuredClone(replacement)
            },
            restoreSelection: vi.fn(),
            publishCharacter: vi.fn(),
            publishConversation: vi.fn(),
            publishConversationReplacement: (result) => {
                publishPersistentConversationReplacementToWorkingSet(database, result)
            },
        },
        prepareDatabase,
    })
    await runtime.initializeActiveWorkingSet(database)
    return {
        runtime,
        store,
        get database() {
            return database
        },
    }
}

function makeDatabaseLease(database: Database, revision: number): PersistentRevisionLease {
    const {
        characters,
        botPresets = [],
        pluginCustomStorage = {},
        ...root
    } = database
    return {
        revision,
        readRoot: vi.fn(async () => ({ revision, value: structuredClone(root) })),
        queryPresets: vi.fn(async () => ({
            revision,
            items: botPresets.map((preset, configuredIndex) => ({
                id: String(configuredIndex),
                configuredIndex,
                name: preset.name ?? '',
                image: preset.image,
            })),
        })),
        readPreset: vi.fn(async (id) => {
            const preset = botPresets[Number(id)]
            return preset ? { revision, value: structuredClone(preset) } : null
        }),
        queryCharacters: vi.fn(async ({ trash }) => ({
            revision,
            items: characters.flatMap((character, configuredIndex) =>
                (character.trashTime !== undefined) === trash ? [{
                    id: character.chaId,
                    name: character.name,
                    image: character.image,
                    configuredIndex,
                    recentAt: character.lastInteraction ?? 0,
                    trashed: trash,
                    conversationCount: character.chats.length,
                    type: character.type,
                    creatorNotes: character.creatorNotes,
                    trashTime: character.trashTime,
                }] : []),
        })),
        readCharacterSummary: vi.fn(async (id) => {
            const configuredIndex = characters.findIndex(
                (candidate) => candidate.chaId === id,
            )
            if (configuredIndex < 0) return null
            const character = characters[configuredIndex]
            return {
                id: character.chaId,
                name: character.name,
                image: character.image,
                configuredIndex,
                recentAt: character.lastInteraction ?? 0,
                trashed: character.trashTime !== undefined,
                conversationCount: character.chats.length,
                type: character.type,
                creatorNotes: character.creatorNotes,
                trashTime: character.trashTime,
            }
        }),
        readCharacter: vi.fn(async (id) => {
            const character = characters.find((candidate) => candidate.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = character
            return { revision, value: structuredClone(detail) }
        }),
        queryConversations: vi.fn(async ({ characterId }) => {
            const character = characters.find((candidate) => candidate.chaId === characterId)
            return {
                revision,
                items: (character?.chats ?? []).map((chat, configuredIndex) => ({
                    id: chat.id!,
                    characterId,
                    name: chat.name ?? '',
                    folderId: chat.folderId,
                    bindedPersona: chat.bindedPersona,
                    configuredIndex,
                    recentAt: chat.lastDate ?? 0,
                    messageCount: chat.message.length,
                })),
            }
        }),
        readConversation: vi.fn(async (characterId, conversationId) => {
            const conversation = characters.find((candidate) =>
                candidate.chaId === characterId)?.chats.find((candidate) =>
                candidate.id === conversationId)
            return conversation
                ? { revision, value: structuredClone(conversation) }
                : null
        }),
        readConversationMetadata: vi.fn(async () => null),
        readConversationWindow: vi.fn(async () => null),
        queryPluginStorage: vi.fn(async () => ({
            revision,
            items: Object.keys(pluginCustomStorage).map((key) => ({
                owner: UNOWNED_PLUGIN_OWNER,
                key,
                byteSize: 0,
            })),
        })),
        readPluginStorage: vi.fn(async (_owner, key) => Object.hasOwn(pluginCustomStorage, key)
            ? { revision, value: structuredClone(pluginCustomStorage[key]) }
            : null),
        readAssetAlias: vi.fn(async () => null),
        readAssetAliasesByKeys: vi.fn(async () => ({ revision, value: [] })),
        listAssetAliases: vi.fn(async () => ({ revision, items: [] })),
        readAssetRepositoryAuthority: vi.fn(async () => ({
            revision,
            value: { format: 'legacy' as const },
        })),
        readAssetOwnerHead: vi.fn(async () => null),
        release: vi.fn(async () => undefined),
    }
}

function createFenceRuntimeHarness(
    initial: Database,
    restored: Database = initial,
    restoredRevision = 2,
) {
    let database = structuredClone(initial)
    let revision = 1
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({
            revision,
            value: capturePersistentRoot(database),
        })),
        commit: vi.fn(async ({ expectedRevision }: { expectedRevision: number }) => {
            revision = expectedRevision + 1
            return { revision }
        }),
        acquireRevision: vi.fn(async (requestedRevision: number) => {
            if (requestedRevision === restoredRevision) {
                return makeDatabaseLease(restored, restoredRevision)
            }
            return makeDatabaseLease(database, requestedRevision)
        }),
        materializeDatabase: vi.fn(async (requestedRevision?: number) => {
            if (requestedRevision !== restoredRevision) {
                throw new Error(`Unexpected materialized revision ${requestedRevision}`)
            }
            return structuredClone(restored)
        }),
    } as unknown as PersistentDataStore
    const runtime = createPersistentDataRuntime({
        store,
        state: {
            captureRoot: () => capturePersistentRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage ?? {},
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0] ?? null,
            captureCharacter: (id) => database.characters.find(
                (character) => character.chaId === id,
            ) ?? null,
            getSelectedCharacterId: () => database.characters[0]?.chaId,
            getSelectedConversationId: () => database.characters[0]?.chats[0]?.id,
            replaceDatabase: (replacement) => {
                database = structuredClone(replacement)
            },
            publishCharacter: vi.fn(),
            publishConversation: vi.fn(),
        },
        prepareDatabase: async (value) => value,
    })
    return {
        runtime,
        store,
        get database() { return database },
        set username(value: string) { database.username = value },
    }
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

describe('persistent conversation replacement publication', () => {
    it('forwards a lease-owned replacement refresh without changing selection', async () => {
        const harness = await createActiveSessionRuntimeHarness()
        const target = harness.runtime.captureSelectedConversationTarget()!
        const lease = await harness.runtime.acquireCompleteConversation(
            'plugin-full-object-setter',
            target,
        )
        const replacement = {
            ...structuredClone(harness.database.characters[0].chats[0]),
            note: 'published through runtime',
        }

        await expect(harness.runtime.replacePersistentConversation(
            target.characterId,
            target.conversationId,
            'plugin-chat-set',
            replacement,
            { expectedRevision: target.storeRevision },
        )).resolves.toBe(true)
        expect(lease.session.matchesConversation(target.characterId, replacement)).toBe(false)

        lease.release()
        expect(harness.runtime.refreshSelectedConversationAfterReplacement(
            lease.target,
            lease.session,
        )).toBe(true)
        expect(harness.runtime.getActiveConversationSession()?.matchesConversation(
            target.characterId,
            harness.database.characters[0].chats[0],
        )).toBe(true)
        expect(harness.runtime.captureSelectedConversationTarget()).toMatchObject({
            characterId: target.characterId,
            conversationId: target.conversationId,
        })
    })

    it('forwards inactive replacement options without changing active selection', async () => {
        const harness = await createActiveSessionRuntimeHarness()
        harness.database.characters.push({
            type: 'character',
            chaId: 'char-b',
            name: 'Inactive',
            chatPage: 0,
            chats: [{
                id: 'chat-b',
                name: 'Inactive chat',
                note: '',
                localLore: [],
                message: [],
            }],
        } as any)
        const selectedCharacterId = harness.database.characters[0].chaId
        const selectedConversationId = harness.database.characters[0].chats[0].id
        expect(harness.runtime.captureSelectedConversationTarget()).toMatchObject({
            characterId: selectedCharacterId,
            conversationId: selectedConversationId,
        })
        const replacement = {
            ...structuredClone(harness.database.characters[1].chats[0]),
            name: 'Runtime replacement',
        }

        await expect(harness.runtime.replacePersistentConversation(
            'char-b',
            'chat-b',
            'plugin-chat-set',
            replacement,
            { expectedRevision: 1 },
        )).resolves.toBe(true)

        expect(harness.database.characters[0].chaId).toBe(selectedCharacterId)
        expect(harness.database.characters[0].chats[0].id).toBe(selectedConversationId)
        expect(harness.runtime.captureSelectedConversationTarget()).toMatchObject({
            characterId: selectedCharacterId,
            conversationId: selectedConversationId,
        })
        expect(harness.database.characters[1].chats[0]).toEqual(replacement)
        expect(harness.store.commit).toHaveBeenCalledWith(expect.objectContaining({
            expectedRevision: 1,
            conversations: [expect.objectContaining({
                characterId: 'char-b',
                conversationId: 'chat-b',
            })],
        }))
        await expect(harness.runtime.replacePersistentConversation(
            'char-b',
            'chat-b',
            'plugin-chat-set',
            replacement,
            { expectedRevision: 1 },
        )).rejects.toBeInstanceOf(RevisionConflictError)
    })

    it('updates complete targets while keeping inactive catalog and summary targets bounded', () => {
        const selected = makeConversationDatabase('Selected').characters[0]
        selected.chaId = 'selected-char'
        selected.chats[0].id = 'selected-chat'
        const inactive = createCatalogCharacterStub({
            id: 'inactive-char',
            configuredIndex: 1,
            name: 'Inactive',
            recentAt: 0,
            trashed: false,
            conversationCount: 1,
            type: 'character',
        })
        const database = {
            username: 'Fixture',
            botPresets: [],
            characters: [selected, inactive],
        } as unknown as Database
        const replacement = {
            ...structuredClone(selected.chats[0]),
            name: 'Updated selected chat',
        }

        publishPersistentConversationReplacementToWorkingSet(database, {
            revision: 2,
            characterId: 'inactive-char',
            conversationId: 'inactive-chat',
            conversation: {
                id: 'inactive-chat',
                name: 'Updated inactive',
                note: '',
                localLore: [],
                message: [],
            },
        })
        expect(isCatalogCharacterStub(database.characters[1])).toBe(true)
        expect(database.characters[1].chats).toEqual([])

        publishPersistentConversationReplacementToWorkingSet(database, {
            revision: 3,
            characterId: 'selected-char',
            conversationId: 'selected-chat',
            conversation: replacement,
        })
        expect(database.characters[0].chats[0]).toEqual(replacement)

        database.characters[0].chats[0] = createConversationSummaryStubFromChat(
            'selected-char',
            replacement,
            0,
        )
        publishPersistentConversationReplacementToWorkingSet(database, {
            revision: 4,
            characterId: 'selected-char',
            conversationId: 'selected-chat',
            conversation: { ...replacement, name: 'Summary metadata' },
        })
        expect(isConversationSummaryStub(database.characters[0].chats[0])).toBe(true)
        expect(database.characters[0].chats[0].name).toBe('Summary metadata')
        expect(database.characters[0].chats[0].message).toEqual([])
        expect(database.characters[0].chaId).toBe('selected-char')
    })
})

describe('persistent character mutation publication', () => {
    it('publishes every committed group detail with a deletion and forgets its released stable ID', () => {
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
            characters: [group, {
                type: 'group',
                chaId: 'group-b',
                name: 'Other group',
                characters: ['char-a'],
                characterTalks: [0.5],
                characterActive: [true],
                chats: [],
            }, removed, {
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
                relatedCharacters: [{
                    type: 'group',
                    chaId: 'group-a',
                    name: 'Group',
                    characters: ['char-b'],
                    characterTalks: [0.75],
                    characterActive: [true],
                } as any, {
                    type: 'group',
                    chaId: 'group-b',
                    name: 'Other group',
                    characters: [],
                    characterTalks: [],
                    characterActive: [],
                } as any],
            },
            residency,
            0,
            vi.fn(),
        )

        expect(group.characters).toEqual(['char-b'])
        expect(group.characterTalks).toEqual([0.75])
        expect(group.characterActive).toEqual([true])
        expect((database.characters[1] as any).characters).toEqual([])
        expect(residency.isCharacterReleased('char-a')).toBe(false)

        const readded = { ...removed, name: 'Re-added' }
        database.characters.push(readded)
        expect(captureResidentPersistentCharacter(database, 'char-a', residency)).toBe(readded)
    })

    it('keeps a related scalable group as a catalog stub after deletion publication', () => {
        const residency = new WorkingSetResidencyRegistry()
        const groupStub = createCatalogCharacterStub({
            id: 'group-a',
            name: 'Group',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 3,
            type: 'group',
        })
        const targetStub = createCatalogCharacterStub({
            id: 'char-a',
            name: 'Target',
            configuredIndex: 1,
            recentAt: 0,
            trashed: false,
            conversationCount: 1,
            type: 'character',
        })
        const database = {
            ...makeDatabase('Scalable delete'),
            characters: [groupStub, targetStub],
        } as Database

        publishPersistentCharacterMutationToWorkingSet(
            database,
            {
                revision: 2,
                root: capturePersistentRoot(database),
                characterId: 'char-a',
                kind: 'delete',
                character: null,
                relatedCharacters: [{
                    type: 'group',
                    chaId: 'group-a',
                    name: 'Updated group',
                    characters: ['char-b'],
                    characterTalks: [0.75],
                    characterActive: [true],
                } as any],
            },
            residency,
            -1,
            vi.fn(),
        )

        expect(database.characters).toHaveLength(1)
        expect(database.characters[0].name).toBe('Updated group')
        expect(isCatalogCharacterStub(database.characters[0])).toBe(true)
        expect(database.characters[0]).not.toHaveProperty('characters')
        expect(database.characters[0].chats).toEqual([])
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
    it('starts detached preparation before the caller can change the replacement input', async () => {
        const ready = deferred<void>()
        const prepareDatabase = vi.fn(async (value: Database) => {
            const captured = structuredClone(value)
            await ready.promise
            return captured
        })
        const harness = await createActiveSessionRuntimeHarness(prepareDatabase)
        const input = makeConversationDatabase('Requested replacement')
        const captured = structuredClone(input)
        const replacing = harness.runtime.replacePersistentDatabase(input, 'capture-before-prepare')
        input.username = 'Changed after request'
        input.characters[0].chats[0].message[0].data = 'Changed nested input'
        ready.resolve()
        await replacing

        expect(prepareDatabase).toHaveBeenCalledOnce()
        expect(harness.store.replaceFromDatabase).toHaveBeenCalledWith(captured, 1)
        expect(harness.database).toEqual(captured)
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('invalidates a resident session before publishing a cloned conversation', async () => {
        const harness = await createActiveSessionRuntimeHarness()
        const session = harness.runtime.getActiveConversationSession()!

        harness.runtime.invalidateActiveConversationSession()

        expect(session.isActive).toBe(false)
        expect(harness.runtime.getActiveConversationSession()).toBeNull()
        expect(() => session.append({ role: 'char', data: 'detached clone write' })).toThrow(
            /inactive/,
        )
    })

    it('invalidates the resident session after explicit database replacement', async () => {
        const harness = await createActiveSessionRuntimeHarness()
        const session = harness.runtime.getActiveConversationSession()!

        await harness.runtime.replacePersistentDatabase(
            makeConversationDatabase('Replacement'),
            'replace-active-session',
        )

        expect(session.isActive).toBe(false)
        expect(harness.runtime.getActiveConversationSession()).toBeNull()
        expect(() => session.append({ role: 'char', data: 'detached replacement write' })).toThrow(
            /inactive/,
        )
    })

    it('invalidates the resident session after scalable working-set projection', async () => {
        const harness = await createActiveSessionRuntimeHarness()
        const session = harness.runtime.getActiveConversationSession()!

        await expect(harness.runtime.releaseInactiveWorkingSet()).resolves.toBe(true)

        expect(session.isActive).toBe(false)
        expect(harness.runtime.getActiveConversationSession()).toBeNull()
        expect(() => session.append({ role: 'char', data: 'detached projection write' })).toThrow(
            /inactive/,
        )
    })

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

    it('preserves newer local edits and rejects a stale prepared replacement', async () => {
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
        await expect(replacement).rejects.toBeInstanceOf(RevisionConflictError)

        expect(database.username).toBe('Edited during preparation')
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(store.commit).toHaveBeenCalledOnce()
        expect(runtime.revision).toBe(2)
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
        const lease = makeDatabaseLease(complete, 4)
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({ revision: 4, value: capturePersistentRoot(complete) })),
            acquireRevision: vi.fn(async () => lease),
            materializeDatabase: vi.fn(async () => {
                throw new Error('legacy materializer must not be called')
            }),
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
                    database = replacement
                },
                restoreSelection,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)

        await runtime.releaseInactiveWorkingSet()

        expect(store.materializeDatabase).not.toHaveBeenCalled()
        expect(store.acquireRevision).toHaveBeenCalledWith(4)
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
        const materialization = deferred<Awaited<
            ReturnType<PersistentRevisionLease['queryCharacters']>
        >>()
        const lease = makeDatabaseLease(snapshot, 4)
        const originalQuery = lease.queryCharacters.bind(lease)
        lease.queryCharacters = vi.fn(() => materialization.promise)
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({
                revision: 4,
                value: capturePersistentRoot(database),
            })),
            acquireRevision: vi.fn(async () => lease),
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
        await vi.waitFor(() => expect(lease.queryCharacters).toHaveBeenCalledOnce())
        materialization.resolve(await originalQuery({
            order: 'configured',
            trash: false,
            limit: 128,
        }))

        await expect(release).resolves.toBe(false)
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('rechecks transition currency synchronously after an async release guard', async () => {
        const database = makeDatabase('Initial')
        const releasePermission = deferred<boolean>()
        let transitionCurrent = true
        const replaceDatabase = vi.fn()
        const snapshot = makeDatabase('Snapshot')
        const runtime = createPersistentDataRuntime({
            store: {
                open: vi.fn(async () => undefined),
                readRoot: vi.fn(async () => ({
                    revision: 4,
                    value: capturePersistentRoot(database),
                })),
                acquireRevision: vi.fn(async () => makeDatabaseLease(snapshot, 4)),
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
                publishCharacterSet: (primary, related) => {
                    for (const value of [primary, ...related]) {
                        const resident = database.characters.find((item) => item.chaId === value.chaId)
                        if (resident) Object.assign(resident, structuredClone(value))
                    }
                },
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

    it('blocks a stale UI edit and persists fresh edits after bounded replacement publication', async () => {
        const complete = {
            username: 'Initial',
            customBackground: '',
            botPresetsId: 0,
            botPresets: [{ name: 'Active', mainPrompt: 'body' }],
            pluginCustomStorage: { cached: true },
            characters: [{
                type: 'character',
                chaId: 'member-a',
                name: 'Member',
                personality: 'member detail',
                chats: [],
            }, {
                type: 'group',
                chaId: 'group-a',
                name: 'Group',
                characters: ['member-a'],
                characterTalks: [1],
                characterActive: [true],
                chats: [],
            }, {
                type: 'character',
                chaId: 'inactive',
                name: 'Inactive',
                personality: 'release me',
                chats: [],
            }],
        } as unknown as Database
        let database = structuredClone(complete)
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const acquireRevision = vi.fn(async () => {
            throw new Error('replacement publication must not acquire a revision')
        })
        const materializeDatabase = vi.fn(async () => {
            throw new Error('replacement publication must not materialize the database')
        })
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({
                revision: 7,
                value: capturePersistentRoot(complete),
            })),
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit,
            acquireRevision,
            materializeDatabase,
        } as unknown as PersistentDataStore
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePluginStorage: () => database.pluginCustomStorage,
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters.find(
                    (character) => character.chaId === 'group-a',
                ) ?? null,
                captureCharacter: (id) => database.characters.find(
                    (character) => character.chaId === id,
                ) ?? null,
                getSelectedCharacterId: () => 'group-a',
                replaceDatabase: (replacement, activeCharacterIds) => {
                    database = projectCompleteScalableWorkingSet(
                        replacement,
                        'group-a',
                        8,
                        activeCharacterIds,
                    )
                },
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        const replacement = structuredClone(complete)
        replacement.username = 'Replacement'

        const replacing = runtime.replacePersistentDatabase(replacement, 'delayed-replacement')
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        expect(() => {
            runtime.assertPersistentMutationAllowed()
            database.customBackground = 'blocked edit during replacement'
        }).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 8 })
        await replacing

        expect(database.username).toBe('Replacement')
        expect(database.customBackground).toBe('')
        expect(database.pluginCustomStorage).toEqual({})
        expect(database.characters[0].personality).toBe('member detail')
        expect(isCatalogCharacterStub(database.characters[2])).toBe(true)
        expect(acquireRevision).not.toHaveBeenCalled()
        expect(materializeDatabase).not.toHaveBeenCalled()

        expect(commit).not.toHaveBeenCalled()
        database.customBackground = 'live edit during replacement'
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('persist-new-live-edit')
        expect(commit).toHaveBeenCalledWith(
            expect.objectContaining({
                expectedRevision: 8,
                rootMutations: [
                    { type: 'set', key: 'customBackground', value: 'live edit during replacement' },
                ],
            }),
        )
    })

    it('cannot split committed replacement publication on revision read or release failure', async () => {
        const complete = {
            username: 'Initial',
            botPresetsId: 0,
            botPresets: [{ name: 'Active', mainPrompt: 'body' }],
            pluginCustomStorage: { cached: true },
            characters: [{
                type: 'character',
                chaId: 'char-a',
                name: 'Selected',
                personality: 'selected detail',
                chats: [],
            }, {
                type: 'character',
                chaId: 'inactive',
                name: 'Inactive',
                personality: 'release me',
                chats: [],
            }],
        } as unknown as Database
        let database = structuredClone(complete)
        const acquireRevision = vi.fn(async () => {
            throw new Error('revision reads are unavailable')
        })
        const materializeDatabase = vi.fn(async () => {
            throw new Error('full materialization is unavailable')
        })
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({
                revision: 7,
                value: capturePersistentRoot(complete),
            })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
            acquireRevision,
            materializeDatabase,
        } as unknown as PersistentDataStore
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePluginStorage: () => database.pluginCustomStorage,
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) => database.characters.find(
                    (character) => character.chaId === id,
                ) ?? null,
                getSelectedCharacterId: () => 'char-a',
                replaceDatabase: (replacement, activeCharacterIds) => {
                    database = projectCompleteScalableWorkingSet(
                        replacement,
                        'char-a',
                        8,
                        activeCharacterIds,
                    )
                },
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        const replacement = structuredClone(complete)
        replacement.username = 'Committed replacement'

        await runtime.replacePersistentDatabase(replacement, 'nonthrowing-publication')

        expect(runtime.revision).toBe(8)
        expect(database.username).toBe('Committed replacement')
        expect(database.pluginCustomStorage).toEqual({})
        expect(database.characters[0].personality).toBe('selected detail')
        expect(isCatalogCharacterStub(database.characters[1])).toBe(true)
        expect(acquireRevision).not.toHaveBeenCalled()
        expect(materializeDatabase).not.toHaveBeenCalled()
    })
})

describe('native replacement working-set refresh', () => {
    it('rebuilds only the scalable selected working set from the committed revision', async () => {
        let database = makeConversationDatabase('Before native restore')
        const restored = makeConversationDatabase('After native restore')
        restored.characters[0].chats[0].message[0].data = 'restored selected message'
        restored.characters.push({
            type: 'character',
            chaId: 'char-b',
            name: 'Inactive',
            personality: 'must stay outside the JS working set',
            chats: [{
                id: 'chat-b',
                name: 'Inactive chat',
                note: '',
                localLore: [],
                message: [{ role: 'user', data: 'inactive payload', chatId: 'message-b' }],
            }],
        } as Database['characters'][number])
        const lease = makeDatabaseLease(restored, 2)
        const materializeDatabase = vi.fn(async () => {
            throw new Error('native refresh must not materialize the complete database')
        })
        const store = {
            open: vi.fn(async () => undefined),
            readRoot: vi.fn(async () => ({
                revision: 1,
                value: capturePersistentRoot(database),
            })),
            acquireRevision: vi.fn(async (revision: number) => {
                expect(revision).toBe(2)
                return lease
            }),
            materializeDatabase,
        } as unknown as PersistentDataStore
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePluginStorage: () => database.pluginCustomStorage ?? null,
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0] ?? null,
                captureCharacter: (id) => database.characters.find(
                    (character) => character.chaId === id,
                ) ?? null,
                getSelectedCharacterId: () => 'char-a',
                getSelectedConversationId: () => 'chat-a',
                replaceDatabase: (replacement) => {
                    database = replacement
                },
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        vi.mocked(store.readRoot).mockResolvedValue({ revision: 2, value: capturePersistentRoot(restored) })

        await expect(runtime.refreshActiveWorkingSetFromStore(2)).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })

        expect(runtime.revision).toBe(2)
        expect(database.username).toBe('After native restore')
        expect(database.characters[0].chats[0].message[0].data).toBe(
            'restored selected message',
        )
        expect(isCatalogCharacterStub(database.characters[1])).toBe(true)
        expect(database.characters[1]).not.toHaveProperty('personality')
        expect(materializeDatabase).not.toHaveBeenCalled()
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('edit_during_prepare_invalidates_auto_apply without discarding the local edit', async () => {
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')

        harness.username = 'Live edit during native parse'
        runtime.markPersistentDataDirty(1)

        const apply = vi.fn()
        await expect(runtime.acquireDestructiveReplacementFence(token).then(apply))
            .rejects.toThrow(/revision/i)
        expect(apply).not.toHaveBeenCalled()
        expect(runtime.revision).toBe(token.revision + 1)
        expect(harness.database.username).toBe('Live edit during native parse')
        expect(harness.store.commit).toHaveBeenCalledWith(
            expect.objectContaining({
                rootMutations: [
                    { type: 'set', key: 'username', value: 'Live edit during native parse' },
                ],
            }),
        )
        expect(() => runtime.assertPersistentMutationAllowed()).not.toThrow()
    })

    it('requires a fresh explicit-choice token after even a no-op dirty notice', async () => {
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')

        runtime.markPersistentDataDirty(0)

        await expect(runtime.acquireDestructiveReplacementFence(token)).rejects.toThrow(/replacement is active/i)
        expect(runtime.revision).toBe(token.revision)
        expect(harness.store.commit).not.toHaveBeenCalled()
        const chosenAgain = await runtime.capturePersistentMutationToken('user-selected-restore-again', {
            publishOfficial: false,
        })
        const fence = await runtime.acquireDestructiveReplacementFence(chosenAgain)
        expect(fence.revision).toBe(token.revision)
        fence.release()
    })

    it('persists outstanding edits but never promotes a stale token during local drain', async () => {
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')

        harness.username = 'Live edit during native parse'
        runtime.markPersistentDataDirty(1)
        vi.mocked(harness.store.commit).mockImplementationOnce(async (commit) => {
            harness.username = 'Edit landed while the fence was flushing'
            return { revision: commit.expectedRevision + 1 }
        })

        await expect(runtime.acquireDestructiveReplacementFence(token)).rejects.toThrow(/revision/i)
        expect(runtime.revision).toBeGreaterThan(token.revision)
        expect(() => runtime.assertPersistentMutationAllowed()).not.toThrow()
        expect(harness.store.commit).toHaveBeenLastCalledWith(
            expect.objectContaining({
                rootMutations: [
                    {
                        type: 'set',
                       
                        key: 'username',
                        value: 'Edit landed while the fence was flushing',
                    },
                ],
            }),
        )
    })

    it('rejects a destructive fence when a live edit happened after the captured token', async () => {
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')

        harness.username = 'Live edit during native parse'
        runtime.markPersistentDataDirty(1)

        await expect(runtime.acquireDestructiveReplacementFence(token)).rejects.toThrow(
            /revision|mutation generation/i,
        )
        expect(harness.database.username).toBe('Live edit during native parse')
        expect(runtime.revision).toBeGreaterThan(token.revision)
    })

    it('keeps edits fenced through committed refresh and releases only after acknowledgement', async () => {
        const restored = makeConversationDatabase('After native restore')
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
            restored,
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')
        const fence = await runtime.acquireDestructiveReplacementFence(token)

        expect(() => runtime.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        await fence.refreshCommittedWorkingSet(2)
        expect(harness.database.username).toBe('After native restore')
        expect(runtime.revision).toBe(2)
        expect(() => runtime.markPersistentDataDirty(1)).toThrow(/replacement is active/i)

        harness.username = 'Edit after native commit'
        expect(() => runtime.markPersistentDataDirty(1)).toThrow(/replacement is active/i)

        fence.release()
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('post-native-commit-edit')
        expect(harness.database.username).toBe('Edit after native commit')
        expect(harness.store.commit).toHaveBeenCalledWith(
            expect.objectContaining({
                rootMutations: [
                    { type: 'set', key: 'username', value: 'Edit after native commit' },
                ],
            }),
        )
    })

    it('keeps a stale working set read-only when native acknowledgement fails after commit', async () => {
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
            makeConversationDatabase('After native restore'),
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')
        const fence = await runtime.acquireDestructiveReplacementFence(token)

        runtime.markCommittedWorkingSetRefreshRequired(
            2,
            new Error('synthetic native acknowledgement failure'),
        )
        fence.release()

        expect(runtime.revision).toBe(2)
        expect(runtime.pendingWorkingSetRefreshRevision).toBe(2)
        expect(harness.database.username).toBe('Before native restore')
        expect(() => runtime.assertPersistentMutationAllowed()).toThrow(/replacement is active/i)
        expect(() => runtime.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('does not overwrite an already-applied edit during committed working-set projection', async () => {
        const restored = makeConversationDatabase('After native restore')
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
            restored,
        )
        const projectionStarted = deferred<void>()
        const allowProjection = deferred<void>()
        vi.mocked(harness.store.acquireRevision).mockImplementationOnce(async () => {
            projectionStarted.resolve()
            await allowProjection.promise
            return makeDatabaseLease(restored, 2)
        })
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')
        const fence = await runtime.acquireDestructiveReplacementFence(token)

        const refreshing = fence.refreshCommittedWorkingSet(2)
        await projectionStarted.promise
        harness.username = 'Edit resumed after native commit'
        allowProjection.resolve()

        await expect(refreshing).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'refresh-required',
        })
        expect(harness.database.username).toBe('Edit resumed after native commit')
        expect(() => runtime.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        fence.release()
        expect(runtime.pendingWorkingSetRefreshRevision).toBe(2)
        expect(() => runtime.assertPersistentMutationAllowed()).toThrow(/replacement is active/i)
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('keeps a large post-publication edit blocked until fence release', async () => {
        const restored = makeConversationDatabase('After native restore')
        const harness = createFenceRuntimeHarness(
            makeConversationDatabase('Before native restore'),
            restored,
        )
        const { runtime } = harness
        await runtime.initializeActiveWorkingSet(harness.database)
        const token = await runtime.capturePersistentMutationToken('native-restore-start')
        const fence = await runtime.acquireDestructiveReplacementFence(token)
        await fence.refreshCommittedWorkingSet(2)

        harness.username = 'Large post-publication edit'
        expect(() => runtime.markPersistentDataDirty(2 * 1024 * 1024)).toThrow(
            /replacement is active/i,
        )
        fence.release()
        runtime.markPersistentDataDirty(2 * 1024 * 1024)
        await runtime.flushPendingData('large-post-publication-edit')

        expect(harness.store.commit).toHaveBeenCalledWith(
            expect.objectContaining({
                rootMutations: [
                    { type: 'set', key: 'username', value: 'Large post-publication edit' },
                ],
            }),
        )
    })
})
