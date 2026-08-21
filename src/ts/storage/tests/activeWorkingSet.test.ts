import { describe, expect, it, vi } from 'vitest'
import { ActiveWorkingSet } from '../activeWorkingSet.svelte'
import type { Chat, Database, character, groupChat } from '../database.svelte'
import type {
    ConversationPage,
    PersistentDataStore,
    PersistentRevisionLease,
} from '../persistentDataStore'

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
        adoptHydratedCharacter: vi.fn(() => true),
    }
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision: 1, value: { username: 'Fixture' } })),
        acquireRevision: vi.fn(async () => lease),
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
        expect(lease.release).toHaveBeenCalledTimes(1)
        expect(harness.coordinator.adoptHydratedCharacter).toHaveBeenCalledWith(
            1,
            harness.publishedCharacters[0],
        )
        expect(
            harness.coordinator.adoptHydratedCharacter.mock.invocationCallOrder[0],
        ).toBeLessThan(harness.publishCharacter.mock.invocationCallOrder[0])
    })

    it('publishes only the newest rapid character navigation', async () => {
        const a = deferred<ReturnType<PersistentRevisionLease['readCharacter']> extends Promise<infer T> ? T : never>()
        const leaseA = makeLease({
            characterId: 'char-a',
            readCharacter: vi.fn(() => a.promise),
        })
        const leaseB = makeLease({ characterId: 'char-b' })
        const harness = makeHarness(leaseA)
        vi.mocked(harness.store.acquireRevision)
            .mockResolvedValueOnce(leaseA)
            .mockResolvedValueOnce(leaseB)

        const first = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(harness.store.acquireRevision).toHaveBeenCalledTimes(1))
        const second = harness.workingSet.activateCharacter('char-b')
        await second
        a.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await first).toBe(false)
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
        expect(leaseA.release).toHaveBeenCalledTimes(1)
        expect(leaseB.release).toHaveBeenCalledTimes(1)
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
        expect(lease.release).toHaveBeenCalledTimes(1)
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
        expect(lease.release).toHaveBeenCalledTimes(1)
    })

    it('discards hydration when the coordinator revision changes', async () => {
        const lease = makeLease({ revision: 1, characterId: 'char-a' })
        const harness = makeHarness(lease)
        Object.defineProperty(harness.coordinator, 'revision', { value: 2, writable: true })

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
        expect(lease.release).toHaveBeenCalledTimes(1)
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
        await vi.waitFor(() => expect(lease.readCharacter).toHaveBeenCalledTimes(1))
        harness.coordinator.revision = 2
        detail.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await activation).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
        expect(lease.release).toHaveBeenCalledTimes(1)
    })

    it('does not publish when the hydrated baseline can no longer be adopted', async () => {
        const lease = makeLease({ characterId: 'char-a' })
        const harness = makeHarness(lease)
        harness.coordinator.adoptHydratedCharacter.mockReturnValueOnce(false)

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(false)

        expect(harness.publishedCharacters).toEqual([])
        expect(lease.release).toHaveBeenCalledTimes(1)
    })

    it('opens the store revision and initializes coordinator baselines', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))

        await harness.workingSet.initializeActiveWorkingSet(harness.database)

        expect(harness.store.open).toHaveBeenCalledTimes(1)
        expect(harness.coordinator.initialize).toHaveBeenCalledWith(1)
    })
})
