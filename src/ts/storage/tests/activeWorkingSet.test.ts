import { IDBFactory, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { ActiveWorkingSet } from '../activeWorkingSet.svelte'
import type { Chat, Database, character, groupChat } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import type {
    ConversationPage,
    PersistentDataStore,
    PersistentRevisionLease,
} from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'

function makeCharacter(id: string, chats: Chat[] = []): character {
    return {
        type: 'character',
        chaId: id,
        name: id.toUpperCase(),
        chats,
    } as unknown as character
}

function makeChat(id: string): Chat {
    return { id, name: id, note: '', localLore: [], message: [] }
}

function makeCharacterDetail(id: string): Omit<character, 'chats'> {
    const { chats: _chats, ...detail } = makeCharacter(id)
    return detail
}

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, resolve, reject }
}

function makeLease(input: {
    revision?: number
    characterId: string
    chats?: Chat[]
    readCharacter?: PersistentRevisionLease['readCharacter']
    readConversation?: PersistentRevisionLease['readConversation']
}): PersistentRevisionLease {
    const revision = input.revision ?? 1
    const chats = input.chats ?? []
    return {
        revision,
        readRoot: vi.fn(),
        queryCharacters: vi.fn(),
        readCharacter:
            input.readCharacter ??
            vi.fn(async () => ({
                revision,
                value: makeCharacterDetail(input.characterId),
            })),
        queryConversations: vi.fn(async ({ cursor }) => {
            const index = cursor ? Number(cursor) : 0
            const pageChats = chats.slice(index, index + 1)
            return {
                revision,
                items: pageChats.map((chat, offset) => ({
                    id: chat.id!,
                    characterId: input.characterId,
                    name: chat.name,
                    configuredIndex: index + offset,
                    recentAt: 0,
                    messageCount: chat.message.length,
                })),
                nextCursor: index + 1 < chats.length ? String(index + 1) : undefined,
            } satisfies ConversationPage
        }),
        readConversation:
            input.readConversation ??
            vi.fn(async (_characterId, conversationId) => {
                const chat = chats.find((candidate) => candidate.id === conversationId)
                return chat ? { revision, value: structuredClone(chat) } : null
            }),
        readConversationWindow: vi.fn(),
        release: vi.fn(async () => undefined),
    }
}

function makeHarness(lease: PersistentRevisionLease) {
    const database = {
        username: 'Fixture',
        characters: [makeCharacter('previous', [makeChat('previous-chat')])],
    } as unknown as Database
    let selectedCharacterId = 'previous'
    const coordinator = {
        revision: 1,
        initialize: vi.fn(),
        flushPendingData: vi.fn(() => Promise.resolve()),
        replacePersistentDatabase: vi.fn(async () => undefined),
        adoptHydratedCharacter: vi.fn(() => true),
    }
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision: 1, value: { username: 'Fixture' } })),
        readCharacter: vi.fn((id: string) => lease.readCharacter(id)),
        queryConversations: vi.fn((input) => lease.queryConversations(input)),
        readConversation: vi.fn((characterId: string, conversationId: string) =>
            lease.readConversation(characterId, conversationId),
        ),
        acquireRevision: vi.fn(() => Promise.reject(new Error('navigation acquired a snapshot'))),
    } as unknown as PersistentDataStore
    const publishedCharacters: character[] = []
    const publishedConversations: Array<{ characterId: string; conversation: Chat }> = []
    const publishCharacter = vi.fn((value: character | groupChat) => {
        selectedCharacterId = value.chaId
        publishedCharacters.push(value as character)
    })
    const workingSet = new ActiveWorkingSet({
        store,
        coordinator: coordinator as never,
        getSelectedCharacterId: () => selectedCharacterId,
        publishCharacter,
        publishConversation: (characterId, conversation) => {
            publishedConversations.push({ characterId, conversation })
        },
    })
    return {
        workingSet,
        store,
        coordinator,
        database,
        publishedCharacters,
        publishedConversations,
        publishCharacter,
        setSelectedCharacterId(id: string) {
            selectedCharacterId = id
        },
    }
}

