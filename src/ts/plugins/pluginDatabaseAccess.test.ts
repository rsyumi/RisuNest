import { advancePluginStorageRemoteChanges, type PluginStorageWriteCheck } from './pluginStorageProtection'
const PLUGIN_ACCESS_OWNER = 'test-plugin'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'
import type {
    CharacterPage,
    ConversationPage,
    ConversationWindow,
    PluginStorageMutation,
    PersistentRevisionLease,
} from '../storage/persistentDataStore'
import { RevisionConflictError } from '../storage/persistentDataStore'
import { createLeaseStub, createStoreStub } from '../storage/persistentDataStore.testSupport'
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
import { createPinnedConversationPositionResolver } from './pinnedConversationPosition'

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

function createHarness(options: { unitIntent?: boolean; revisionAdmission?: boolean } = {}) {
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
    const messageWindowReads: unknown[] = []
    const fullConversationReads: string[] = []
    const archivedCharacterIds = new Set<string>()
    const releasedLeases: Array<ReturnType<typeof vi.fn>> = []
    const authoritativeSnapshots: Array<{
        database: Database
        revision: number
        mutationGeneration?: number
        pluginStorageValues?: Array<{ owner: string; key: string; value: unknown }>
    }> = []
    let lastPinnedDatabase: Database | undefined
    const readRoot = vi.fn(async () => {
        const database = pinnedDatabases[0] ?? lastPinnedDatabase ?? authoritativeSnapshots[0]?.database ?? compatibilityDatabase
        const { characters, botPresets, pluginCustomStorage, ...root } = database
        return { revision: 4, value: root }
    })
    const acquireRevision = vi.fn(async (revision: number) => {
        const database = pinnedDatabases.shift() ?? lastPinnedDatabase ?? authoritativeSnapshots[0]?.database ?? compatibilityDatabase
        lastPinnedDatabase = database
        const characters = database.characters ?? []
        const presets = database.botPresets ?? []
        const pluginStorage = database.pluginCustomStorage ?? {}
        const release = vi.fn(async () => undefined)
        releasedLeases.push(release)
        return createLeaseStub({
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
                fullConversationReads.push(conversationId)
                const chat = characters
                    .find((value) => value.chaId === characterId)
                    ?.chats.find((value) => value.id === conversationId)
                return chat ? { revision, value: chat } : null
            },
            readConversationMetadata: async (characterId: string, conversationId: string) => {
                const chat = characters.find((value) => value.chaId === characterId)
                    ?.chats.find((value) => value.id === conversationId)
                if (!chat) return null
                const { message, ...conversation } = chat
                return { revision, value: { characterId, conversationId, conversation, totalMessages: message.length } }
            },
            readConversationWindow: async ({ characterId, conversationId, startIndex, limit }) => {
                messageWindowReads.push({ startIndex, limit })
                const chat = characters.find((value) => value.chaId === characterId)
                    ?.chats.find((value) => value.id === conversationId)
                if (!chat) return null
                return { revision, value: {
                    characterId, conversationId, startIndex,
                    endIndex: Math.min(startIndex + limit, chat.message.length),
                    messages: chat.message.slice(startIndex, startIndex + limit),
                    totalMessages: chat.message.length,
                } as ConversationWindow }
            },
            readPluginStorageValues: async ({ owner, afterKey, limit = 256 }) => {
                const keys = Object.keys(pluginStorage)
                const start = afterKey ? afterKey.ordinal + 1 : 0
                const end = Math.min(start + limit, keys.length)
                return { revision,
                    items: owner !== undefined && owner !== PLUGIN_ACCESS_OWNER ? [] : keys.slice(start, end).map((key) => ({ owner: PLUGIN_ACCESS_OWNER, key, value: pluginStorage[key] })),
                    nextCursor: end < keys.length ? { owner: PLUGIN_ACCESS_OWNER, key: keys[end - 1], ordinal: end - 1 } : null,
                }
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
        })
    })
    const store = createStoreStub({
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
    })
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
    const commitPersistentUnitIntent = vi.fn(async (_reason: string, _units: readonly import('./pluginUnitIntents').PluginUnitMutation[], _conversations?: readonly unknown[], _wholeMessages?: readonly import('./pluginUnitIntents').PluginWholeMessageIntent[], _committed?: unknown, check?: PluginStorageWriteCheck) => { check?.validate(); check?.committed() })
    const access = createPluginDatabaseAccess({
        owner: PLUGIN_ACCESS_OWNER,
        store,
        commitPersistentUnitIntent,
        getPersistentRevision: () => 4,
        flushPendingData,
        getCompatibilityDatabase: () => compatibilityDatabase,
        getSelectedCharacterId,
        captureSelectedConversationTarget: () => selectedTarget as any,
        acquireCompleteConversation: acquireCompleteConversation as any,
        refreshSelectedConversationAfterReplacement:
            refreshSelectedConversationAfterReplacement as any,
        invalidateActiveConversationSession,
        reportIdentityReplacementRejected,
        getNavigationGeneration: () => navigationGeneration,
        getStorageAuthorityEpoch: () => authorityEpoch,
        assertPersistentMutationAllowed,
        readPluginStorageSnapshot,
        prepareAuthoritativeDatabaseUpdate,
        snapshot: <T>(value: T) => snapshot(value) as T,
    })
    return {
        access,
        commitPersistentUnitIntent,
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
        messageWindowReads,
        fullConversationReads,
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

describe('plugin unit writes with read provenance', () => {
    it('commits only the field edited on an exact character read', async () => {
        const harness = createHarness({ unitIntent: true })
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database)
        const read = await harness.access.getCharacterFromIndex(0, callContext())
        read!.name = 'Edited'
        database.characters[0].image = 'Remote description'
        await harness.access.setCharacterToIndex(0, read!, callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setCharacterToIndex', [{ key: '["character","active","name"]', type: 'set', value: 'Edited' }], [], [])
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
    })

    it('passes only explicit opaque edits from public character and chat setters to local unit commit', async () => {
        const harness = createHarness({ unitIntent: true })
        const database = makeFullObjectDatabase()
        Object.assign(database.characters[0], { opaqueRetained: 'Imported', opaqueEdited: 'Before' })
        Object.assign(database.characters[0].chats[0], { opaqueRetained: 'Imported', opaqueEdited: 'Before' })
        harness.pinnedDatabases.push(database, database, database)
        const read: any = await harness.access.getCharacterFromIndex(0, callContext())
        read.opaqueEdited = 'Edited character'
        await harness.access.setCharacterToIndex(0, read, callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenLastCalledWith('plugin-setCharacterToIndex', [{ key: '["character","active","opaqueEdited"]', type: 'set', value: 'Edited character' }], [], [])
        read.chats[0].opaqueEdited = 'Edited chat'
        await harness.access.setChatToIndex(0, 0, read.chats[0], callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenLastCalledWith('plugin-setChatToIndex', [{ key: '["conversation","active","active-chat-a","opaqueEdited"]', type: 'set', value: 'Edited chat' }], [], [])
        expect(harness.replacePersistentCompleteCharacter).not.toHaveBeenCalled()
        expect(harness.replacePersistentConversation).not.toHaveBeenCalled()
    })

    it('uses nested chat provenance from a character read without replacing messages', async () => {
        const harness = createHarness({ unitIntent: true })
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database)
        const read = await harness.access.getCurrentCharacter(callContext())
        const chat = read!.chats[0]
        chat.name = 'Edited chat'
        database.characters[0].chats[0].message.push({ role: 'user', data: 'Remote append' })
        await harness.access.setChatToIndex(0, 0, chat, callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setChatToIndex', [{ key: '["conversation","active","active-chat-a","name"]', type: 'set', value: 'Edited chat' }], [], [])
    })

    it('confirms only changed plugin records and commits approved additions and in-place replacements', async () => {
        const harness = createHarness()
        const original = { name: 'installed', script: 'original', version: '3.0', enabled: true }
        harness.compatibilityDatabase.plugins = [original] as any
        harness.prepareAuthoritativeDatabaseUpdate.mockImplementation(async update => ({ ...update, plugins: [] }))
        await harness.access.setDatabase({ plugins: [] }, ['plugins'])
        expect(harness.prepareAuthoritativeDatabaseUpdate.mock.calls.at(-1)?.[0]).not.toHaveProperty('plugins')
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([])
        const replacement = { ...original, script: 'replacement' }
        await harness.access.setDatabase({ plugins: [replacement] }, ['plugins'])
        expect(harness.prepareAuthoritativeDatabaseUpdate.mock.calls.at(-1)?.[0]).toEqual({ plugins: [replacement] })
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([])
        const proposed = { name: 'new', script: 'proposed', version: '3.0', enabled: true }
        await harness.access.setDatabase({ plugins: [original, proposed] }, ['plugins'])
        expect(harness.prepareAuthoritativeDatabaseUpdate.mock.calls.at(-1)?.[0]).toEqual({ plugins: [proposed] })
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([])
        harness.prepareAuthoritativeDatabaseUpdate.mockImplementation(async update => ({ ...update, plugins: [replacement] }))
        await harness.access.setDatabase({ plugins: [replacement] }, ['plugins'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["record","plugins","installed"]', type: 'set', value: replacement },
        ])
        harness.prepareAuthoritativeDatabaseUpdate.mockImplementation(async update => ({ ...update, plugins: [proposed] }))
        await harness.access.setDatabase({ plugins: [proposed, proposed] }, ['plugins'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["record","plugins","new"]', type: 'set', value: proposed },
            { key: '["order","plugins"]', type: 'set', value: ['installed', 'new'] },
        ])
        expect(harness.compatibilityDatabase.plugins).toEqual([original])
    })

    it('commits a copy against the newest database read of the same keys', async () => {
        const harness = createHarness({ unitIntent: true })
        const read = await harness.access.getDatabaseSnapshot(['username'], ['username'])
        const copy = JSON.parse(JSON.stringify(read))
        copy.username = 'Copy edit'
        await harness.access.setDatabase(copy, ['username'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([{ key: '["root","username"]', type: 'set', value: 'Copy edit' }])
        harness.compatibilityDatabase.username = 'Copy edit'
        await harness.access.getDatabaseSnapshot(['username'], ['username'])
        await harness.access.setDatabase(copy, ['username'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([])
    })

    it.each([
        ['an edited', (data: string) => `${data} edited`, [{ role: 'user', data: 'a edited' }]],
        ['an unchanged', (data: string) => data, null],
    ] as const)('commits %s message map spread over the newest chat read after its metadata changed', async (_name, edit, messages) => {
        const harness = createHarness({ unitIntent: true })
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        await harness.access.getChatFromIndex(0, 0, callContext())
        Object.assign(database.characters[0].chats[0], { lastMemory: 'Remote memory' })
        const fresh = (await harness.access.getChatFromIndex(0, 0, callContext()))!
        await harness.access.setChatToIndex(0, 0, { ...fresh, message: fresh.message.map(message => ({ ...message, data: edit(message.data) })) }, callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setChatToIndex', [], [], messages ? [{ characterId: 'active', conversationId: 'active-chat-a', messages }] : [])
    })

    it('rejects an expired JSON clone and a delayed exact object after authority replacement', async () => {
        const harness = createHarness({ unitIntent: true })
        const read = await harness.access.getDatabaseSnapshot(['username'], ['username'])
        const copy = JSON.parse(JSON.stringify(read))
        harness.advanceAuthorityEpoch()
        harness.access.expireReadBaselines()
        await expect(harness.access.setDatabase(copy, ['username'])).rejects.toThrow('stale')
        await expect(harness.access.setDatabase(read, ['username'])).rejects.toThrow('stale')
        expect(harness.commitPersistentUnitIntent).not.toHaveBeenCalled()
    })

    it('pins a dormant no-read chat baseline before awaits and preserves its messages', async () => {
        const harness = createHarness({ unitIntent: true, revisionAdmission: true })
        const database = makeFullObjectDatabase()
        harness.compatibilityDatabase.characters = [createCatalogCharacterStub({ id: 'active', name: 'Stub', type: 'character', configuredIndex: 0, recentAt: 0, trashed: false, conversationCount: 2 })]
        harness.pinnedDatabases.push(database, database)
        const submitted = structuredClone(database.characters[0].chats[0])
        submitted.name = 'Edited dormant chat'
        const writing = harness.access.setChatToIndex(0, 0, submitted, callContext())
        expect(harness.store.acquireRevision).toHaveBeenCalledWith(4)
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        submitted.message.push({ role: 'user', data: 'Mutation after admission' })
        await writing
        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setChatToIndex', [{ key: '["conversation","active","active-chat-a","name"]', type: 'set', value: 'Edited dormant chat' }], [], [])
        expect(harness.releasedLeases.every(release => release.mock.calls.length === 1)).toBe(true)
    })

    it.each(['archived predecessor', 'trashed configured order'])(
        'admits a no-read chat write by pinned order with %s and keeps later unrelated edits',
        async (order) => {
            const harness = createHarness({ unitIntent: true })
            const archived = makeCharacter('archived')
            const other = makeCharacter('other')
            const target = makeCharacter('target', order === 'trashed configured order')
            target.chats.reverse()
            const database = makeFullObjectDatabase(order === 'archived predecessor'
                ? [archived, other, target]
                : [other, target])
            harness.archivedCharacterIds.add(archived.chaId)
            harness.pinnedDatabases.push(database)
            const liveTarget = structuredClone(target)
            liveTarget.chats.reverse()
            const liveChat = liveTarget.chats.find(chat => chat.id === 'target-chat-a')!
            liveChat.note = 'Unsaved call-boundary note'
            harness.compatibilityDatabase.characters = order === 'archived predecessor'
                ? [archived, other, liveTarget]
                : [liveTarget, other]
            const submitted = structuredClone(liveChat)
            submitted.name = 'Plugin edit'
            const flush = deferred<void>()
            harness.flushPendingData.mockReturnValueOnce(flush.promise)

            const writing = harness.access.setChatToIndex(1, 1, submitted, callContext())
            liveChat.note = 'Edit after admission'
            await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
            const persistedChat = target.chats.find(chat => chat.id === submitted.id)!
            persistedChat.note = liveChat.note
            persistedChat.message.push({ role: 'char', data: 'Reply during flush' })
            flush.resolve()
            await writing

            expect(harness.commitPersistentUnitIntent).toHaveBeenCalledExactlyOnceWith(
                'plugin-setChatToIndex',
                [{ key: '["conversation","target","target-chat-a","name"]', type: 'set', value: 'Plugin edit' }],
                [], [],
            )
            expect(harness.releasedLeases.every(release => release.mock.calls.length === 1)).toBe(true)
        },
    )

    it('rejects a no-read chat write when its pinned owner changes during flush', async () => {
        const harness = createHarness({ unitIntent: true })
        const first = makeCharacter('first')
        const second = makeCharacter('second')
        first.chats[0].id = 'shared-chat-id'
        second.chats[0].id = 'shared-chat-id'
        harness.pinnedDatabases.push(
            makeFullObjectDatabase([first, second]),
            makeFullObjectDatabase([second, first]),
        )
        harness.compatibilityDatabase.characters = [first, second]
        const submitted = { ...structuredClone(first.chats[0]), name: 'Original owner edit' }
        const flush = deferred<void>()
        harness.flushPendingData.mockReturnValueOnce(flush.promise)

        const writing = harness.access.setChatToIndex(0, 0, submitted, callContext())
        await vi.waitFor(() => expect(harness.flushPendingData).toHaveBeenCalledOnce())
        flush.resolve()

        await expect(writing).rejects.toThrow('target')
        expect(harness.commitPersistentUnitIntent).not.toHaveBeenCalled()
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.releasedLeases.every(release => release.mock.calls.length === 1)).toBe(true)
    })

    it('rejects a no-read setter when its admitted revision cannot be pinned', async () => {
        const harness = createHarness({ unitIntent: true, revisionAdmission: true })
        harness.store.acquireRevision = vi.fn(async () => { throw new RevisionConflictError(4, 5) })
        const writing = harness.access.setCharacterToIndex(0, makeCharacter('active'), callContext())
        expect(harness.store.acquireRevision).toHaveBeenCalledWith(4)
        harness.advanceAuthorityEpoch()
        await expect(writing).rejects.toThrow('stale')
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.commitPersistentUnitIntent).not.toHaveBeenCalled()
    })

    it('rejects a write that outlives execution while asynchronous preparation is pending', async () => {
        const harness = createHarness({ unitIntent: true })
        const gate = deferred<Record<string, unknown>>()
        harness.prepareAuthoritativeDatabaseUpdate.mockReturnValueOnce(gate.promise)
        const writing = harness.access.setDatabase({ username: 'Old execution' }, ['username'])
        await vi.waitFor(() => expect(harness.prepareAuthoritativeDatabaseUpdate).toHaveBeenCalledOnce())
        harness.access.closeReadBaselines()
        gate.resolve({ username: 'Old execution' })
        await expect(writing).rejects.toThrow('closed')
        expect(harness.commitPersistentUnitIntent).not.toHaveBeenCalled()
    })
})

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
        const rejected = expect(writing).rejects.toThrow('stale read baseline')
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

        const input = {
            characterId: 'active',
            conversationId: 'active-chat-c',
            liveCharacter,
            liveConversation: liveSelected,
            windowStart: 0,
        }
        const projected = await projector({ ...input, history: 'full' })

        expect(harness.fullConversationReads).toEqual(['active-chat-a'])
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

        const current = await projector({ ...input, history: 'current' })
        const withoutMessages = ({ message: _message, ...metadata }: PluginCompleteCharacter['chats'][number]) => metadata
        expect(harness.fullConversationReads).toEqual(['active-chat-a'])
        expect(current.char.chats).toEqual([durable.chats[0], dirtyResident, liveSelected].map(withoutMessages))
        expect(current.chat).toEqual(liveSelected)
        expect(current.char.name).toBe('Live owner detail')
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
            windowStart: 0,
            history: 'full' as const,
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

    it('streams full snapshot message rows without assembling host characters', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push(makeFullObjectDatabase([
            makeCharacter('active-a'),
            makeCharacter('active-b'),
        ]))

        const stream = await harness.access.getDatabaseSnapshotStream(
            ['characters'],
            ['characters'],
        )
        const chunks = []
        for await (const chunk of stream as any) chunks.push(...(chunk.type === 'batch' ? chunk.chunks : [chunk]))

        expect(chunks.map((chunk) => chunk.type)).toEqual([
            'arrayStart',
            'characterStart',
            'conversationStart',
            'message',
            'conversationStart',
            'message',
            'characterStart',
            'conversationStart',
            'message',
            'conversationStart',
            'message',
            'provenance',
        ])
        expect(chunks.filter((chunk) => chunk.type === 'characterStart').map((chunk) => chunk.value.chaId)).toEqual([
            'active-a',
            'active-b',
        ])
        expect(harness.snapshot).not.toHaveBeenCalled()
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
    })

    it('preserves current plugin records when an approved update reaches a newer database snapshot', () => {
        const current = { name: 'installed', script: 'newer', version: '3.0', enabled: true, realArg: { marker: 'newer' } }
        const added = { name: 'approved', script: 'approved', version: '3.0', enabled: true, realArg: {} }
        const candidate = { plugins: [current], pluginCustomStorage: {} } as unknown as Database
        applyPluginDatabaseUpdate(candidate, { plugins: [added] }, ['plugins'], PLUGIN_ACCESS_OWNER)
        expect(candidate.plugins).toEqual([current, added])
        applyPluginDatabaseUpdate(candidate, { plugins: [] }, ['plugins'], PLUGIN_ACCESS_OWNER)
        expect(candidate.plugins).toEqual([current, added])
    })

    it('pages 1000 messages without changing streamed order or values', async () => {
        const harness = createHarness()
        const character = makeCharacter('active')
        character.chats = [character.chats[0]]
        character.chats[0].message = Array.from({ length: 1000 }, (_, index) => ({ role: 'user' as const, data: String(index) }))
        harness.pinnedDatabases.push(makeFullObjectDatabase([character]))
        const stream = await harness.access.getDatabaseSnapshotStream(['characters'], ['characters'])
        const messages = []
        for await (const frame of stream as any) {
            for (const chunk of frame.type === 'batch' ? frame.chunks : [frame]) {
                if (chunk.type === 'message') messages.push(chunk.value)
            }
        }
        expect(messages).toEqual(character.chats[0].message)
        expect(harness.messageWindowReads).toHaveLength(8)
        expect(harness.messageWindowReads).toEqual(Array.from({ length: 8 }, (_, index) => ({ startIndex: index * 128, limit: 128 })))
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
    })

    it('releases a database snapshot cancelled before consuming any chunk', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push(makeFullObjectDatabase([makeCharacter('active')]))
        const stream = await harness.access.getDatabaseSnapshotStream(['characters'], ['characters'])
        await stream.cancel()
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
    })

    it('releases a streamed snapshot lease when iframe assembly is cancelled', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push(makeFullObjectDatabase([makeCharacter('active')]))
        const stream = await harness.access.getDatabaseSnapshotStream(
            ['characters'],
            ['characters'],
        )
        const reader = stream.getReader()
        await reader.read()
        await reader.cancel()
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
    })

    it.each([
        [{}, 'character', 'active'],
        [{ characterIndex: 1 }, 'character', 'trashed'],
        [{ characterIndex: 0, chatIndex: 1 }, 'conversation', 'active'],
    ] as const)('streams a full-object target %j from one pinned revision', async (target, select, id) => {
        const harness = createHarness()
        harness.pinnedDatabases.push(makeFullObjectDatabase())
        const result = await harness.access.getFullObjectSnapshotStream(target, callContext())
        expect(result?.select).toBe(select)
        const chunks: any[] = []
        for await (const chunk of result!.value as any) chunks.push(...(chunk.type === 'batch' ? chunk.chunks : [chunk]))
        expect(chunks.find((chunk) => chunk.type === 'characterStart').value.chaId).toBe(id)
        expect(chunks.filter((chunk) => chunk.type === 'message').map((chunk) => chunk.value.data))
            .toEqual(select === 'conversation' ? ['b'] : ['a', 'b'])
        expect(harness.snapshot).not.toHaveBeenCalled()
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
    })

    it('releases full-object streaming resources when the plugin is unloaded', async () => {
        const harness = createHarness()
        harness.pinnedDatabases.push(makeFullObjectDatabase())
        const lifetime = new AbortController()
        const result = await harness.access.getFullObjectSnapshotStream({}, {
            pluginName: PLUGIN_ACCESS_OWNER, signal: lifetime.signal,
        })
        const reader = result!.value.getReader()
        await reader.read()
        lifetime.abort()
        await vi.waitFor(() => expect(harness.releasedLeases[0]).toHaveBeenCalledOnce())
        await expect(reader.read()).rejects.toMatchObject({ name: 'AbortError' })
        reader.releaseLock()
        expect(harness.releasedLeases[0]).toHaveBeenCalledOnce()
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

    it('resolves a conversation by ID to the position the index APIs read', async () => {
        const harness = createHarness()
        harness.archivedCharacterIds.add('archived-a')
        harness.pinnedDatabases.push(makeFullObjectDatabase([
            makeCharacter('archived-a'),
            makeCharacter('trashed-b', true),
            makeCharacter('active-c'),
        ]))
        const resolvePosition = createPinnedConversationPositionResolver(() => ({
            store: harness.store,
            flushPendingData: harness.flushPendingData,
        }))

        const position = await resolvePosition('active-c', 'active-c-chat-b')

        expect(position).toEqual({ characterIndex: 1, chatIndex: 1 })
        expect(harness.flushPendingData).toHaveBeenCalledOnce()
        const chat = await harness.access.getChatFromIndex(position.characterIndex, position.chatIndex, callContext())
        expect(chat?.id).toBe('active-c-chat-b')
        await expect(resolvePosition('active-c', null)).resolves.toEqual({ characterIndex: 1, chatIndex: -1 })
        await expect(resolvePosition('active-c', 'missing-chat')).resolves.toEqual({ characterIndex: 1, chatIndex: -1 })
        await expect(resolvePosition('archived-a', 'archived-a-chat-a')).resolves.toEqual({ characterIndex: -1, chatIndex: -1 })
        expect(harness.releasedLeases.every((release) => release.mock.calls.length === 1)).toBe(true)
    })

    it('resolves reordered and trashed output targets using persisted character and conversation order', async () => {
        const harness = createHarness()
        harness.archivedCharacterIds.add('archived-a')
        const target = makeCharacter('target', true)
        target.chats.reverse()
        harness.pinnedDatabases.push(makeFullObjectDatabase([
            makeCharacter('archived-a'), makeCharacter('other'), target,
        ]))
        const resolvePosition = createPinnedConversationPositionResolver(() => ({
            store: harness.store, flushPendingData: harness.flushPendingData,
        }))
        expect(await resolvePosition('target', 'target-chat-a')).toEqual({ characterIndex: 1, chatIndex: 1 })
        expect(await resolvePosition('missing', 'missing-chat')).toEqual({ characterIndex: -1, chatIndex: -1 })
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

    it('captures stable IDs and admits only changed fields before an indexed character write', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        harness.compatibilityDatabase.characters = structuredClone(database.characters)
        const candidate = structuredClone(database.characters[1])
        candidate.name = 'Updated inactive character'

        const mutation = harness.access.setCharacterToIndex(1, candidate, callContext())
        harness.compatibilityDatabase.characters.reverse()
        await mutation

        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith(
            expect.stringMatching(/^plugin-setCharacter/),
            expect.arrayContaining([{ key: JSON.stringify(['character', candidate.chaId, 'name']), type: 'set', value: candidate.name }]), [], [],
        )
    })

    it('replaces only the captured conversation and keeps invalid indexes as no-ops', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database, database)
        const replacement = structuredClone(database.characters[0].chats[1])
        replacement.localLore = [{ key: 'plugin', content: 'saved' } as any]

        await harness.access.setChatToIndex(0, 1, replacement, callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setChatToIndex',
            expect.arrayContaining([{ key: JSON.stringify(['conversation', 'active', replacement.id, 'localLore']), type: 'set', value: replacement.localLore }]), [], [],
        )

        await harness.access.setChatToIndex(99, 99, replacement, callContext())
        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledTimes(1)
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

        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setCharacter',
            expect.arrayContaining([{ key: '["character","active","name"]', type: 'set', value: candidate.name }]), [], [],
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

        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledWith('plugin-setChatToIndex',
            [{ key: '["conversation","active","active-chat-a","note"]', type: 'set', value: replacement.note }], [], [],
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

        expect(harness.commitPersistentUnitIntent).toHaveBeenCalledOnce()
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

    it('holds a selected windowed lease until the unit commit settles', async () => {
        const harness = createHarness()
        const database = makeFullObjectDatabase()
        harness.pinnedDatabases.push(database)
        const replacement = structuredClone(database.characters[0])
        replacement.name = 'Selected replacement'
        const persistence = deferred<boolean>()
        harness.commitPersistentUnitIntent.mockImplementationOnce(async () => { await persistence.promise })

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
            harness.commitPersistentUnitIntent.mockRejectedValueOnce(outcome)
        } else {
            harness.commitPersistentUnitIntent.mockRejectedValueOnce(new Error('stale target'))
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
        harness.commitPersistentUnitIntent.mockImplementationOnce(async () => { await persistence.promise })

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
            commitPersistentUnitIntent: harness.commitPersistentUnitIntent,
            getPersistentRevision: () => 4,
            flushPendingData: harness.flushPendingData,
            getCompatibilityDatabase: () => harness.compatibilityDatabase,
            getSelectedCharacterId: harness.getSelectedCharacterId,
            captureSelectedConversationTarget: () => null,
            acquireCompleteConversation: vi.fn(),
            refreshSelectedConversationAfterReplacement: vi.fn(),
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: () => 0,
            getStorageAuthorityEpoch: () => 0,
            assertPersistentMutationAllowed: vi.fn(),
            readPluginStorageSnapshot: harness.readPluginStorageSnapshot,
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
            id: '0',
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

    it('emits only edits from a database baseline including inactive characters and owned storage', async () => {
        const harness = createHarness()
        const authoritative = makeFullObjectDatabase()
        harness.pinnedDatabases.push(authoritative)
        const read = await harness.access.getDatabaseSnapshot(['characters', 'username'], ['characters', 'username'])
        ;(read.characters as Database['characters'])[1].name = 'Edited while inactive'
        read.username = 'After'
        read.privateValue = 42
        await harness.access.setDatabase(read, ['characters', 'username'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["character","trashed","name"]', type: 'set', value: 'Edited while inactive' },
            { key: '["plugin","test-plugin","privateValue"]', type: 'set', value: 42 },
            { key: '["root","username"]', type: 'set', value: 'After' },
        ])
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(authoritative.characters[1].name).toBe('trashed')
    })

    it('scopes same-key plugin storage changes to the execution owner', async () => {
        const harness = createHarness()
        const authoritative = { characters: [], botPresets: [], pluginCustomStorage: { shared: 'before' } } as unknown as Database
        harness.pinnedDatabases.push(authoritative)
        const read = await harness.access.getDatabaseSnapshot(['pluginCustomStorage'], ['pluginCustomStorage'])
        ;(read.pluginCustomStorage as Record<string, unknown>).shared = 'updated'
        await harness.access.setDatabase(read, ['pluginCustomStorage'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["plugin","test-plugin","shared"]', type: 'set', value: 'updated' },
        ])
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('waits for the unit commit and its renderer projection before resolving', async () => {
        const harness = createHarness()
        const pending = deferred<void>()
        const projection = vi.fn()
        harness.commitPersistentUnitIntent.mockImplementationOnce(async () => {
            await pending.promise
            projection()
        })
        const finished = vi.fn()
        const writing = harness.access.setDatabase({ username: 'After' }, ['username']).then(finished)
        await vi.waitFor(() => expect(harness.commitPersistentUnitIntent).toHaveBeenCalledOnce())
        expect(finished).not.toHaveBeenCalled()
        expect(projection).not.toHaveBeenCalled()
        pending.resolve(undefined)
        await writing
        expect(projection).toHaveBeenCalledOnce()
        expect(finished).toHaveBeenCalledOnce()
        expect(projection.mock.invocationCallOrder[0]).toBeLessThan(finished.mock.invocationCallOrder[0])
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

    it('routes lite and async root-only setters through the same unit commit path', async () => {
        const harness = createHarness()
        await harness.access.setDatabaseLite({ username: 'Lite' }, ['username'])
        await harness.access.setDatabase({ username: 'Async' }, ['username'])
        expect(harness.commitPersistentUnitIntent.mock.calls.map(call => [call[0], call[1]])).toEqual([
            ['plugin-setDatabaseLite', [{ key: '["root","username"]', type: 'set', value: 'Lite' }]],
            ['plugin-setDatabase', [{ key: '["root","username"]', type: 'set', value: 'Async' }]],
        ])
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('ignores a plugin list sent through setDatabaseLite and still rejects a malformed one', async () => {
        const harness = createHarness()
        const installed = { name: 'installed', script: 'installed', version: '3.0', enabled: true }
        harness.compatibilityDatabase.plugins = [installed] as any
        const proposed = { name: 'new', script: 'proposed', version: '3.0', enabled: true }
        harness.prepareAuthoritativeDatabaseUpdate.mockImplementation(async update => ({ ...update, plugins: [proposed] }))

        await harness.access.setDatabaseLite({ username: 'Lite', plugins: [installed, proposed] }, ['plugins', 'username'])

        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["root","username"]', type: 'set', value: 'Lite' },
        ])
        expect(harness.compatibilityDatabase.plugins).toEqual([installed])
        expect(() => harness.access.setDatabaseLite({ plugins: [{ name: 'broken' }] }, ['plugins']))
            .toThrow(/plugins must be an array of plugin records/i)
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
            harness.commitPersistentUnitIntent.mockImplementation(async (_reason, units) => {
                character.name = 'Concurrent edit'
                const update = Object.fromEntries(units.filter(unit => JSON.parse(unit.key)[0] === 'root').map(unit => [JSON.parse(unit.key)[1], unit.type === 'set' ? unit.value : undefined]))
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

        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["plugin","test-plugin","2"]', type: 'set', value: 0 },
            { key: '["plugin","test-plugin","memory"]', type: 'set', value: { replaced: true } },
        ])
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('routes a scalable async plugin-only update without materializing the database', async () => {
        const harness = createHarness()

        await harness.access.setDatabase({
            pluginCustomStorage: { memory: 'authoritative' },
        }, ['pluginCustomStorage'])

        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["plugin","test-plugin","memory"]', type: 'set', value: 'authoritative' },
        ])
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('leaves read and live values unchanged when the unit commit fails', async () => {
        const harness = createHarness()
        const liveBefore = structuredClone(harness.compatibilityDatabase)
        const submitted = { username: 'After' }
        harness.commitPersistentUnitIntent.mockRejectedValueOnce(new Error('unit commit failed'))
        await expect(harness.access.setDatabase(submitted, ['username'])).rejects.toThrow('unit commit failed')
        expect(harness.compatibilityDatabase).toEqual(liveBefore)
        expect(submitted).toEqual({ username: 'After' })
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
    })

    it('commits concurrent routine setters as independent admitted unit intents', async () => {
        const harness = createHarness({ unitIntent: true })
        const outcomes = await Promise.allSettled([
            harness.access.setDatabase({ username: 'First' }, ['username']),
            harness.access.setDatabase({ maxContext: 4096 }, ['maxContext']),
        ])
        expect(outcomes.every(outcome => outcome.status === 'fulfilled')).toBe(true)
        expect(harness.commitPersistentUnitIntent.mock.calls.map(call => call[1])).toEqual([
            [{ key: '["root","username"]', type: 'set', value: 'First' }],
            [{ key: '["root","maxContext"]', type: 'set', value: 4096 }],
        ])
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('omits untouched stale character fields when a database read is submitted later', async () => {
        const harness = createHarness({ unitIntent: true })
        const database = { characters: [{ chaId: 'char', name: 'Before', chats: [] }] } as unknown as Database
        harness.pinnedDatabases.push(database)
        const read = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])
        ;(read.characters as Database['characters'])[0].name = 'Plugin edit'
        database.characters[0].image = 'Concurrent remote description'
        await harness.access.setDatabase(read, ['characters'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["character","char","name"]', type: 'set', value: 'Plugin edit' },
        ])
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(database.characters[0].image).toBe('Concurrent remote description')
    })

    it('uses the exact read baseline instead of taking a newer database baseline', async () => {
        const harness = createHarness({ unitIntent: true })
        const read = await harness.access.getDatabaseSnapshot(['username', 'maxContext'], ['username', 'maxContext'])
        harness.compatibilityDatabase.maxContext = 16384
        read.username = 'After'
        await harness.access.setDatabase(read, ['username', 'maxContext'])
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["root","username"]', type: 'set', value: 'After' },
        ])
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.compatibilityDatabase.maxContext).toBe(16384)
    })

    it('keeps the no-read call admission baseline across asynchronous preparation', async () => {
        const harness = createHarness({ unitIntent: true })
        const gate = deferred<Record<string, unknown>>()
        harness.prepareAuthoritativeDatabaseUpdate.mockImplementationOnce(() => gate.promise)
        const writing = harness.access.setDatabase({ username: 'Plugin edit', maxContext: 8192 }, ['username', 'maxContext'])
        await vi.waitFor(() => expect(harness.prepareAuthoritativeDatabaseUpdate).toHaveBeenCalledOnce())
        harness.compatibilityDatabase.maxContext = 16384
        gate.resolve({ username: 'Plugin edit' })
        await writing
        expect(harness.commitPersistentUnitIntent.mock.calls.at(-1)?.[1]).toEqual([
            { key: '["root","username"]', type: 'set', value: 'Plugin edit' },
        ])
        expect(harness.compatibilityDatabase.maxContext).toBe(16384)
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
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
        const base = { characters: [], botPresets: [], pluginCustomStorage: { existing: 'before' } } as unknown as Database
        first.pinnedDatabases.push(structuredClone(base))
        second.pinnedDatabases.push(structuredClone(base))
        const explicit = { shared: 'explicit', explicitOnly: 'value' }
        await first.access.setDatabase({ pluginCustomStorage: explicit, shared: 'extra', extraOnly: 2 }, ['pluginCustomStorage'])
        await second.access.setDatabase({ extraOnly: 2, shared: 'extra', pluginCustomStorage: explicit }, ['pluginCustomStorage'])
        const expected = [
            { key: '["plugin","test-plugin","extraOnly"]', type: 'set', value: 2 },
            { key: '["plugin","test-plugin","existing"]', type: 'delete' },
            { key: '["plugin","test-plugin","explicitOnly"]', type: 'set', value: 'value' },
            { key: '["plugin","test-plugin","shared"]', type: 'set', value: 'extra' },
        ]
        expect(first.commitPersistentUnitIntent.mock.calls[0][1]).toEqual(expected)
        expect(second.commitPersistentUnitIntent.mock.calls[0][1]).toEqual(expected)
    })

    it.each(['setDatabase', 'setDatabaseLite'] as const)('commits plugin storage values as JSON.stringify output through %s', async (method) => {
        const harness = createHarness()
        harness.pinnedDatabases.push({ characters: [], botPresets: [], pluginCustomStorage: {} } as unknown as Database)
        const when = new Date(Date.UTC(2026, 9, 17, 12, 30))
        const explicit = { stamp: when, nested: { when, list: [when] } }
        await harness.access[method]({ pluginCustomStorage: explicit, extraWhen: when }, ['pluginCustomStorage'])
        // Upstream RisuAI persists plugin storage values as JSON.stringify output.
        const json = (value: unknown) => JSON.parse(JSON.stringify(value))
        expect(harness.commitPersistentUnitIntent.mock.calls[0][1]).toEqual([
            { key: '["plugin","test-plugin","extraWhen"]', type: 'set', value: json(when) },
            { key: '["plugin","test-plugin","nested"]', type: 'set', value: json(explicit.nested) },
            { key: '["plugin","test-plugin","stamp"]', type: 'set', value: json(when) },
        ])
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


describe('database permission storage observations', () => {
    it.each(['setDatabase', 'setDatabaseLite'] as const)('rejects stale owned storage through %s and accepts a fresh read', async method => {
        const h = createHarness()
        const stale = await h.access.getDatabaseSnapshot(['pluginCustomStorage'], ['pluginCustomStorage'])
        ;(stale.pluginCustomStorage as Record<string, unknown>).memory = { retained: false }
        advancePluginStorageRemoteChanges([JSON.stringify(['plugin', PLUGIN_ACCESS_OWNER, 'memory'])])
        await expect(h.access[method](stale, ['pluginCustomStorage'])).rejects.toThrow('PLUGIN_STORAGE_STALE')
        const fresh = await h.access.getDatabaseSnapshot(['pluginCustomStorage'], ['pluginCustomStorage'])
        ;(fresh.pluginCustomStorage as Record<string, unknown>).memory = { retained: false }
        await expect(h.access[method](fresh, ['pluginCustomStorage'])).resolves.toBeUndefined()
        await h.access.closeReadBaselines()
    })

    it('does not rebase an in-flight database submission when another read completes', async () => {
        const h = createHarness()
        const stale = await h.access.getDatabaseSnapshot(['pluginCustomStorage'], ['pluginCustomStorage'])
        ;(stale.pluginCustomStorage as Record<string, unknown>).memory = { retained: false }
        let finish!: () => void
        const paused = new Promise<void>(resolve => { finish = resolve })
        h.prepareAuthoritativeDatabaseUpdate.mockImplementationOnce(async update => { await paused; return update })
        const writing = h.access.setDatabase(stale, ['pluginCustomStorage'])
        const rejected = expect(writing).rejects.toThrow('PLUGIN_STORAGE_STALE')
        await vi.waitFor(() => expect(h.prepareAuthoritativeDatabaseUpdate).toHaveBeenCalled())
        advancePluginStorageRemoteChanges([JSON.stringify(['plugin', PLUGIN_ACCESS_OWNER, 'memory'])])
        await h.access.getDatabaseSnapshot(['pluginCustomStorage'], ['pluginCustomStorage'])
        finish()
        await rejected
        await h.access.closeReadBaselines()
    })
})
