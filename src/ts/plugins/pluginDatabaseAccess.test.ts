const PLUGIN_ACCESS_OWNER = 'test-plugin'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'
import type {
    CharacterPage,
    ConversationPage,
    ConversationWindow,
    PersistentDataStore,
    PluginStorageMutation,
    PersistentRevisionLease,
} from '../storage/persistentDataStore'
import { RevisionConflictError } from '../storage/persistentDataStore'
import { getPersistentDataStore } from '../storage/persistentDataStoreFactory'
import {
    createCatalogCharacterStub,
    createCatalogPresetWorkingSet,
} from '../storage/workingSetCatalog'
import { createConversationSummaryStubFromChat } from '../storage/conversationResidency'
import {
    createPluginDatabaseAccess,
    applyPluginDatabaseUpdate,
    createProductionPluginDatabaseAccess,
    createProductionPluginChatOutputProjector,
    linkPluginQueryAbortSignals,
    type PluginCompleteCharacter,
    type PluginFullObjectCallContext,
    PluginIdentityReplacementRejectedError,
} from './pluginDatabaseAccess'

vi.mock('../storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: vi.fn(),
}))

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (reason?: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, reject, resolve }
}

const characterPage: CharacterPage = {
    revision: 4,
    items: [
        {
            id: 'char-a',
            name: 'Alpha',
            configuredIndex: 0,
            recentAt: 10,
            trashed: false,
            conversationCount: 1,
            type: 'character',
        },
    ],
}

const conversationPage: ConversationPage = {
    revision: 4,
    items: [
        {
            id: 'conv-a',
            characterId: 'char-a',
            name: 'First chat',
            configuredIndex: 0,
            recentAt: 10,
            messageCount: 1,
        },
    ],
}

const conversationWindow: ConversationWindow = {
    characterId: 'char-a',
    conversationId: 'conv-a',
    messages: [{ role: 'user', data: 'hello' }],
    startIndex: 0,
    endIndex: 1,
    totalMessages: 1,
    hasMoreBefore: false,
    hasMoreAfter: false,
}

function createHarness() {
    const compatibilityDatabase = {
        username: 'Live user',
        maxContext: 8192,
    } as unknown as Database
    let navigationGeneration = 0
    let selectedCharacterId: string | null = 'active'
    const getSelectedCharacterId = vi.fn(() => selectedCharacterId)
    const materializedDatabases: Database[] = []
    const pinnedDatabases: Database[] = []
    const pinnedCharacterQueries: unknown[] = []
    const archivedCharacterIds = new Set<string>()
    const releasedLeases: Array<ReturnType<typeof vi.fn>> = []
    const authoritativeSnapshots: Array<{
        database: Database
        revision: number
        mutationGeneration?: number
        pluginStorageValues?: Array<{ owner: string; key: string; value: unknown }>
    }> = []
    const readRoot = vi.fn(async () => {
        const database = pinnedDatabases[0]!
        const { characters, botPresets, pluginCustomStorage, ...root } = database
        return { revision: 4, value: root }
    })
    const acquireRevision = vi.fn(async (revision: number) => {
        const database = pinnedDatabases.shift()!
        const characters = database.characters ?? []
        const presets = database.botPresets ?? []
        const pluginStorage = database.pluginCustomStorage ?? {}
        const release = vi.fn(async () => undefined)
        releasedLeases.push(release)
        return {
            revision,
            readRoot: async () => {
                const { characters: _characters, botPresets, pluginCustomStorage, ...root } = database
                return { revision, value: root }
            },
            queryPresets: async () => ({
                revision,
                items: presets.map((preset, configuredIndex) => ({
                    id: String(configuredIndex),
                    name: preset.name,
                    image: preset.image,
                    configuredIndex,
                })),
            }),
            readPreset: async (id: string) => ({ revision, value: presets[Number(id)] }),
            queryCharacters: async ({ trash, limit, cursor }) => {
                pinnedCharacterQueries.push({ trash, limit, cursor })
                const values = characters
                    .map((character, configuredIndex) => ({ character, configuredIndex }))
                    .filter(({ character }) => Boolean(character.trashTime) === trash)
                const start = cursor ? Number(cursor) : 0
                const end = Math.min(start + limit, values.length)
                return {
                    revision,
                    items: values.slice(start, end).map(({ character, configuredIndex }) => ({
                        id: character.chaId,
                        name: character.name,
                        image: character.image,
                        configuredIndex,
                        recentAt: character.lastInteraction ?? 0,
                        trashed: trash,
                        conversationCount: character.chats.length,
                        type: character.type,
                        ...(archivedCharacterIds.has(character.chaId)
                            ? {
                                  archived: {
                                      archivedAt: 10,
                                      conversationCount: character.chats.length,
                                      messageCount: 0,
                                  },
                              }
                            : {}),
                    })),
                    nextCursor: end < values.length ? String(end) : undefined,
                }
            },
            readCharacter: async (id: string) => {
                if (archivedCharacterIds.has(id)) {
                    throw new Error(`Character ${id} is archived`)
                }
                const character = characters.find((value) => value.chaId === id)
                if (!character) return null
                const { chats, ...detail } = character
                return { revision, value: detail }
            },
            queryConversations: async ({ characterId, limit, cursor }) => {
                const chats = characters.find((value) => value.chaId === characterId)?.chats ?? []
                const start = cursor ? Number(cursor) : 0
                const end = Math.min(start + limit, chats.length)
                return {
                    revision,
                    items: chats.slice(start, end).map((chat, index) => ({
                        id: chat.id,
                        characterId,
                        name: chat.name ?? '',
                        configuredIndex: start + index,
                        recentAt: chat.lastDate ?? 0,
                        messageCount: chat.message.length,
                    })),
                    nextCursor: end < chats.length ? String(end) : undefined,
                }
            },
            readConversation: async (characterId: string, conversationId: string) => {
                const chat = characters
                    .find((value) => value.chaId === characterId)
                    ?.chats.find((value) => value.id === conversationId)
                return chat ? { revision, value: chat } : null
            },
            queryPluginStorage: async () => ({
                revision,
                items: Object.keys(pluginStorage).map((key) => ({
                    owner: PLUGIN_ACCESS_OWNER,
                    key,
                    byteSize: 0,
                })),
            }),
            readPluginStorage: async (_owner: string, key: string) =>
                Object.prototype.hasOwnProperty.call(pluginStorage, key)
                    ? { revision, value: pluginStorage[key] }
                    : null,
            release,
        } as unknown as PersistentRevisionLease
    })
    const store = {
        open: vi.fn(async () => undefined),
        queryCharacters: vi.fn(async () => characterPage),
        queryConversations: vi.fn(async () => conversationPage),
        readConversationWindow: vi.fn(async () => ({ revision: 4, value: conversationWindow })),
        materializeDatabase: vi.fn(async () => materializedDatabases.shift()!),
        readRoot,
        readCharacter: vi.fn(),
        readConversation: vi.fn(),
        commit: vi.fn(),
        replaceFromDatabase: vi.fn(),
        acquireRevision,
    } as unknown as PersistentDataStore
    const flushPendingData = vi.fn(async () => undefined)
    const snapshot = vi.fn((value: unknown) => structuredClone(value))
    const applyCompatibilityDatabaseLite = vi.fn((_database: Record<string, unknown>) => undefined)
    const materializeDatabaseSnapshot = vi.fn(async () => {
        const snapshot = authoritativeSnapshots.shift()!
        return {
            ...snapshot,
            mutationGeneration: snapshot.mutationGeneration ?? 0,
        }
    })
    const prepareAuthoritativeDatabaseUpdate = vi.fn(async (
        database: Record<string, unknown>,
    ) => database)
    let authorityEpoch = 0
    let mutationFenced = false
    const assertPersistentMutationAllowed = (expected = authorityEpoch) => {
        if (mutationFenced || expected !== authorityEpoch) throw new Error('Persistent mutation fenced')
    }
    const replacePersistentDatabase = vi.fn(async (
        _database: Database,
        _reason: string,
        _options: {
            authoritative?: boolean
            publishOfficial?: boolean
            expectedRevision?: number
            expectedMutationGeneration?: number
            pluginStorageValues?: Array<{ owner: string; key: string; value: unknown }>
        },
    ) => ({ kind: 'committed' as const, revision: 5, projection: 'applied' as const }))
    const readPluginStorageSnapshot = vi.fn(async () => ({
        '2': 0,
        memory: { retained: true },
    }))
    const mutatePluginStorage = vi.fn(async (
        _mutations: readonly PluginStorageMutation[],
    ) => undefined)
    const invalidatePluginStorage = vi.fn()
    const selectedConversationTarget = {
        characterId: 'active',
        conversationId: 'active-chat-a',
        navigationGeneration: 0,
        storeRevision: 4,
    }
    let selectedTarget: typeof selectedConversationTarget | null = selectedConversationTarget
    const completeConversationRelease = vi.fn()
    const completeConversationSession = {}
    const acquireCompleteConversation = vi.fn(async () => ({
        session: completeConversationSession,
        target: selectedTarget!,
        release: completeConversationRelease,
    }))
    const refreshSelectedConversationAfterReplacement = vi.fn(() => true)
    const invalidateActiveConversationSession = vi.fn()
    const replacePersistentCompleteCharacter = vi.fn(async () => true)
    const replacePersistentConversation = vi.fn(async () => true)
    const reportIdentityReplacementRejected = vi.fn()
    const access = createPluginDatabaseAccess({
        owner: PLUGIN_ACCESS_OWNER,
        store,
        flushPendingData,
        getCompatibilityDatabase: () => compatibilityDatabase,
        getSelectedCharacterId,
        captureSelectedConversationTarget: () => selectedTarget as any,
        acquireCompleteConversation: acquireCompleteConversation as any,
        refreshSelectedConversationAfterReplacement:
            refreshSelectedConversationAfterReplacement as any,
        invalidateActiveConversationSession,
        replacePersistentCompleteCharacter,
        replacePersistentConversation,
        reportIdentityReplacementRejected,
        getNavigationGeneration: () => navigationGeneration,
        getStorageAuthorityEpoch: () => authorityEpoch,
        assertPersistentMutationAllowed,
        applyCompatibilityDatabaseLite,
        materializeDatabaseSnapshot,
        replacePersistentDatabase,
        readPluginStorageSnapshot,
        mutatePluginStorage,
        invalidatePluginStorage,
        prepareAuthoritativeDatabaseUpdate,
        snapshot: <T>(value: T) => snapshot(value) as T,
    })
    return {
        access,
        applyCompatibilityDatabaseLite,
        archivedCharacterIds,
        authoritativeSnapshots,
        compatibilityDatabase,
        flushPendingData,
        materializedDatabases,
        materializeDatabaseSnapshot,
        mutatePluginStorage,
        prepareAuthoritativeDatabaseUpdate,
        readPluginStorageSnapshot,
        invalidatePluginStorage,
        pinnedDatabases,
        pinnedCharacterQueries,
        releasedLeases,
        replacePersistentDatabase,
        replacePersistentCompleteCharacter,
        replacePersistentConversation,
        reportIdentityReplacementRejected,
        acquireCompleteConversation,
        completeConversationSession,
        completeConversationRelease,
        refreshSelectedConversationAfterReplacement,
        invalidateActiveConversationSession,
        getSelectedCharacterId,
        setSelectedCharacterId(id: string | null) {
            selectedCharacterId = id
        },
        setSelectedConversationTarget(target: typeof selectedTarget) {
            selectedTarget = target
        },
        setNavigationGeneration(generation: number) {
            navigationGeneration = generation
        },
        advanceAuthorityEpoch() { authorityEpoch++ },
        setMutationFenced(value: boolean) { mutationFenced = value },
        snapshot,
        store,
    }
}