describe('ActiveWorkingSet', () => {
    it('reconstructs a complete character from detail, configured summaries, and full chats', async () => {
        const chats = [makeChat('chat-a'), makeChat('chat-b')]
        const lease = makeLease({ characterId: 'char-a', chats })
        const harness = makeHarness(lease)

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(harness.publishedCharacters[0]).toMatchObject({
            chaId: 'char-a',
            chats: [{ id: 'chat-a' }, { id: 'chat-b' }],
        })
        expect(lease.queryConversations).toHaveBeenCalledTimes(2)
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
        expect(harness.coordinator.adoptHydratedCharacter).toHaveBeenCalledWith(
            1,
            harness.publishedCharacters[0],
        )
        expect(
            harness.coordinator.adoptHydratedCharacter.mock.invocationCallOrder[0],
        ).toBeLessThan(harness.publishCharacter.mock.invocationCallOrder[0])
    })

    it('hydrates conversations concurrently while preserving configured order', async () => {
        const chats = ['chat-a', 'chat-b', 'chat-c', 'chat-d', 'chat-e'].map(makeChat)
        const pending = new Map<
            string,
            ReturnType<typeof deferred<{ revision: number; value: Chat } | null>>
        >()
        const lease = makeLease({
            characterId: 'char-a',
            chats,
            readConversation: vi.fn((_characterId: string, conversationId: string) => {
                const entry = deferred<{ revision: number; value: Chat } | null>()
                pending.set(conversationId, entry)
                return entry.promise
            }),
        })
        const harness = makeHarness(lease)
        vi.mocked(lease.queryConversations).mockResolvedValue({
            revision: 1,
            items: chats.map((chat, index) => ({
                id: chat.id!,
                characterId: 'char-a',
                name: chat.name,
                configuredIndex: index,
                recentAt: 0,
                messageCount: 0,
            })),
        })

        const activation = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(pending.size).toBe(chats.length))
        for (const chat of [...chats].reverse()) {
            pending.get(chat.id!)!.resolve({ revision: 1, value: structuredClone(chat) })
        }

        expect(await activation).toBe(true)
        expect(harness.publishedCharacters[0].chats.map((chat) => chat.id)).toEqual([
            'chat-a',
            'chat-b',
            'chat-c',
            'chat-d',
            'chat-e',
        ])
    })

    it('publishes only the newest rapid character navigation', async () => {
        const a = deferred<ReturnType<PersistentRevisionLease['readCharacter']> extends Promise<infer T> ? T : never>()
        const leaseA = makeLease({
            characterId: 'char-a',
            readCharacter: vi.fn(() => a.promise),
        })
        const leaseB = makeLease({ characterId: 'char-b' })
        const harness = makeHarness(leaseA)
        vi.mocked(harness.store.readCharacter).mockImplementation((id) =>
            id === 'char-a' ? leaseA.readCharacter(id) : leaseB.readCharacter(id),
        )
        vi.mocked(harness.store.queryConversations).mockImplementation((input) =>
            input.characterId === 'char-a'
                ? leaseA.queryConversations(input)
                : leaseB.queryConversations(input),
        )
        vi.mocked(harness.store.readConversation).mockImplementation((characterId, conversationId) =>
            characterId === 'char-a'
                ? leaseA.readConversation(characterId, conversationId)
                : leaseB.readConversation(characterId, conversationId),
        )

        const first = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(leaseA.readCharacter).toHaveBeenCalledTimes(1))
        const second = harness.workingSet.activateCharacter('char-b')
        await second
        a.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await first).toBe(false)
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('does not publish navigation invalidated by activated database adoption', async () => {
        const detail = deferred<{ revision: number; value: Omit<character, 'chats'> } | null>()
        const lease = makeLease({
            characterId: 'char-a',
            readCharacter: vi.fn(() => detail.promise),
        })
        const harness = makeHarness(lease)

        const activation = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledOnce())
        harness.workingSet.invalidateNavigation()
        detail.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await activation).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
    })

    it('does not replace from stale character preparation after newer navigation starts', async () => {
        const leaseA = makeLease({ characterId: 'char-a' })
        const leaseB = makeLease({ characterId: 'char-b' })
        const harness = makeHarness(leaseA)
        vi.mocked(harness.store.readCharacter).mockImplementation((id) =>
            id === 'char-a' ? leaseA.readCharacter(id) : leaseB.readCharacter(id),
        )
        vi.mocked(harness.store.queryConversations).mockImplementation((input) =>
            input.characterId === 'char-a'
                ? leaseA.queryConversations(input)
                : leaseB.queryConversations(input),
        )
        vi.mocked(harness.store.readConversation).mockImplementation((characterId, conversationId) =>
            characterId === 'char-a'
                ? leaseA.readConversation(characterId, conversationId)
                : leaseB.readConversation(characterId, conversationId),
        )
        const preparation = deferred<{ database: Database; reason: string } | null>()
        const prepare = vi.fn(() => preparation.promise)

        const first = harness.workingSet.activateCharacter('char-a', { prepare })
        await vi.waitFor(() => expect(prepare).toHaveBeenCalledOnce())
        const second = harness.workingSet.activateCharacter('char-b')
        expect(await second).toBe(true)
        preparation.resolve({ database: harness.database, reason: 'cold-character-restore' })

        expect(await first).toBe(false)
        expect(harness.coordinator.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
    })

    it('preserves selection when character preparation produces no candidate', async () => {
        const lease = makeLease({ characterId: 'char-a' })
        const harness = makeHarness(lease)

        expect(await harness.workingSet.activateCharacter('char-a', {
            prepare: async () => null,
        })).toBe(false)

        expect(harness.store.readCharacter).not.toHaveBeenCalled()
        expect(harness.coordinator.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.publishedCharacters).toEqual([])
    })

    it('does not publish an older character when newer navigation starts during replacement', async () => {
        const leaseA = makeLease({ characterId: 'char-a' })
        const leaseB = makeLease({ characterId: 'char-b' })
        const harness = makeHarness(leaseA)
        vi.mocked(harness.store.readCharacter).mockImplementation((id) =>
            id === 'char-a' ? leaseA.readCharacter(id) : leaseB.readCharacter(id),
        )
        vi.mocked(harness.store.queryConversations).mockImplementation((input) =>
            input.characterId === 'char-a'
                ? leaseA.queryConversations(input)
                : leaseB.queryConversations(input),
        )
        vi.mocked(harness.store.readConversation).mockImplementation((characterId, conversationId) =>
            characterId === 'char-a'
                ? leaseA.readConversation(characterId, conversationId)
                : leaseB.readConversation(characterId, conversationId),
        )
        const replacement = deferred<void>()
        let replacementStarted = false
        harness.coordinator.replacePersistentDatabase.mockImplementation(async () => {
            replacementStarted = true
            await replacement.promise
        })
        harness.coordinator.flushPendingData.mockImplementation(() =>
            replacementStarted ? replacement.promise : Promise.resolve(),
        )

        const first = harness.workingSet.activateCharacter('char-a', {
            prepare: async () => ({
                database: harness.database,
                reason: 'cold-character-restore',
            }),
        })
        await vi.waitFor(() =>
            expect(harness.coordinator.replacePersistentDatabase).toHaveBeenCalledOnce(),
        )
        const second = harness.workingSet.activateCharacter('char-b')
        replacement.resolve()

        expect(await first).toBe(false)
        expect(await second).toBe(true)
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
    })

    it('preserves the previous selection when hydration fails', async () => {
        const lease = makeLease({
            characterId: 'char-a',
            readCharacter: vi.fn(async () => null),
        })
        const harness = makeHarness(lease)

        await expect(harness.workingSet.activateCharacter('char-a')).rejects.toThrow(
            'Character char-a was not found',
        )

        expect(harness.publishedCharacters).toEqual([])
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('uses the selected character captured before conversation navigation awaits', async () => {
        const chat = makeChat('chat-a')
        const lease = makeLease({ characterId: 'char-a', chats: [chat] })
        const harness = makeHarness(lease)
        harness.setSelectedCharacterId('char-a')
        const pendingFlush = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(pendingFlush.promise)

        const activation = harness.workingSet.activateConversation('chat-a')
        harness.setSelectedCharacterId('char-b')
        harness.database.characters.reverse()
        pendingFlush.resolve()
        expect(await activation).toBe(true)

        expect(lease.readConversation).toHaveBeenCalledWith('char-a', 'chat-a')
        expect(harness.publishedConversations).toEqual([{ characterId: 'char-a', conversation: chat }])
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('discards hydration when the coordinator revision changes', async () => {
        const lease = makeLease({ revision: 1, characterId: 'char-a' })
        const harness = makeHarness(lease)
        Object.defineProperty(harness.coordinator, 'revision', { value: 2, writable: true })

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('discards a result when the revision changes during hydration', async () => {
        const detail = deferred<{ revision: number; value: Omit<character, 'chats'> } | null>()
        const lease = makeLease({
            revision: 1,
            characterId: 'char-a',
            readCharacter: vi.fn(() => detail.promise),
        })
        const harness = makeHarness(lease)

        const activation = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledTimes(1))
        harness.coordinator.revision = 2
        detail.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await activation).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('does not publish when the hydrated baseline can no longer be adopted', async () => {
        const lease = makeLease({ characterId: 'char-a' })
        const harness = makeHarness(lease)
        harness.coordinator.adoptHydratedCharacter.mockReturnValueOnce(false)

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(false)

        expect(harness.publishedCharacters).toEqual([])
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('opens the store revision and initializes coordinator baselines', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))

        await harness.workingSet.initializeActiveWorkingSet(harness.database)

        expect(harness.store.open).toHaveBeenCalledTimes(1)
        expect(harness.coordinator.initialize).toHaveBeenCalledWith(1, harness.database)
    })

    it('rehydrates after reopen through bounded reads without writing IndexedDB', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'active-working-set-reopen'
        const initial = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await initial.open()
        const imported = await initial.replaceFromDatabase(fixtureDatabase)
        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        const acquireRevision = vi.spyOn(reopened, 'acquireRevision')
        const writes = { put: 0, add: 0, delete: 0, clear: 0 }
        const originalPut = IDBObjectStore.prototype.put
        const originalAdd = IDBObjectStore.prototype.add
        const originalDelete = IDBObjectStore.prototype.delete
        const originalClear = IDBObjectStore.prototype.clear
        const putSpy = vi.spyOn(IDBObjectStore.prototype, 'put').mockImplementation(function (...args) {
            writes.put++
            return originalPut.apply(this, args as Parameters<IDBObjectStore['put']>)
        })
        const addSpy = vi.spyOn(IDBObjectStore.prototype, 'add').mockImplementation(function (...args) {
            writes.add++
            return originalAdd.apply(this, args as Parameters<IDBObjectStore['add']>)
        })
        const deleteSpy = vi.spyOn(IDBObjectStore.prototype, 'delete').mockImplementation(function (...args) {
            writes.delete++
            return originalDelete.apply(this, args as Parameters<IDBObjectStore['delete']>)
        })
        const clearSpy = vi.spyOn(IDBObjectStore.prototype, 'clear').mockImplementation(function () {
            writes.clear++
            return originalClear.apply(this)
        })
        const published: Array<character | groupChat> = []
        const coordinator = {
            revision: imported.revision,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => undefined),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
        }
        const workingSet = new ActiveWorkingSet({
            store: reopened,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            publishCharacter: (value) => published.push(value),
            publishConversation: vi.fn(),
        })

        try {
            expect(await workingSet.activateCharacter('char-a')).toBe(true)
        } finally {
            putSpy.mockRestore()
            addSpy.mockRestore()
            deleteSpy.mockRestore()
            clearSpy.mockRestore()
        }

        expect(published[0]).toEqual(fixtureDatabase.characters[1])
        expect(acquireRevision).not.toHaveBeenCalled()
        expect(writes).toEqual({ put: 0, add: 0, delete: 0, clear: 0 })
    })

    it('discards mixed revisions when an intervening commit makes the next page empty', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'active-working-set-revision-race'
        const reader = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const writer = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reader.open()
        const imported = await reader.replaceFromDatabase(fixtureDatabase)
        await writer.open()
        const originalQuery = reader.queryConversations.bind(reader)
        const originalReadConversation = reader.readConversation.bind(reader)
        let firstPage = true
        vi.spyOn(reader, 'queryConversations').mockImplementation(async (input) => {
            const page = await originalQuery(input)
            if (firstPage) {
                firstPage = false
                return { ...page, nextCursor: '1' }
            }
            return page
        })
        let firstConversation = true
        vi.spyOn(reader, 'readConversation').mockImplementation(async (characterId, conversationId) => {
            const conversation = await originalReadConversation(characterId, conversationId)
            if (firstConversation) {
                firstConversation = false
                const root = await writer.readRoot()
                await writer.commit({
                    expectedRevision: root.revision,
                    root: { ...root.value, username: 'Intervening commit' },
                })
            }
            return conversation
        })
        const publishCharacter = vi.fn()
        const workingSet = new ActiveWorkingSet({
            store: reader,
            coordinator: {
                revision: imported.revision,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
            },
            getSelectedCharacterId: () => 'char-b',
            publishCharacter,
            publishConversation: vi.fn(),
        })

        expect(await workingSet.activateCharacter('char-b')).toBe(false)
        expect(publishCharacter).not.toHaveBeenCalled()
        expect(vi.mocked(reader.queryConversations)).toHaveBeenCalledTimes(2)
        expect((await originalQuery({ characterId: 'char-b', order: 'configured', limit: 100, cursor: '1' })).items).toEqual([])
    })
})