function makeCharacter(id: string, trashed = false): PluginCompleteCharacter {
    return {
        type: 'character',
        chaId: id,
        name: id,
        ...(trashed ? { trashTime: 1 } : {}),
        chatPage: 0,
        chats: [
            { id: `${id}-chat-a`, name: 'A', message: [{ role: 'user', data: 'a' }] },
            { id: `${id}-chat-b`, name: 'B', message: [{ role: 'char', data: 'b' }] },
        ],
    } as PluginCompleteCharacter
}

function makeFullObjectDatabase(
    characters: PluginCompleteCharacter[] = [
        makeCharacter('active'),
        makeCharacter('trashed', true),
    ],
): Database {
    return { username: 'Fixture', botPresets: [], characters } as unknown as Database
}

function callContext(): PluginFullObjectCallContext {
    return { pluginName: 'fixture-plugin', signal: new AbortController().signal }
}

describe('plugin database access', () => {
    it('late_plugin_result_cannot_cross_replacement during database preparation', async () => {
        const harness = createHarness()
        const prepared = deferred<Record<string, unknown>>()
        harness.prepareAuthoritativeDatabaseUpdate.mockReturnValueOnce(prepared.promise)
        const writing = harness.access.setDatabase({ temperature: 0.5 }, ['temperature'])
        const rejected = expect(writing).rejects.toThrow('Persistent mutation fenced')
        harness.advanceAuthorityEpoch()
        prepared.resolve({ temperature: 0.5 })
        await rejected
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.mutatePluginStorage).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('rejects a late selected-character write even when the new library retains the same IDs', async () => {
        const harness = createHarness()
        const flushed = deferred<void>()
        harness.flushPendingData.mockReturnValueOnce(flushed.promise)
        const writing = harness.access.setCurrentCharacter(makeCharacter('active'), callContext())
        const rejected = expect(writing).rejects.toThrow('Persistent mutation fenced')
        harness.advanceAuthorityEpoch()
        flushed.resolve()
        await rejected
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.replacePersistentConversation).not.toHaveBeenCalled()
    })

    it('blocks synchronous plugin settings before mutating the working set during refresh', () => {
        const harness = createHarness()
        harness.setMutationFenced(true)
        expect(() => harness.access.setDatabaseLite({ temperature: 0.5 }, ['temperature']))
            .toThrow('Persistent mutation fenced')
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.mutatePluginStorage).not.toHaveBeenCalled()
    })

    it('projector overlays durable, dirty resident, and exact live output without flushing', async () => {
        const harness = createHarness()
        const durable = makeCharacter('active')
        durable.name = 'Durable owner'
        durable.chats.push({
            id: 'active-chat-c',
            name: 'C',
            note: 'durable selected',
            localLore: [],
            message: [{ role: 'char', data: 'durable output' }],
        })
        const dirtyResident: PluginCompleteCharacter['chats'][number] = {
            ...structuredClone(durable.chats[1]),
            note: 'dirty resident metadata',
            message: [{ role: 'user' as const, data: 'dirty resident message' }],
        }
        const liveSelected: PluginCompleteCharacter['chats'][number] = {
            ...structuredClone(durable.chats[2]),
            note: 'final live metadata',
            message: [
                ...durable.chats[2].message,
                { role: 'char' as const, data: 'newly generated output' },
            ],
        }
        const liveCharacter: PluginCompleteCharacter = {
            ...structuredClone(durable),
            name: 'Live owner detail',
            chats: [
                createConversationSummaryStubFromChat('active', durable.chats[0], 0),
                dirtyResident,
                liveSelected,
            ],
        }
        harness.pinnedDatabases.push(makeFullObjectDatabase([durable]))
        vi.mocked(getPersistentDataStore).mockReturnValue(harness.store)
        const projector = createProductionPluginChatOutputProjector(structuredClone)

        const projected = await projector({
            characterId: 'active',
            conversationId: 'active-chat-c',
            liveCharacter,
            liveConversation: liveSelected,
        })

        expect(projected.char.name).toBe('Live owner detail')
        expect(projected.char.chats[0].message).toEqual(durable.chats[0].message)
        expect(projected.char.chats[1]).toEqual(dirtyResident)
        expect(projected.char.chats[2]).toEqual(liveSelected)
        expect(projected.chat).toEqual(liveSelected)
        expect(projected.chat).not.toBe(liveSelected)
        expect(harness.releasedLeases).toHaveLength(1)
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('retries opening the projector store after a transient failure', async () => {
        const harness = createHarness()
        const durable = makeCharacter('active')
        const transient = new Error('synthetic projector open failure')
        vi.mocked(harness.store.open)
            .mockRejectedValueOnce(transient)
            .mockResolvedValue(undefined)
        harness.pinnedDatabases.push(makeFullObjectDatabase([durable]))
        vi.mocked(getPersistentDataStore).mockReturnValue(harness.store)
        const projector = createProductionPluginChatOutputProjector(structuredClone)
        const input = {
            characterId: 'active',
            conversationId: 'active-chat-a',
            liveCharacter: durable,
            liveConversation: durable.chats[0],
        }

        await expect(projector(input)).rejects.toBe(transient)
        await expect(projector(input)).resolves.toMatchObject({
            chat: { id: 'active-chat-a' },
        })

        expect(harness.store.open).toHaveBeenCalledTimes(2)
    })

    it('merges active and trash configured order before resolving an index', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push(makeFullObjectDatabase([
            makeCharacter('active-a'),
            makeCharacter('trash-b', true),
            makeCharacter('active-c'),
        ]))

        await expect(harness.access.getCharacterFromIndex(1, callContext()))
            .resolves.toMatchObject({ chaId: 'trash-b' })
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
    })

    // Invariant 9.
    it('reads the whole database without the archived characters and without failing', async () => {
        const harness = createHarness()
        harness.archivedCharacterIds.add('archived-b')
        harness.pinnedDatabases.push(makeFullObjectDatabase([
            makeCharacter('active-a'),
            makeCharacter('archived-b'),
            makeCharacter('active-c'),
        ]))

        const result = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])

        const characters = result.characters as PluginCompleteCharacter[]
        expect(characters.map((character) => character.chaId)).toEqual([
            'active-a',
            'active-c',
        ])
        expect(JSON.stringify(result)).not.toContain('archived-b')
    })

    // Invariants 20 and 27.
    it('resolves every index to the character the whole database read holds there', async () => {
        const harness = createHarness()
        harness.archivedCharacterIds.add('archived-a')
        harness.archivedCharacterIds.add('archived-c')
        const database = () => makeFullObjectDatabase([
            makeCharacter('archived-a'),
            makeCharacter('active-b'),
            makeCharacter('archived-c'),
            makeCharacter('active-d'),
            makeCharacter('trash-e', true),
        ])
        harness.pinnedDatabases.push(database(), database(), database(), database())

        const snapshot = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])
        const characters = snapshot.characters as PluginCompleteCharacter[]
        expect(characters.map((character) => character.chaId)).toEqual([
            'active-b',
            'active-d',
            'trash-e',
        ])
        for (let index = 0; index < characters.length; index += 1) {
            const indexed = await harness.access.getCharacterFromIndex(index, callContext())
            expect(indexed?.chaId).toBe(characters[index].chaId)
        }
    })

    it('resolves no index past the end of the filtered list', async () => {
        const harness = createHarness()
        harness.archivedCharacterIds.add('archived-b')
        harness.pinnedDatabases.push(makeFullObjectDatabase([
            makeCharacter('active-a'),
            makeCharacter('archived-b'),
        ]))

        await expect(harness.access.getCharacterFromIndex(1, callContext())).resolves.toBeNull()
    })

    it('returns exact detached current, indexed character, and indexed chat objects', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database, database)

        const current = await harness.access.getCurrentCharacter(callContext())
        const indexed = await harness.access.getCharacterFromIndex(0, callContext())
        const chat = await harness.access.getChatFromIndex(0, 1, callContext())

        expect(current).toEqual(database.characters[0])
        expect(indexed).toEqual(database.characters[0])
        expect(chat).toEqual(database.characters[0].chats[1])
        current!.name = 'mutated snapshot'
        chat!.message.splice(0)
        expect(database.characters[0].name).not.toBe('mutated snapshot')
        expect(database.characters[0].chats[1].message).not.toHaveLength(0)
        expect(harness.releasedLeases).toHaveLength(3)
        expect(harness.releasedLeases.every((release) => release.mock.calls.length === 1)).toBe(true)
    })

    it('keeps undefined, null, abort, and lease-release contracts', async () => {
        const harness = createHarness()
        harness.setSelectedCharacterId(null)
        await expect(harness.access.getCurrentCharacter(callContext())).resolves.toBeUndefined()

        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database)
        await expect(harness.access.getCharacterFromIndex(99, callContext())).resolves.toBeNull()
        await expect(harness.access.getChatFromIndex(0, 99, callContext())).resolves.toBeNull()

        const controller = new AbortController()
        controller.abort(new DOMException('plugin unloaded', 'AbortError'))
        await expect(harness.access.getCharacterFromIndex(0, {
            pluginName: 'fixture', signal: controller.signal,
        })).rejects.toMatchObject({ name: 'AbortError' })
        expect(harness.releasedLeases.every((release) => release.mock.calls.length === 1)).toBe(true)
    })

    it('uses the pinned character ID when the compatibility array reorders during acquire', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        const acquire = deferred<PersistentRevisionLease>()
        const originalAcquire = harness.store.acquireRevision.bind(harness.store)
        vi.mocked(harness.store.acquireRevision).mockImplementationOnce(() => acquire.promise)
        harness.pinnedDatabases.push(database)

        const pending = harness.access.getCharacterFromIndex(1, callContext())
        await vi.waitFor(() => expect(harness.store.acquireRevision).toHaveBeenCalledOnce())
        harness.compatibilityDatabase.characters = structuredClone(database.characters).reverse()
        acquire.resolve(await originalAcquire(4))

        await expect(pending).resolves.toMatchObject({ chaId: 'trashed' })
    })

    it('polls exact chat getters without full materialization or profile changes', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database, database)

        await harness.access.getChatFromIndex(0, 0, callContext())
        await harness.access.getChatFromIndex(0, 0, callContext())
        await harness.access.getChatFromIndex(0, 0, callContext())

        expect(harness.flushPendingData).toHaveBeenCalledTimes(3)
        expect(harness.store.commit).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.releasedLeases).toHaveLength(3)
    })

    it('captures IDs and expected revision before an indexed character write', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        harness.compatibilityDatabase.characters = structuredClone(database.characters)
        const candidate = structuredClone(database.characters[1])
        candidate.name = 'Updated inactive character'

        const mutation = harness.access.setCharacterToIndex(1, candidate, callContext())
        harness.compatibilityDatabase.characters.reverse()
        await mutation

        expect(harness.replacePersistentCompleteCharacter).toHaveBeenCalledWith(
            database.characters[1].chaId,
            'plugin-setCharacterToIndex',
            expect.any(Function),
            { expectedRevision: 4 },
        )
    })

    it('replaces only the captured conversation and keeps invalid indexes as no-ops', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database)
        const replacement = structuredClone(database.characters[0].chats[1])
        replacement.localLore = [{ key: 'plugin', content: 'saved' } as any]

        await harness.access.setChatToIndex(0, 1, replacement, callContext())
        expect(harness.replacePersistentConversation).toHaveBeenCalledWith(
            'active', replacement.id, 'plugin-setChatToIndex', replacement,
            { expectedRevision: 4 },
        )

        await harness.access.setChatToIndex(99, 99, replacement, callContext())
        expect(harness.replacePersistentConversation).toHaveBeenCalledTimes(1)
    })

    it('keeps the call-boundary current character when navigation changes during flush', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const flushed = deferred<void>()
        harness.flushPendingData.mockReturnValueOnce(flushed.promise)
        const candidate = structuredClone(database.characters[0])
        candidate.name = 'Call-boundary current replacement'

        const writing = harness.access.setCurrentCharacter(candidate, callContext())
        await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
        harness.setSelectedCharacterId('trashed')
        harness.setNavigationGeneration(1)
        harness.setSelectedConversationTarget({
            characterId: 'trashed',
            conversationId: 'trashed-chat-a',
            navigationGeneration: 1,
            storeRevision: 4,
        })
        flushed.resolve(undefined)
        await writing

        expect(harness.replacePersistentCompleteCharacter).toHaveBeenCalledWith(
            'active',
            'plugin-setCharacter',
            expect.any(Function),
            { expectedRevision: 4 },
        )
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.refreshSelectedConversationAfterReplacement).not.toHaveBeenCalled()
    })

    it.each([
        'setCurrentCharacter',
        'setCharacterToIndex',
        'setChatToIndex',
    ] as const)('recaptures a revision-advanced selected target for %s', async (operation) => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const flushed = deferred<void>()
        harness.flushPendingData.mockReturnValueOnce(flushed.promise)
        const nextTarget = {
            characterId: 'active',
            conversationId: 'active-chat-a',
            navigationGeneration: 0,
            storeRevision: 5,
        }
        const writing = operation === 'setCurrentCharacter'
            ? harness.access.setCurrentCharacter(
                structuredClone(database.characters[0]),
                callContext(),
            )
            : operation === 'setCharacterToIndex'
                ? harness.access.setCharacterToIndex(
                    0,
                    structuredClone(database.characters[0]),
                    callContext(),
                )
                : harness.access.setChatToIndex(
                    0,
                    0,
                    structuredClone(database.characters[0].chats[0]),
                    callContext(),
                )
        await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
        harness.setSelectedConversationTarget(nextTarget)
        flushed.resolve(undefined)
        await writing

        expect(harness.acquireCompleteConversation).toHaveBeenCalledWith(
            'plugin-full-object-setter',
            nextTarget,
        )
    })

    it('does not acquire a newer same-ID session after navigation away and back', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const flushed = deferred<void>()
        harness.flushPendingData.mockReturnValueOnce(flushed.promise)
        const replacement = structuredClone(database.characters[0].chats[0])
        replacement.note = 'stable call-boundary chat write'

        const writing = harness.access.setChatToIndex(0, 0, replacement, callContext())
        await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
        harness.setNavigationGeneration(2)
        harness.setSelectedConversationTarget({
            characterId: 'active',
            conversationId: 'active-chat-a',
            navigationGeneration: 2,
            storeRevision: 4,
        })
        flushed.resolve(undefined)
        await writing

        expect(harness.replacePersistentConversation).toHaveBeenCalledWith(
            'active',
            'active-chat-a',
            'plugin-setChatToIndex',
            replacement,
            { expectedRevision: 4 },
        )
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.refreshSelectedConversationAfterReplacement).not.toHaveBeenCalled()
        expect(harness.invalidateActiveConversationSession).toHaveBeenCalledOnce()
    })

    it('does not invalidate the session for a leaseless write to an unselected conversation', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const flushed = deferred<void>()
        harness.flushPendingData.mockReturnValueOnce(flushed.promise)
        const replacement = structuredClone(database.characters[0].chats[1])

        const writing = harness.access.setChatToIndex(0, 1, replacement, callContext())
        await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
        harness.setNavigationGeneration(2)
        harness.setSelectedConversationTarget({
            characterId: 'active',
            conversationId: 'active-chat-a',
            navigationGeneration: 2,
            storeRevision: 4,
        })
        flushed.resolve(undefined)
        await writing

        expect(harness.replacePersistentConversation).toHaveBeenCalledOnce()
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.invalidateActiveConversationSession).not.toHaveBeenCalled()
    })

    it('fulfills invalid current and indexed setters without promotion or mutation', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database)
        harness.setSelectedCharacterId(null)

        await expect(harness.access.setCurrentCharacter(
            structuredClone(database.characters[0]),
            callContext(),
        )).resolves.toBeUndefined()
        await expect(harness.access.setCharacterToIndex(
            99,
            structuredClone(database.characters[0]),
            callContext(),
        )).resolves.toBeUndefined()
        await expect(harness.access.setChatToIndex(
            0,
            99,
            structuredClone(database.characters[0].chats[0]),
            callContext(),
        )).resolves.toBeUndefined()

        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.replacePersistentConversation).not.toHaveBeenCalled()
    })

    it('holds a selected windowed lease until the scoped replacement settles', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const replacement = structuredClone(database.characters[0])
        replacement.name = 'Selected replacement'
        const persistence = deferred<boolean>()
        harness.replacePersistentCompleteCharacter.mockReturnValueOnce(persistence.promise)

        const writing = harness.access.setCurrentCharacter(replacement, callContext())
        await vi.waitFor(() => expect(harness.acquireCompleteConversation).toHaveBeenCalledOnce())
        expect(harness.completeConversationRelease).not.toHaveBeenCalled()
        persistence.resolve(true)
        await writing
        expect(harness.completeConversationRelease).toHaveBeenCalledOnce()
        expect(harness.refreshSelectedConversationAfterReplacement).toHaveBeenCalledWith(
            expect.anything(),
            harness.completeConversationSession,
        )
        expect(harness.completeConversationRelease.mock.invocationCallOrder[0]).toBeLessThan(
            harness.refreshSelectedConversationAfterReplacement.mock.invocationCallOrder[0],
        )
    })

    it.each([
        ['stale target', false],
        ['store failure', new Error('store failed')],
    ] as const)('releases the selected lease once after %s', async (_name, outcome) => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const replacement = structuredClone(database.characters[0])
        if (outcome instanceof Error) {
            harness.replacePersistentCompleteCharacter.mockRejectedValueOnce(outcome)
        } else {
            harness.replacePersistentCompleteCharacter.mockResolvedValueOnce(outcome)
        }

        await expect(harness.access.setCurrentCharacter(replacement, callContext())).rejects
            .toBeInstanceOf(Error)
        expect(harness.completeConversationRelease).toHaveBeenCalledOnce()
        expect(harness.refreshSelectedConversationAfterReplacement).toHaveBeenCalledOnce()
    })

    it('does not mutate or leak a lease when selected promotion becomes stale', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        harness.acquireCompleteConversation.mockRejectedValueOnce(
            new Error('selected promotion became stale'),
        )

        await expect(harness.access.setCurrentCharacter(
            structuredClone(database.characters[0]),
            callContext(),
        )).rejects.toThrow('selected promotion became stale')

        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.completeConversationRelease).not.toHaveBeenCalled()
        expect(harness.refreshSelectedConversationAfterReplacement).not.toHaveBeenCalled()
    })

    it('releases the selected lease without mutation when unload aborts promotion', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const promoted = deferred<any>()
        harness.acquireCompleteConversation.mockReturnValueOnce(promoted.promise)
        const controller = new AbortController()
        const writing = harness.access.setCurrentCharacter(
            structuredClone(database.characters[0]),
            { pluginName: 'fixture-plugin', signal: controller.signal },
        )
        await vi.waitFor(() => expect(harness.acquireCompleteConversation).toHaveBeenCalledOnce())
        controller.abort(new Error('plugin unloaded'))
        promoted.resolve({
            session: harness.completeConversationSession,
            target: { characterId: 'active', conversationId: 'active-chat-a' },
            release: harness.completeConversationRelease,
        })

        await expect(writing).rejects.toThrow('plugin unloaded')
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.completeConversationRelease).toHaveBeenCalledOnce()
        expect(harness.refreshSelectedConversationAfterReplacement).toHaveBeenCalledOnce()
    })

    it('refreshes through the captured lease after navigation changes while a write is pending', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const persistence = deferred<boolean>()
        harness.replacePersistentCompleteCharacter.mockReturnValueOnce(persistence.promise)

        const writing = harness.access.setCurrentCharacter(
            structuredClone(database.characters[0]),
            callContext(),
        )
        await vi.waitFor(() => expect(harness.acquireCompleteConversation).toHaveBeenCalledOnce())
        harness.setNavigationGeneration(1)
        harness.setSelectedConversationTarget({
            characterId: 'active',
            conversationId: 'active-chat-b',
            navigationGeneration: 1,
            storeRevision: 4,
        })
        persistence.resolve(true)
        await writing

        expect(harness.refreshSelectedConversationAfterReplacement).toHaveBeenCalledWith(
            expect.objectContaining({
                characterId: 'active',
                conversationId: 'active-chat-a',
            }),
            harness.completeConversationSession,
        )
    })

    it.each([
        ['setCurrentCharacter', 'chaId', 'replacement-current-character-id'],
        ['setCharacterToIndex', 'chaId', 'replacement-character-id'],
        ['setChatToIndex', 'id', 'replacement-chat-id'],
    ] as const)('rejects %s ID replacement with a structured diagnostic', async (
        operation,
        idField,
        attemptedId,
    ) => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const target = operation === 'setChatToIndex'
            ? structuredClone(database.characters[0].chats[0])
            : structuredClone(database.characters[0])
        ;(target as any)[idField] = attemptedId

        const write = operation === 'setCurrentCharacter'
            ? harness.access.setCurrentCharacter(target as PluginCompleteCharacter, callContext())
            : operation === 'setCharacterToIndex'
                ? harness.access.setCharacterToIndex(
                    0,
                    target as PluginCompleteCharacter,
                    callContext(),
                )
                : harness.access.setChatToIndex(0, 0, target as any, callContext())
        await expect(write).rejects.toBeInstanceOf(PluginIdentityReplacementRejectedError)
        expect(harness.reportIdentityReplacementRejected).toHaveBeenCalledWith({
            kind: 'plugin-identity-replacement-rejected',
            pluginName: 'fixture-plugin',
            operation: operation === 'setCurrentCharacter' ? 'setCharacter' : operation,
            targetId: operation === 'setChatToIndex' ? 'active-chat-a' : 'active',
            attemptedId,
        })
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.replacePersistentConversation).not.toHaveBeenCalled()
    })

    it('rejects malformed scoped replacements before promotion or mutation', async () => {
        const harness = createHarness()

        await expect(harness.access.setCurrentCharacter(null as any, callContext()))
            .rejects.toBeInstanceOf(TypeError)
        await expect(harness.access.setCurrentCharacter({
            type: 'character', chaId: '', chats: [],
        } as any, callContext())).rejects.toBeInstanceOf(TypeError)
        await expect(harness.access.setCurrentCharacter({
            type: 'character', chaId: 'active', chats: null,
        } as any, callContext())).rejects.toBeInstanceOf(TypeError)
        await expect(harness.access.setChatToIndex(
            0,
            0,
            null as any,
            callContext(),
        )).rejects.toBeInstanceOf(TypeError)
        await expect(harness.access.setChatToIndex(
            0,
            0,
            { id: '', message: [] } as any,
            callContext(),
        )).rejects.toBeInstanceOf(TypeError)
        await expect(harness.access.setChatToIndex(
            0,
            0,
            { id: 'active-chat-a', message: null } as any,
            callContext(),
        )).rejects.toBeInstanceOf(TypeError)
        expect(harness.reportIdentityReplacementRejected).not.toHaveBeenCalled()
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.replacePersistentConversation).not.toHaveBeenCalled()
    })

    it('composes scalable API v3 queries with the shared production store', async () => {
        const harness = createHarness()
        vi.mocked(getPersistentDataStore).mockReturnValue(harness.store)
        Object.defineProperty(harness.compatibilityDatabase, 'characters', {
            get() {
                throw new Error('scalable query touched DBState.db.characters')
            },
        })
        const access = createProductionPluginDatabaseAccess({
        owner: PLUGIN_ACCESS_OWNER,
            flushPendingData: harness.flushPendingData,
            getCompatibilityDatabase: () => harness.compatibilityDatabase,
            getSelectedCharacterId: harness.getSelectedCharacterId,
            captureSelectedConversationTarget: () => null,
            acquireCompleteConversation: vi.fn(),
            refreshSelectedConversationAfterReplacement: vi.fn(),
            replacePersistentCompleteCharacter: vi.fn(),
            replacePersistentConversation: vi.fn(),
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: () => 0,
            getStorageAuthorityEpoch: () => 0,
            assertPersistentMutationAllowed: vi.fn(),
            applyCompatibilityDatabaseLite: harness.applyCompatibilityDatabaseLite,
            readPluginStorageSnapshot: harness.readPluginStorageSnapshot,
            mutatePluginStorage: harness.mutatePluginStorage,
            invalidatePluginStorage: harness.invalidatePluginStorage,
            materializeDatabaseSnapshot: harness.materializeDatabaseSnapshot,
            replacePersistentDatabase: harness.replacePersistentDatabase,
            prepareAuthoritativeDatabaseUpdate: harness.prepareAuthoritativeDatabaseUpdate,
            snapshot: <T>(value: T) => harness.snapshot(value) as T,
        })

        await expect(access.queryCharacters({ limit: 500 })).resolves.toEqual(characterPage)

        expect(harness.store.queryCharacters).toHaveBeenCalledWith({
            search: undefined,
            order: 'configured',
            trash: false,
            limit: 100,
            cursor: undefined,
        })
        expect(harness.snapshot).not.toHaveBeenCalled()
    })

    it('runs scalable queries without touching the compatibility character array', async () => {
        const harness = createHarness()
        Object.defineProperty(harness.compatibilityDatabase, 'characters', {
            get() {
                throw new Error('scalable query touched DBState.db.characters')
            },
        })

        expect(await harness.access.queryCharacters({ limit: 10 })).toEqual(characterPage)
        expect(
            await harness.access.queryConversations({ characterId: 'char-a', limit: 10 }),
        ).toEqual(conversationPage)
        expect(
            await harness.access.queryConversationMessages({
                characterId: 'char-a',
                conversationId: 'conv-a',
                limit: 10,
            }),
        ).toEqual({ ...conversationWindow, revision: 4 })
        expect(harness.snapshot).not.toHaveBeenCalled()
        expect(harness.store.open).toHaveBeenCalledTimes(3)
        expect(harness.store.readRoot).not.toHaveBeenCalled()
        expect(harness.store.readCharacter).not.toHaveBeenCalled()
        expect(harness.store.readConversation).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('retries opening the store after a transient query failure', async () => {
        const harness = createHarness()
        const transient = new Error('synthetic open failure')
        vi.mocked(harness.store.open)
            .mockRejectedValueOnce(transient)
            .mockResolvedValue(undefined)

        await expect(harness.access.queryCharacters()).rejects.toBe(transient)
        await expect(harness.access.queryCharacters()).resolves.toEqual(characterPage)

        expect(harness.store.open).toHaveBeenCalledTimes(2)
        expect(harness.store.queryCharacters).toHaveBeenCalledOnce()
    })

    it('finishes each flush before the matching persistent query begins', async () => {
        const harness = createHarness()
        const events: string[] = []
        vi.mocked(harness.flushPendingData).mockImplementation(async () => {
            events.push('flush')
        })
        vi.mocked(harness.store.queryCharacters).mockImplementation(async () => {
            events.push('characters')
            return characterPage
        })
        vi.mocked(harness.store.queryConversations).mockImplementation(async () => {
            events.push('conversations')
            return conversationPage
        })
        vi.mocked(harness.store.readConversationWindow).mockImplementation(async () => {
            events.push('messages')
            return { revision: 4, value: conversationWindow }
        })

        await harness.access.queryCharacters()
        await harness.access.queryConversations({ characterId: 'char-a' })
        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
        })

        expect(events).toEqual([
            'flush',
            'characters',
            'flush',
            'conversations',
            'flush',
            'messages',
        ])
        expect(harness.flushPendingData).toHaveBeenNthCalledWith(1, 'plugin-database-query')
    })

    it('applies documented defaults and clamps maximum query sizes', async () => {
        const harness = createHarness()

        await harness.access.queryCharacters()
        await harness.access.queryCharacters({ limit: 500 })
        await harness.access.queryConversations({ characterId: ' char-a ', limit: 500 })
        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
        })
        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            limit: 500,
        })

        expect(harness.store.queryCharacters).toHaveBeenNthCalledWith(1, {
            order: 'configured',
            trash: false,
            limit: 50,
        })
        expect(harness.store.queryCharacters).toHaveBeenNthCalledWith(2, {
            order: 'configured',
            trash: false,
            limit: 100,
        })
        expect(harness.store.queryConversations).toHaveBeenCalledWith({
            characterId: ' char-a ',
            order: 'configured',
            limit: 100,
        })
        expect(harness.store.readConversationWindow).toHaveBeenNthCalledWith(1, {
            characterId: 'char-a',
            conversationId: 'conv-a',
            limit: 128,
        })
        expect(harness.store.readConversationWindow).toHaveBeenNthCalledWith(2, {
            characterId: 'char-a',
            conversationId: 'conv-a',
            limit: 128,
        })
    })

    it.each([
        () => ({ kind: 'conversations', input: { characterId: '' } }),
        () => ({ kind: 'conversations', input: { characterId: '   ' } }),
        () => ({ kind: 'characters', input: { limit: 1.5 } }),
        () => ({ kind: 'characters', input: { limit: 0 } }),
        () => ({ kind: 'messages', input: { characterId: '', conversationId: 'conv-a' } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: '' } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', before: 1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: '', before: 1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: 'm', limit: 1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: 'm', before: -1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: 'm', before: 64, after: 64 } }),
    ])('rejects invalid query input before persistence access', async (makeCase) => {
        const harness = createHarness()
        const testCase = makeCase()
        const operation =
            testCase.kind === 'characters'
                ? harness.access.queryCharacters(testCase.input as never)
                : testCase.kind === 'conversations'
                  ? harness.access.queryConversations(testCase.input as never)
                  : harness.access.queryConversationMessages(testCase.input as never)

        await expect(operation).rejects.toBeInstanceOf(RangeError)
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.store.queryCharacters).not.toHaveBeenCalled()
        expect(harness.store.queryConversations).not.toHaveBeenCalled()
        expect(harness.store.readConversationWindow).not.toHaveBeenCalled()
    })

    it('unwraps versioned message windows and preserves missing windows', async () => {
        const harness = createHarness()
        expect(
            await harness.access.queryConversationMessages({
                characterId: 'char-a',
                conversationId: 'conv-a',
                anchorMessageId: 'message-a',
                before: 2,
                after: 3,
            }),
        ).toEqual({ ...conversationWindow, revision: 4 })
        vi.mocked(harness.store.readConversationWindow).mockResolvedValueOnce(null)
        expect(
            await harness.access.queryConversationMessages({
                characterId: 'char-a',
                conversationId: 'missing',
            }),
        ).toBeNull()
    })

    it('returns revision evidence so callers can reject pages split by a commit', async () => {
        const harness = createHarness()
        const firstWindow: ConversationWindow = {
            ...conversationWindow,
            messages: [
                { role: 'user', data: 'first', chatId: 'message-0' },
                { role: 'char', data: 'shared', chatId: 'message-1' },
            ],
            startIndex: 0,
            endIndex: 2,
            totalMessages: 4,
            hasMoreAfter: true,
        }
        const secondWindow: ConversationWindow = {
            ...conversationWindow,
            messages: [
                { role: 'char', data: 'shared', chatId: 'message-1' },
                { role: 'user', data: 'last', chatId: 'message-2' },
            ],
            startIndex: 2,
            endIndex: 4,
            totalMessages: 4,
            hasMoreBefore: true,
        }
        vi.mocked(harness.store.readConversationWindow)
            .mockResolvedValueOnce({ revision: 4, value: firstWindow })
            .mockResolvedValueOnce({ revision: 5, value: secondWindow })

        const first = await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            startIndex: 0,
            limit: 2,
        })
        const second = await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            startIndex: 2,
            limit: 2,
        })

        expect(first).toEqual({ ...firstWindow, revision: 4 })
        expect(second).toEqual({ ...secondWindow, revision: 5 })
        expect(first?.revision).not.toBe(second?.revision)
        expect(first?.messages.map((message) => message.chatId)).toEqual([
            'message-0',
            'message-1',
        ])
        expect(second?.messages.map((message) => message.chatId)).toEqual([
            'message-1',
            'message-2',
        ])
    })

    it('rejects an already cancelled message query before flushing', async () => {
        const harness = createHarness()
        const controller = new AbortController()
        controller.abort()

        await expect(harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            signal: controller.signal,
        })).rejects.toMatchObject({ name: 'AbortError' })
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.store.open).not.toHaveBeenCalled()
        expect(harness.store.readConversationWindow).not.toHaveBeenCalled()
    })

    it('does not open persistence when cancelled while pending data flushes', async () => {
        const harness = createHarness()
        const controller = new AbortController()
        const flush = deferred<void>()
        vi.mocked(harness.flushPendingData).mockReturnValueOnce(flush.promise)

        const result = harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            signal: controller.signal,
        })
        await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
        controller.abort()
        flush.resolve()

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(harness.store.open).not.toHaveBeenCalled()
        expect(harness.store.readConversationWindow).not.toHaveBeenCalled()
    })

    it('does not start a read when plugin lifetime ends while persistence opens', async () => {
        const harness = createHarness()
        const caller = new AbortController()
        const pluginLifetime = new AbortController()
        const linked = linkPluginQueryAbortSignals(caller.signal, pluginLifetime.signal)
        const opening = deferred<void>()
        vi.mocked(harness.store.open).mockReturnValueOnce(opening.promise)

        const result = harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            signal: linked.signal,
        })
        await vi.waitFor(() => expect(harness.store.open).toHaveBeenCalledOnce())
        pluginLifetime.abort()
        opening.resolve()

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        linked.dispose()
        expect(harness.store.readConversationWindow).not.toHaveBeenCalled()
    })

    it('does not return late success when cancelled during the message read', async () => {
        const harness = createHarness()
        const controller = new AbortController()
        const reading = deferred<{
            revision: number
            value: ConversationWindow
        }>()
        vi.mocked(harness.store.readConversationWindow).mockReturnValueOnce(reading.promise)

        const result = harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            signal: controller.signal,
        })
        await vi.waitFor(() => expect(harness.store.readConversationWindow).toHaveBeenCalledOnce())
        controller.abort()
        reading.resolve({ revision: 4, value: conversationWindow })

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
    })

    it('passes a bounded absolute message range without materializing a conversation', async () => {
        const harness = createHarness()

        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            startIndex: 4_096,
            limit: 500,
        })

        expect(harness.store.readConversationWindow).toHaveBeenCalledWith({
            characterId: 'char-a',
            conversationId: 'conv-a',
            startIndex: 4_096,
            limit: 128,
        })
        expect(harness.store.readConversation).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
    })

    it.each([
        { startIndex: -1, limit: 1 },
        { startIndex: 1.5, limit: 1 },
        { startIndex: 0 },
        { startIndex: 0, limit: 1, anchorMessageId: 'message-a' },
        { startIndex: 0, limit: 1, before: 1 },
        { startIndex: 0, limit: 1, after: 1 },
    ])('rejects invalid absolute message range %# before persistence access', async (range) => {
        const harness = createHarness()

        await expect(harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            ...range,
        })).rejects.toBeInstanceOf(RangeError)
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.store.readConversationWindow).not.toHaveBeenCalled()
    })

    it('snapshots root-only compatibility keys without opening persistence', async () => {
        const harness = createHarness()

        await expect(
            harness.access.getDatabaseSnapshot(['username', 'maxContext'], [
                'characters',
                'username',
                'maxContext',
            ]),
        ).resolves.toEqual({ username: 'Live user', maxContext: 8192 })
        expect(harness.store.open).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.flushPendingData).not.toHaveBeenCalled()
    })

    it('hydrates requested scalable presets without traversing characters', async () => {
        const harness = createHarness()
        const durablePresets = [
            { name: 'First', image: 'first.png', mainPrompt: 'full first body' },
            { name: 'Second', image: 'second.png', mainPrompt: 'full second body' },
        ] as Database['botPresets']
        const catalog = {
            revision: 4,
            items: durablePresets.map((preset, configuredIndex) => ({
                id: String(configuredIndex),
                configuredIndex,
                name: preset.name,
                image: preset.image,
            })),
        }
        harness.compatibilityDatabase.botPresets = createCatalogPresetWorkingSet(catalog, null)
        harness.pinnedDatabases.push({
            characters: [makeCharacter('must-not-be-read')],
            botPresets: durablePresets,
        } as unknown as Database)

        const result = await harness.access.getDatabaseSnapshot(
            ['botPresets', 'username'],
            ['characters', 'botPresets'],
        )

        expect(result).toEqual({ botPresets: durablePresets })
        expect(harness.pinnedCharacterQueries).toEqual([])
        expect(harness.store.readCharacter).not.toHaveBeenCalled()
        const resultPresets = result.botPresets as Database['botPresets']
        resultPresets[0].mainPrompt = 'mutated snapshot'
        expect(durablePresets[0].mainPrompt).toBe('full first body')
        expect(harness.compatibilityDatabase.botPresets[0]).toEqual({
            name: 'First',
            image: 'first.png',
        })
    })

    it('reads requested scalable plugin storage through the per-key authority', async () => {
        const harness = createHarness()
        Object.defineProperty(harness.compatibilityDatabase, 'pluginCustomStorage', {
            get() {
                throw new Error('scalable plugin storage touched the compatibility root')
            },
        })

        await expect(harness.access.getDatabaseSnapshot(
            ['pluginCustomStorage'],
            ['pluginCustomStorage', 'username'],
        )).resolves.toEqual({
            pluginCustomStorage: {
                '2': 0,
                memory: { retained: true },
            },
        })
        expect(harness.readPluginStorageSnapshot).toHaveBeenCalledOnce()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.store.queryCharacters).not.toHaveBeenCalled()
        expect(harness.store.readCharacter).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('builds the explicit scalable character result directly from one pinned reader', async () => {
        const harness = createHarness()
        const pinned = {
            username: 'Persisted user from the same revision',
            characters: [{
                type: 'character',
                chaId: 'persisted-character',
                name: 'Persisted character',
                chats: [{ id: 'persisted-chat', name: 'Chat', message: [] }],
            }],
        } as unknown as Database
        harness.pinnedDatabases.push(pinned)
        Object.defineProperty(harness.compatibilityDatabase, 'characters', {
            get() {
                throw new Error('full snapshot read compatibility characters')
            },
        })

        await expect(
            harness.access.getDatabaseSnapshot('all', ['characters', 'username', 'unknown']),
        ).resolves.toEqual({
            characters: pinned.characters,
            username: 'Persisted user from the same revision',
            unknown: undefined,
        })
        expect(harness.flushPendingData).toHaveBeenCalledWith('plugin-full-database-snapshot')
        expect(harness.store.open).toHaveBeenCalledTimes(1)
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).toHaveBeenCalledWith(4)
        expect(harness.releasedLeases[0]).toHaveBeenCalledTimes(1)
    })

    it('retries a transient explicit snapshot release within the same request', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push({
            characters: [],
            username: 'Pinned user',
        } as unknown as Database)
        const acquireRevision = vi.mocked(harness.store.acquireRevision)
        const acquirePinnedReader = acquireRevision.getMockImplementation()!
        acquireRevision.mockImplementationOnce(async (revision) => {
            const reader = await acquirePinnedReader(revision)
            harness.releasedLeases[0]
                .mockRejectedValueOnce(new Error('release unavailable'))
                .mockResolvedValueOnce(undefined)
            return reader
        })

        await expect(harness.access.getDatabaseSnapshot(
            ['characters', 'username'],
            ['characters', 'username'],
        )).resolves.toEqual({ characters: [], username: 'Pinned user' })
        expect(harness.releasedLeases[0]).toHaveBeenCalledTimes(2)
    })

    it('retries a scalable character snapshot when current revision acquisition races a commit', async () => {
        const harness = createHarness()
        const stale = {
            username: 'Stale',
            characters: [{ type: 'character', chaId: 'stale', name: 'Stale', chats: [] }],
        } as unknown as Database
        const current = {
            username: 'Current',
            characters: [{ type: 'character', chaId: 'current', name: 'Current', chats: [] }],
        } as unknown as Database
        harness.pinnedDatabases.push(stale, current)
        vi.mocked(harness.store.readRoot)
            .mockResolvedValueOnce({ revision: 4, value: { username: 'Stale' } as never })
            .mockResolvedValueOnce({ revision: 5, value: { username: 'Current' } as never })
        const acquireRevision = vi.mocked(harness.store.acquireRevision)
        const acquirePinnedReader = acquireRevision.getMockImplementation()!
        acquireRevision
            .mockImplementationOnce(async () => {
                harness.pinnedDatabases.shift()
                throw new RevisionConflictError(4, 5)
            })
            .mockImplementationOnce(acquirePinnedReader)

        await expect(harness.access.getDatabaseSnapshot(
            ['characters', 'username'],
            ['characters', 'username'],
        )).resolves.toEqual({
            characters: current.characters,
            username: 'Current',
        })
        expect(acquireRevision.mock.calls.map(([revision]) => revision)).toEqual([4, 5])
        expect(harness.releasedLeases[0]).toHaveBeenCalledTimes(1)
    })

    it('bounds scalable character snapshot revision acquisition retries', async () => {
        const harness = createHarness()
        vi.mocked(harness.store.readRoot).mockResolvedValue({
            revision: 4,
            value: { username: 'Racing' } as never,
        })
        vi.mocked(harness.store.acquireRevision).mockRejectedValue(
            new RevisionConflictError(4, 5),
        )

        await expect(harness.access.getDatabaseSnapshot(
            ['characters'],
            ['characters'],
        )).rejects.toBeInstanceOf(RevisionConflictError)
        expect(harness.store.acquireRevision).toHaveBeenCalledTimes(3)
    })

    it('does not retain pinned character results between calls', async () => {
        const harness = createHarness()
        const first = {
            characters: [{ type: 'character', chaId: 'first', name: 'First', chats: [] }],
        } as unknown as Database
        const second = {
            characters: [{ type: 'character', chaId: 'second', name: 'Second', chats: [] }],
        } as unknown as Database
        harness.pinnedDatabases.push(first, second)

        const firstResult = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])
        const secondResult = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])

        expect(firstResult.characters).toEqual(first.characters)
        expect(secondResult.characters).toEqual(second.characters)
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).toHaveBeenCalledTimes(2)
    })

    it('preserves a falsy plugin value in an explicit scalable character snapshot', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push({
            characters: [],
            pluginCustomStorage: { zero: 0 },
        } as unknown as Database)

        await expect(harness.access.getDatabaseSnapshot(
            ['characters', 'pluginCustomStorage'],
            ['characters', 'pluginCustomStorage'],
        )).resolves.toEqual({
            characters: [],
            pluginCustomStorage: { zero: 0 },
        })
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
    })

    it('preserves a JSON-origin own proto key in the paged plugin snapshot', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push({
            characters: [],
            pluginCustomStorage: JSON.parse(
                '{"0":0,"zeta":false,"__proto__":{"safe":true},"alpha":""}',
            ),
        } as unknown as Database)

        const result = await harness.access.getDatabaseSnapshot(
            ['characters', 'pluginCustomStorage'],
            ['characters', 'pluginCustomStorage'],
        )
        const storage = result.pluginCustomStorage as Record<string, unknown>

        expect(result.characters).toEqual([])
        expect(Object.keys(storage)).toEqual([
            '0',
            'zeta',
            '__proto__',
            'alpha',
        ])
        expect(Object.hasOwn(storage, '__proto__')).toBe(true)
        expect(storage.__proto__).toEqual({ safe: true })
        expect(Object.getPrototypeOf(storage)).toBe(Object.prototype)
        expect(storage['0']).toBe(0)
        expect(storage.zeta).toBe(false)
        expect(storage.alpha).toBe('')
    })

    it('preserves an explicit snapshot failure when both release attempts fail', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push({
            characters: [{
                type: 'character',
                chaId: 'broken',
                name: 'Broken',
                chats: [{ id: 'missing', name: 'Missing', message: [] }],
            }],
        } as unknown as Database)
        const acquireRevision = vi.mocked(harness.store.acquireRevision)
        const acquirePinnedReader = acquireRevision.getMockImplementation()!
        acquireRevision.mockImplementationOnce(async (revision) => {
            const reader = await acquirePinnedReader(revision)
            reader.readConversation = vi.fn(async () => null)
            harness.releasedLeases[0].mockRejectedValue(new Error('release unavailable'))
            return reader
        })

        await expect(harness.access.getDatabaseSnapshot(
            ['characters'],
            ['characters'],
        )).rejects.toThrow('Missing conversation missing')
        expect(harness.releasedLeases[0]).toHaveBeenCalledTimes(2)
    })

    it('omits unapproved selected keys and snapshots live non-character values', async () => {
        const harness = createHarness()

        await expect(
            harness.access.getDatabaseSnapshot(['username', 'secret'], ['username']),
        ).resolves.toEqual({ username: 'Live user' })
        expect(harness.snapshot).toHaveBeenCalledWith('Live user')
    })

    it('replaces scalable character updates from a detached authoritative snapshot', async () => {
        const harness = createHarness()
        const authoritative = {
            username: 'Before',
            botPresets: [{ name: 'Preserved preset', prompt: 'preset body' }],
            characters: [
                {
                    chaId: 'active',
                    name: 'Active',
                    chats: [{ id: 'active-chat', message: [{ role: 'user', data: 'keep active' }] }],
                },
                {
                    chaId: 'inactive',
                    name: 'Inactive',
                    chats: [{ id: 'inactive-chat', message: [{ role: 'char', data: 'keep inactive' }] }],
                },
            ],
        } as unknown as Database
        harness.authoritativeSnapshots.push({ database: authoritative, revision: 7 })
        const pluginCharacters = structuredClone(authoritative.characters)
        pluginCharacters[1].name = 'Edited while inactive'

        await harness.access.setDatabase(
            { characters: pluginCharacters, username: 'After', privateValue: 42 },
            ['characters', 'username'],
        )

        expect(harness.materializeDatabaseSnapshot).toHaveBeenCalledWith(
            'plugin-database-set',
            { includePluginStorageValues: true },
        )
        expect(harness.replacePersistentDatabase).toHaveBeenCalledTimes(1)
        const [candidate, reason, options] = harness.replacePersistentDatabase.mock.calls[0]
        expect(reason).toBe('plugin-database-set')
        expect(options).toEqual({
            authoritative: true,
            publishOfficial: true,
            expectedRevision: 7,
            expectedMutationGeneration: 0,
        })
        expect(candidate.username).toBe('After')
        expect(candidate.characters[1].name).toBe('Edited while inactive')
        expect(candidate.characters[0].chats[0].message[0].data).toBe('keep active')
        expect(candidate.characters[1].chats[0].message[0].data).toBe('keep inactive')
        expect(candidate.botPresets).toEqual(authoritative.botPresets)
        expect(candidate.pluginCustomStorage.privateValue).toBe(42)
        expect(candidate).not.toHaveProperty('privateValue')
        expect(candidate).not.toBe(authoritative)
        expect(candidate.characters).not.toBe(pluginCharacters)
    })

    it('keeps same-key values from other plugin owners during a full replacement', async () => {
        const harness = createHarness()
        const authoritative = {
            username: 'Before',
            botPresets: [],
            characters: [{ chaId: 'inactive', name: 'Before', chats: [] }],
            pluginCustomStorage: { shared: 'plugin-a-value' },
        } as unknown as Database
        harness.authoritativeSnapshots.push({
            database: authoritative,
            revision: 7,
            pluginStorageValues: [
                { owner: PLUGIN_ACCESS_OWNER, key: 'shared', value: 'plugin-a-value' },
                { owner: 'plugin-b', key: 'shared', value: 'plugin-b-value' },
            ],
        })

        await harness.access.setDatabase(
            {
                characters: [{ chaId: 'inactive', name: 'After', chats: [] }],
                pluginCustomStorage: { shared: 'plugin-a-updated' },
            },
            ['characters', 'pluginCustomStorage'],
        )

        const [, , options] = harness.replacePersistentDatabase.mock.calls[0]
        expect(options.pluginStorageValues).toHaveLength(2)
        expect(options.pluginStorageValues).toEqual(expect.arrayContaining([
            { owner: PLUGIN_ACCESS_OWNER, key: 'shared', value: 'plugin-a-updated' },
            { owner: 'plugin-b', key: 'shared', value: 'plugin-b-value' },
        ]))
    })

    it('waits for authoritative replacement so scalable live reprojection is observable', async () => {
        const harness = createHarness()
        harness.authoritativeSnapshots.push({
            database: {
                characters: [{ chaId: 'inactive', name: 'Before', chats: [] }],
                botPresets: [],
            } as unknown as Database,
            revision: 8,
        })
        vi.mocked(harness.replacePersistentDatabase).mockImplementation(async () => {
            harness.compatibilityDatabase.characters = [
                { chaId: 'inactive', name: 'Projected', chats: [] },
            ] as never
        })

        await harness.access.setDatabase(
            { characters: [{ chaId: 'inactive', name: 'After', chats: [] }] },
            ['characters'],
        )

        expect(harness.compatibilityDatabase.characters[0].name).toBe('Projected')
    })

    it('rejects synchronous scalable character updates before live mutation', () => {
        const harness = createHarness()

        expect(() => harness.access.setDatabaseLite(
            { characters: [{ chaId: 'inactive', chats: [] }] },
            ['characters'],
        )).toThrow(/async setDatabase/i)
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('rejects incomplete scalable character values before materialization', async () => {
        const harness = createHarness()

        await expect(harness.access.setDatabase(
            { characters: [{ chaId: 'catalog-stub', chats: [{ id: 'summary-only' }] }] },
            ['characters'],
        )).rejects.toThrow(/not fully hydrated/i)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('rejects live catalog stubs instead of merging them into the full snapshot', async () => {
        const harness = createHarness()
        const stub = createCatalogCharacterStub({
            id: 'catalog-stub',
            name: 'Catalog stub',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 3,
            type: 'character',
        })

        await expect(harness.access.setDatabase(
            { characters: [stub] },
            ['characters'],
        )).rejects.toThrow(/catalog working-set stubs/i)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('keeps synchronous root-only setters on the observer path and serializes async updates', async () => {
        const harness = createHarness()
        const liteUpdate = { username: 'Lite' }
        const asyncUpdate = { username: 'Async' }
        harness.authoritativeSnapshots.push({
            database: {
                username: 'Before',
                characters: [],
                botPresets: [],
            } as unknown as Database,
            revision: 9,
        })

        harness.access.setDatabaseLite(liteUpdate, ['characters', 'username'])
        await harness.access.setDatabase(asyncUpdate, ['characters', 'username'])

        expect(harness.applyCompatibilityDatabaseLite).toHaveBeenCalledWith(liteUpdate)
        expect(harness.applyCompatibilityDatabaseLite).toHaveBeenCalledWith(asyncUpdate)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.flushPendingData).toHaveBeenCalledWith('plugin-root-update')
    })

    it(
        'persists a root-only update without replacing a concurrently edited character',
        async () => {
            const harness = createHarness()
            harness.compatibilityDatabase.characters = [
                { chaId: 'character', name: 'Before', chats: [] },
            ] as Database['characters']
            harness.authoritativeSnapshots.push({
                database: structuredClone(harness.compatibilityDatabase) as Database,
                revision: 9,
            })
            const character = harness.compatibilityDatabase.characters[0]
            harness.applyCompatibilityDatabaseLite.mockImplementation((update) => {
                applyPluginDatabaseUpdate(
                    harness.compatibilityDatabase as Database,
                    update,
                    ['username'],
                    PLUGIN_ACCESS_OWNER,
                    () => PLUGIN_ACCESS_OWNER,
                )
            })
            harness.flushPendingData.mockImplementation(async () => {
                character.name = 'Concurrent edit'
            })
            await harness.access.setDatabase({ username: 'Plugin root edit' }, ['username'])
            expect(harness.compatibilityDatabase.username).toBe('Plugin root edit')
            expect(harness.compatibilityDatabase.characters[0]).toBe(character)
            expect(character.name).toBe('Concurrent edit')
            expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
        },
    )

    it('routes scalable lite plugin storage replacement through atomic per-key mutations', async () => {
        const harness = createHarness()

        await harness.access.setDatabaseLite({
            pluginCustomStorage: {
                '2': 0,
                memory: { replaced: true },
            },
        }, ['pluginCustomStorage'])

        expect(harness.mutatePluginStorage).toHaveBeenCalledWith([
            { type: 'clear' },
            { type: 'set', key: '2', value: 0 },
            { type: 'set', key: 'memory', value: { replaced: true } },
        ])
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('routes a scalable async plugin-only update without materializing the database', async () => {
        const harness = createHarness()

        await harness.access.setDatabase({
            pluginCustomStorage: { memory: 'authoritative' },
        }, ['pluginCustomStorage'])

        expect(harness.mutatePluginStorage).toHaveBeenCalledWith([
            { type: 'clear' },
            { type: 'set', key: 'memory', value: 'authoritative' },
        ])
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('leaves live and authoritative snapshots unchanged when scalable replacement fails', async () => {
        const harness = createHarness()
        const liveBefore = structuredClone(harness.compatibilityDatabase)
        const authoritative = {
            username: 'Before',
            botPresets: [{ name: 'Preset' }],
            characters: [{ chaId: 'inactive', name: 'Before', chats: [] }],
        } as unknown as Database
        const authoritativeBefore = structuredClone(authoritative)
        harness.authoritativeSnapshots.push({ database: authoritative, revision: 10 })
        harness.replacePersistentDatabase.mockRejectedValueOnce(new Error('replacement failed'))

        await expect(harness.access.setDatabase(
            { characters: [{ chaId: 'inactive', name: 'After', chats: [] }] },
            ['characters'],
        )).rejects.toThrow('replacement failed')

        expect(harness.compatibilityDatabase).toEqual(liveBefore)
        expect(authoritative).toEqual(authoritativeBefore)
    })

    it('allows only one of two setters materialized from the same revision to commit', async () => {
        const harness = createHarness()
        const base = {
            username: 'Before',
            characters: [],
            botPresets: [{ name: 'Preset' }],
        } as unknown as Database
        harness.authoritativeSnapshots.push(
            { database: structuredClone(base), revision: 20 },
            { database: structuredClone(base), revision: 20 },
        )
        let currentRevision = 20
        harness.replacePersistentDatabase.mockImplementation(async (
            _database,
            _reason,
            options,
        ) => {
            if (options.expectedRevision !== currentRevision) {
                throw new Error('revision-conflict')
            }
            currentRevision++
            return { kind: 'committed', revision: currentRevision, projection: 'applied' }
        })

        const outcomes = await Promise.allSettled([
            harness.access.setDatabase({ username: 'First', characters: [] }, [
                'username',
                'characters',
            ]),
            harness.access.setDatabase({ username: 'Second', characters: [] }, [
                'username',
                'characters',
            ]),
        ])

        expect(outcomes.filter((outcome) => outcome.status === 'fulfilled')).toHaveLength(1)
        expect(outcomes.filter((outcome) => outcome.status === 'rejected')).toHaveLength(1)
        expect(harness.replacePersistentDatabase).toHaveBeenCalledTimes(2)
    })

    it('does not overwrite a disjoint edit committed after materialization', async () => {
        const harness = createHarness()
        const storeDatabase = {
            username: 'Before',
            characters: [{ chaId: 'char', name: 'Before', chats: [] }],
            botPresets: [],
        } as unknown as Database
        harness.authoritativeSnapshots.push({
            database: structuredClone(storeDatabase),
            revision: 30,
        })
        storeDatabase.characters[0].name = 'Concurrent character edit'
        harness.replacePersistentDatabase.mockRejectedValueOnce(new Error('revision-conflict'))

        await expect(
            harness.access.setDatabase(
                {
                    username: 'Plugin root edit',
                    characters: [{ chaId: 'char', name: 'Before', chats: [] }],
                },
                ['username', 'characters'],
            ),
        ).rejects.toThrow('revision-conflict')

        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.anything(),
            'plugin-database-set',
            expect.objectContaining({ expectedRevision: 30 }),
        )
        expect(storeDatabase.characters[0].name).toBe('Concurrent character edit')
        expect(storeDatabase.username).toBe('Before')
    })

    it('uses the revision paired with the materialized database snapshot', async () => {
        const harness = createHarness()
        let currentRevision = 50
        harness.authoritativeSnapshots.push({
            database: {
                username: 'Before',
                characters: [],
                botPresets: [],
            } as unknown as Database,
            revision: currentRevision,
        })
        harness.materializeDatabaseSnapshot.mockImplementationOnce(async () => {
            const snapshot = harness.authoritativeSnapshots.shift()!
            currentRevision++
            return {
                ...snapshot,
                mutationGeneration: snapshot.mutationGeneration ?? 0,
            }
        })

        await harness.access.setDatabase({ username: 'After', characters: [] }, [
            'username',
            'characters',
        ])

        expect(currentRevision).toBe(51)
        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ username: 'After' }),
            'plugin-database-set',
            expect.objectContaining({ expectedRevision: 50 }),
        )
    })

    it('rejects replacement after an unflushed live mutation changes generation', async () => {
        const harness = createHarness()
        const liveBefore = structuredClone(harness.compatibilityDatabase)
        let currentMutationGeneration = 70
        harness.authoritativeSnapshots.push({
            database: {
                username: 'Before',
                characters: [],
                botPresets: [],
            } as unknown as Database,
            revision: 60,
            mutationGeneration: currentMutationGeneration,
        })
        harness.materializeDatabaseSnapshot.mockImplementationOnce(async () => {
            const snapshot = harness.authoritativeSnapshots.shift()!
            currentMutationGeneration++
            return {
                ...snapshot,
                mutationGeneration: snapshot.mutationGeneration!,
            }
        })
        harness.replacePersistentDatabase.mockImplementationOnce(async (
            _database,
            _reason,
            options,
        ) => {
            if (options.expectedMutationGeneration !== currentMutationGeneration) {
                throw new Error('mutation-generation-conflict')
            }
            return { kind: 'committed', revision: 61, projection: 'applied' }
        })

        await expect(
            harness.access.setDatabase({ username: 'Plugin edit', characters: [] }, [
                'username',
                'characters',
            ]),
        ).rejects.toThrow('mutation-generation-conflict')

        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.anything(),
            'plugin-database-set',
            expect.objectContaining({
                expectedRevision: 60,
                expectedMutationGeneration: 70,
            }),
        )
        expect(harness.compatibilityDatabase).toEqual(liveBefore)
    })

    it('rejects a scalable update when confirmation outlives profile or navigation state', async () => {
        const harness = createHarness()
        const confirmation = deferred<Record<string, unknown>>()
        harness.prepareAuthoritativeDatabaseUpdate.mockReturnValueOnce(confirmation.promise)
        const pending = harness.access.setDatabase(
            { plugins: [], username: 'After' },
            ['plugins', 'username'],
        )

        harness.setNavigationGeneration(1)
        confirmation.resolve({ plugins: [], username: 'After' })

        await expect(pending).rejects.toThrow(/became stale/i)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('merges explicit and extra custom storage independently of input key order', async () => {
        const first = createHarness()
        const second = createHarness()
        const base = {
            characters: [],
            botPresets: [],
            pluginCustomStorage: { existing: 'kept only without explicit replacement' },
        } as unknown as Database
        first.authoritativeSnapshots.push({ database: structuredClone(base), revision: 40 })
        second.authoritativeSnapshots.push({ database: structuredClone(base), revision: 40 })
        const explicit = { shared: 'explicit', explicitOnly: 'value' }
        const firstUpdate = {
            pluginCustomStorage: explicit,
            shared: 'extra',
            extraOnly: 2,
        }
        const secondUpdate = {
            extraOnly: 2,
            shared: 'extra',
            pluginCustomStorage: explicit,
        }

        await first.access.setDatabase(firstUpdate, ['pluginCustomStorage'])
        await second.access.setDatabase(secondUpdate, ['pluginCustomStorage'])

        expect(first.mutatePluginStorage).toHaveBeenCalledWith([
            { type: 'clear' },
            { type: 'set', key: 'shared', value: 'extra' },
            { type: 'set', key: 'explicitOnly', value: 'value' },
            { type: 'set', key: 'extraOnly', value: 2 },
        ])
        expect(second.mutatePluginStorage.mock.calls[0][0]).toEqual(
            first.mutatePluginStorage.mock.calls[0][0],
        )
        expect(first.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(second.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it.each([
        null,
        [],
        new Date(),
        Object.create({ inherited: true }),
    ])('rejects non-plain database updates', async (update) => {
        const harness = createHarness()

        await expect(harness.access.setDatabase(
            update as unknown as Record<string, unknown>,
            ['username'],
        )).rejects.toThrow(/plain record/i)
        expect(() => harness.access.setDatabaseLite(
            update as unknown as Record<string, unknown>,
            ['username'],
        )).toThrow(/plain record/i)
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
    })

    it.each(['__proto__', 'prototype', 'constructor'])(
        'rejects dangerous custom key %s',
        async (key) => {
            const harness = createHarness()
            const update = JSON.parse(`{"${key}":"blocked"}`) as Record<string, unknown>

            await expect(harness.access.setDatabase(update, ['username'])).rejects.toThrow(
                /unsafe plugin database key/i,
            )
            expect(() => harness.access.setDatabaseLite(update, ['username'])).toThrow(
                /unsafe plugin database key/i,
            )
        },
    )

    it('rejects invalid explicit plugin custom storage', async () => {
        const harness = createHarness()
        const update = { pluginCustomStorage: null }

        await expect(harness.access.setDatabase(update, ['pluginCustomStorage'])).rejects.toThrow(
            /pluginCustomStorage must be a plain record/i,
        )
        expect(() => harness.access.setDatabaseLite(update, ['pluginCustomStorage'])).toThrow(
            /pluginCustomStorage must be a plain record/i,
        )
    })
})
