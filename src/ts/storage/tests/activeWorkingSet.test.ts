import { IDBFactory, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { ActiveWorkingSet } from '../activeWorkingSet.svelte'
import type { Chat, Database, character, groupChat } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { installMaximumCompatibilityWorkingSet } from '../persistentDataRuntime'
import { isCatalogCharacterStub } from '../workingSetCatalog'
import { WorkingSetResidencyRegistry } from '../workingSetResidency'
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
        queryPresets: vi.fn(async () => ({ revision, items: [] })),
        readPreset: vi.fn(async () => null),
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
        mutationGeneration: 0,
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
    const publishCharacterSet = vi.fn((
        primary: character | groupChat,
        related: Array<character | groupChat>,
    ) => {
        publishedCharacters.push(...related as character[])
        selectedCharacterId = primary.chaId
        publishedCharacters.push(primary as character)
    })
    const releaseInactiveCharacter = vi.fn()
    let releaseAllowed = true
    let workingSetActivationAllowed = true
    let workingSetReleaseAllowed = true
    const workingSet = new ActiveWorkingSet({
        store,
        coordinator: coordinator as never,
        getSelectedCharacterId: () => selectedCharacterId,
        publishCharacter,
        publishCharacterSet,
        publishConversation: (characterId, conversation) => {
            publishedConversations.push({ characterId, conversation })
        },
        canActivateWorkingSet: () => workingSetActivationAllowed,
        canDeactivateWorkingSet: () => workingSetReleaseAllowed,
        canDeactivateCharacter: () => releaseAllowed,
        releaseInactiveCharacter,
    })
    return {
        workingSet,
        store,
        coordinator,
        database,
        publishedCharacters,
        publishedConversations,
        publishCharacter,
        publishCharacterSet,
        releaseInactiveCharacter,
        setSelectedCharacterId(id: string) {
            selectedCharacterId = id
        },
        setReleaseAllowed(allowed: boolean) {
            releaseAllowed = allowed
        },
        setWorkingSetActivationAllowed(allowed: boolean) {
            workingSetActivationAllowed = allowed
        },
        setWorkingSetReleaseAllowed(allowed: boolean) {
            workingSetReleaseAllowed = allowed
        },
    }
}

describe('ActiveWorkingSet', () => {
    it('reconciles selected group dependencies from an authoritative snapshot', () => {
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        const database = {
            characters: [
                {
                    type: 'group',
                    chaId: 'group-a',
                    characters: ['member-b', 'missing', 'member-c', 'member-b'],
                    chats: [],
                },
                makeCharacter('member-b'),
                makeCharacter('member-c'),
            ],
        } as unknown as Database

        expect([
            ...harness.workingSet.reconcileActiveCharacterIds(database, 'group-a'),
        ]).toEqual(['group-a', 'member-b', 'member-c'])
        expect([...harness.workingSet.activeCharacterIds]).toEqual([
            'group-a',
            'member-b',
            'member-c',
        ])
    })

    it('treats a selected catalog group stub as inactive', () => {
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        const database = {
            characters: [{ type: 'group', chaId: 'group-a', name: 'Group' }],
        } as unknown as Database

        expect([
            ...harness.workingSet.reconcileActiveCharacterIds(database, 'group-a'),
        ]).toEqual([])
    })

    it('flushes before releasing all active characters on leave', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.coordinator.flushPendingData.mockClear()

        await expect(harness.workingSet.deactivate()).resolves.toBe(true)

        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('char-a')
        expect(harness.coordinator.flushPendingData).toHaveBeenCalledWith(
            'deactivate-working-set',
        )
        expect(harness.coordinator.flushPendingData.mock.invocationCallOrder[0]).toBeLessThan(
            harness.releaseInactiveCharacter.mock.invocationCallOrder[0],
        )
        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
    })

    it('keeps dirty active characters resident when leave flush fails', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.coordinator.flushPendingData.mockRejectedValueOnce(new Error('flush failed'))

        await expect(harness.workingSet.deactivate()).rejects.toThrow('flush failed')

        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect([...harness.workingSet.activeCharacterIds]).toEqual(['char-a'])
    })

    it('keeps a streaming character active until a later leave after settlement', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.setReleaseAllowed(false)

        await expect(harness.workingSet.deactivate()).resolves.toBe(false)

        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect([...harness.workingSet.activeCharacterIds]).toEqual(['char-a'])

        harness.setReleaseAllowed(true)
        await expect(harness.workingSet.deactivate()).resolves.toBe(true)
        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('char-a')
        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
    })

    it('keeps the active character resident while generation is busy before streaming starts', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.coordinator.flushPendingData.mockClear()
        harness.setWorkingSetReleaseAllowed(false)

        await expect(harness.workingSet.deactivate()).resolves.toBe(false)

        expect(harness.coordinator.flushPendingData).not.toHaveBeenCalled()
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect([...harness.workingSet.activeCharacterIds]).toEqual(['char-a'])
    })

    it('allows maximum compatibility to leave while retaining complete data', async () => {
        const harness = makeHarness(makeLease({ characterId: 'char-a' }))
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.releaseInactiveCharacter.mockReturnValueOnce(false)

        await expect(harness.workingSet.deactivate()).resolves.toBe(true)

        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('char-a')
        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
    })

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
            0,
            harness.publishedCharacters[0],
        )
        expect(
            harness.coordinator.adoptHydratedCharacter.mock.invocationCallOrder[0],
        ).toBeLessThan(harness.publishCharacter.mock.invocationCallOrder[0])
    })

    it('releases the previous character only after the hydrated target is published', async () => {
        const lease = makeLease({ characterId: 'char-a', chats: [makeChat('chat-a')] })
        const harness = makeHarness(lease)

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('previous')
        expect(harness.coordinator.flushPendingData.mock.invocationCallOrder[0]).toBeLessThan(
            harness.releaseInactiveCharacter.mock.invocationCallOrder[0],
        )
        expect(harness.publishCharacter.mock.invocationCallOrder[0]).toBeLessThan(
            harness.releaseInactiveCharacter.mock.invocationCallOrder[0],
        )
    })

    it('bounds full character detail while visiting A then B then C', async () => {
        const database = {
            characters: ['a', 'b', 'c'].map((id) => ({
                ...makeCharacter(`char-${id}`, [makeChat(`chat-${id}`)]),
                personality: `body-${id}`,
            })),
        } as unknown as Database
        const authoritative = structuredClone(database.characters)
        let selectedCharacterId = 'char-a'
        const residency = new WorkingSetResidencyRegistry()
        const store = {
            readCharacter: vi.fn(async (id: string) => {
                const character = authoritative.find((candidate) => candidate.chaId === id)
                if (!character) return null
                const { chats: _chats, ...detail } = character
                return { revision: 1, value: detail }
            }),
            queryConversations: vi.fn(async ({ characterId }: { characterId: string }) => {
                const character = authoritative.find((candidate) => candidate.chaId === characterId)!
                return {
                    revision: 1,
                    items: character.chats.map((chat, configuredIndex) => ({
                        id: chat.id!,
                        characterId,
                        name: chat.name,
                        configuredIndex,
                        recentAt: 0,
                        messageCount: chat.message.length,
                    })),
                }
            }),
            readConversation: vi.fn(async (characterId: string, conversationId: string) => {
                const character = authoritative.find((candidate) => candidate.chaId === characterId)!
                return {
                    revision: 1,
                    value: structuredClone(
                        character.chats.find((chat) => chat.id === conversationId)!,
                    ),
                }
            }),
        } as unknown as PersistentDataStore
        const workingSet = new ActiveWorkingSet({
            store,
            coordinator: {
                revision: 1,
                mutationGeneration: 0,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
            },
            getSelectedCharacterId: () => selectedCharacterId,
            publishCharacter: (character) => {
                const index = database.characters.findIndex(
                    (candidate) => candidate.chaId === character.chaId,
                )
                database.characters[index] = character
                residency.markCharacterHydrated(character.chaId)
                selectedCharacterId = character.chaId
            },
            publishCharacterSet: vi.fn(),
            publishConversation: vi.fn(),
            releaseInactiveCharacter: (id) => {
                residency.releaseCharacterToCatalog(database, id)
            },
        })

        expect(await workingSet.activateCharacter('char-b')).toBe(true)
        expect(await workingSet.activateCharacter('char-c')).toBe(true)

        expect(database.characters.map(isCatalogCharacterStub)).toEqual([true, true, false])
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(database.characters[1]).not.toHaveProperty('personality')
        expect(database.characters[2]).toHaveProperty('personality', 'body-c')
    })

    it('publishes a group only after every unique member is completely hydrated', async () => {
        const memberTwo = deferred<{
            revision: number
            value: Omit<character, 'chats'>
        } | null>()
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['member-a', 'member-b', 'member-a'],
        } as Omit<groupChat, 'chats'>
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        vi.mocked(harness.store.readCharacter).mockImplementation(async (id) => {
            if (id === 'group-a') return { revision: 1, value: group }
            if (id === 'member-a') return { revision: 1, value: makeCharacterDetail(id) }
            if (id === 'member-b') return memberTwo.promise
            return null
        })
        vi.mocked(harness.store.queryConversations).mockResolvedValue({
            revision: 1,
            items: [],
        })

        const activation = harness.workingSet.activateCharacter('group-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledTimes(3))
        expect(harness.publishCharacterSet).not.toHaveBeenCalled()

        memberTwo.resolve({ revision: 1, value: makeCharacterDetail('member-b') })
        expect(await activation).toBe(true)

        expect(harness.publishCharacterSet).toHaveBeenCalledWith(
            expect.objectContaining({ chaId: 'group-a' }),
            [
                expect.objectContaining({ chaId: 'member-a', chats: [] }),
                expect.objectContaining({ chaId: 'member-b', chats: [] }),
            ],
        )
        expect(harness.coordinator.adoptHydratedCharacter).toHaveBeenCalledOnce()
        expect([...harness.workingSet.activeCharacterIds]).toEqual([
            'group-a',
            'member-a',
            'member-b',
        ])
    })

    it('bounds concurrent group member hydration while preserving member order', async () => {
        const memberIds = Array.from({ length: 12 }, (_, index) => `member-${index}`)
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: memberIds,
        } as Omit<groupChat, 'chats'>
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        let inFlight = 0
        let peakInFlight = 0
        vi.mocked(harness.store.readCharacter).mockImplementation(async (id) => {
            if (id === 'group-a') return { revision: 1, value: group }
            inFlight++
            peakInFlight = Math.max(peakInFlight, inFlight)
            await new Promise((resolve) => setTimeout(resolve, 5))
            inFlight--
            return { revision: 1, value: makeCharacterDetail(id) }
        })
        vi.mocked(harness.store.queryConversations).mockResolvedValue({
            revision: 1,
            items: [],
        })

        expect(await harness.workingSet.activateCharacter('group-a')).toBe(true)

        expect(peakInFlight).toBeLessThanOrEqual(4)
        expect(harness.publishCharacterSet.mock.calls[0][1].map((member) => member.chaId))
            .toEqual(memberIds)
    })

    it('stops scheduling group member chunks after navigation is superseded', async () => {
        const memberIds = Array.from({ length: 8 }, (_, index) => `member-${index}`)
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: memberIds,
        } as Omit<groupChat, 'chats'>
        const firstChunk = deferred<void>()
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        vi.mocked(harness.store.readCharacter).mockImplementation(async (id) => {
            if (id === 'group-a') return { revision: 1, value: group }
            await firstChunk.promise
            return { revision: 1, value: makeCharacterDetail(id) }
        })
        vi.mocked(harness.store.queryConversations).mockResolvedValue({
            revision: 1,
            items: [],
        })

        const activation = harness.workingSet.activateCharacter('group-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledTimes(5))
        harness.workingSet.invalidateNavigation()
        firstChunk.resolve()

        expect(await activation).toBe(false)
        expect(harness.store.readCharacter).toHaveBeenCalledTimes(5)
    })

    it('releases group members after navigating away from the group', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['member-a', 'member-b'],
        } as Omit<groupChat, 'chats'>
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        vi.mocked(harness.store.readCharacter).mockImplementation(async (id) => ({
            revision: 1,
            value: id === 'group-a' ? group : makeCharacterDetail(id),
        }))
        vi.mocked(harness.store.queryConversations).mockResolvedValue({
            revision: 1,
            items: [],
        })
        await harness.workingSet.activateCharacter('group-a')
        harness.releaseInactiveCharacter.mockClear()

        await harness.workingSet.activateCharacter('char-next')

        expect(harness.releaseInactiveCharacter.mock.calls.map(([id]) => id)).toEqual([
            'group-a',
            'member-a',
            'member-b',
        ])
    })

    it('opens a group after permanently deleted or trash-expired members are removed', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['member-a', 'deleted-a', 'member-b', 'deleted-b'],
            characterTalks: [0.1, 0.2, 0.3, 0.4],
            characterActive: [true, false, true, false],
        } as Omit<groupChat, 'chats'>
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        vi.mocked(harness.store.readCharacter).mockImplementation(async (id) => {
            if (id === 'group-a') return { revision: 1, value: group }
            if (id === 'member-a' || id === 'member-b') {
                return { revision: 1, value: makeCharacterDetail(id) }
            }
            return null
        })
        vi.mocked(harness.store.queryConversations).mockResolvedValue({
            revision: 1,
            items: [],
        })

        expect(await harness.workingSet.activateCharacter('group-a')).toBe(true)

        const [publishedGroup, publishedMembers] = harness.publishCharacterSet.mock.calls[0]
        expect(publishedGroup).toMatchObject({
            characters: ['member-a', 'member-b'],
            characterTalks: [0.1, 0.3],
            characterActive: [true, true],
        })
        expect(publishedMembers.map((member) => member.chaId)).toEqual(['member-a', 'member-b'])
        expect(harness.coordinator.adoptHydratedCharacter).toHaveBeenCalledWith(
            1,
            0,
            expect.objectContaining({
                characters: ['member-a', 'deleted-a', 'member-b', 'deleted-b'],
            }),
        )
        expect([...harness.workingSet.activeCharacterIds]).toEqual([
            'group-a',
            'member-a',
            'member-b',
        ])
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
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('does not release the current character when exact hydration is cancelled', async () => {
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
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
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

    it('does not publish a stale conversation after the resident chat changes', async () => {
        const conversationRead = deferred<{ revision: number; value: Chat } | null>()
        const lease = makeLease({
            characterId: 'char-a',
            readConversation: vi.fn(() => conversationRead.promise),
        })
        const harness = makeHarness(lease)
        harness.setSelectedCharacterId('char-a')

        const activation = harness.workingSet.activateConversation('chat-a')
        await vi.waitFor(() => expect(harness.store.readConversation).toHaveBeenCalledOnce())
        harness.coordinator.mutationGeneration++
        conversationRead.resolve({ revision: 1, value: makeChat('chat-a') })

        expect(await activation).toBe(false)
        expect(harness.publishedConversations).toEqual([])
    })

    it('does not adopt a hydrated body after the resident working set changes', async () => {
        const detail = deferred<{ revision: number; value: Omit<character, 'chats'> } | null>()
        const lease = makeLease({
            revision: 1,
            characterId: 'char-a',
            readCharacter: vi.fn(() => detail.promise),
        })
        const harness = makeHarness(lease)

        const activation = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledOnce())
        harness.coordinator.mutationGeneration++
        detail.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await activation).toBe(false)
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.publishedCharacters).toEqual([])
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
    })

    it('does not publish or release when generation starts during character hydration', async () => {
        const detail = deferred<{ revision: number; value: Omit<character, 'chats'> } | null>()
        const lease = makeLease({
            revision: 1,
            characterId: 'char-a',
            readCharacter: vi.fn(() => detail.promise),
        })
        const harness = makeHarness(lease)

        const activation = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledOnce())
        harness.setWorkingSetActivationAllowed(false)
        detail.resolve({ revision: 1, value: makeCharacterDetail('char-a') })

        expect(await activation).toBe(false)
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.publishCharacter).not.toHaveBeenCalled()
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
    })

    it('does not publish a group when generation starts during member hydration', async () => {
        const member = deferred<{ revision: number; value: Omit<character, 'chats'> } | null>()
        const harness = makeHarness(makeLease({ characterId: 'group-a' }))
        vi.mocked(harness.store.readCharacter).mockImplementation(async (id) => {
            if (id === 'group-a') {
                return {
                    revision: 1,
                    value: {
                        type: 'group',
                        chaId: 'group-a',
                        characters: ['member-a'],
                        characterTalks: [0.5],
                        characterActive: [true],
                    } as Omit<groupChat, 'chats'>,
                }
            }
            return member.promise
        })
        vi.mocked(harness.store.queryConversations).mockResolvedValue({
            revision: 1,
            items: [],
        })

        const activation = harness.workingSet.activateCharacter('group-a')
        await vi.waitFor(() => expect(harness.store.readCharacter).toHaveBeenCalledWith('member-a'))
        harness.setWorkingSetActivationAllowed(false)
        member.resolve({ revision: 1, value: makeCharacterDetail('member-a') })

        expect(await activation).toBe(false)
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.publishCharacterSet).not.toHaveBeenCalled()
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
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
            mutationGeneration: 0,
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
            publishCharacterSet: (primary, related) => published.push(...related, primary),
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
                mutationGeneration: 0,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
            },
            getSelectedCharacterId: () => 'char-b',
            publishCharacter,
            publishCharacterSet: (primary, related) => {
                for (const value of related) publishCharacter(value)
                publishCharacter(primary)
            },
            publishConversation: vi.fn(),
        })

        expect(await workingSet.activateCharacter('char-b')).toBe(false)
        expect(publishCharacter).not.toHaveBeenCalled()
        expect(vi.mocked(reader.queryConversations)).toHaveBeenCalledTimes(2)
        expect((await originalQuery({ characterId: 'char-b', order: 'configured', limit: 100, cursor: '1' })).items).toEqual([])
    })
})

describe('maximum compatibility working set installation', () => {
    it('captures stable selection IDs after flushing before materialization', async () => {
        const database = {
            username: 'Complete',
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        } as unknown as Database
        const events: string[] = []
        let selectedCharacterId = 'char-a'
        let selectedConversationId = 'chat-a'

        await installMaximumCompatibilityWorkingSet({
            getSelectedCharacterId: () => selectedCharacterId,
            getSelectedConversationId: () => selectedConversationId,
            flushPendingData: async () => {
                events.push('flush')
                selectedCharacterId = 'changed-character'
                selectedConversationId = 'changed-conversation'
            },
            getRevision: () => 7,
            getMutationGeneration: () => 0,
            getNavigationGeneration: () => 0,
            materializeDatabase: async (revision) => {
                events.push(`materialize:${revision}`)
                return database
            },
            installCompleteDatabase: (candidate) => {
                expect(candidate).toBe(database)
                events.push('install')
            },
            restoreSelection: (characterId, conversationId) => {
                events.push(`restore:${characterId}:${conversationId}`)
            },
            adoptMaterializedDatabase: (revision, _mutationGeneration, candidate) => {
                expect(candidate).toBe(database)
                events.push(`baseline:${revision}`)
                return true
            },
        })

        expect(events).toEqual([
            'flush',
            'materialize:7',
            'baseline:7',
            'install',
            'restore:changed-character:changed-conversation',
        ])
    })

    it('keeps the existing working set when pinned materialization fails', async () => {
        const error = new Error('materialization failed')
        const installCompleteDatabase = vi.fn()
        const restoreSelection = vi.fn()

        await expect(installMaximumCompatibilityWorkingSet({
            getSelectedCharacterId: () => 'char-a',
            getSelectedConversationId: () => 'chat-a',
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 3,
            materializeDatabase: vi.fn(async () => { throw error }),
            installCompleteDatabase,
            restoreSelection,
            getMutationGeneration: () => 0,
            getNavigationGeneration: () => 0,
            adoptMaterializedDatabase: vi.fn(() => true),
        })).rejects.toBe(error)

        expect(installCompleteDatabase).not.toHaveBeenCalled()
        expect(restoreSelection).not.toHaveBeenCalled()
    })

    it('retries when edits and navigation change while materialization is pending', async () => {
        const stale = {
            username: 'Stale',
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        } as unknown as Database
        const fresh = {
            username: 'Fresh',
            characters: [makeCharacter('char-b', [makeChat('chat-b')])],
        } as unknown as Database
        const firstMaterialization = deferred<Database>()
        let revision = 7
        let mutationGeneration = 0
        let navigationGeneration = 0
        let selectedCharacterId = 'char-a'
        let selectedConversationId = 'chat-a'
        let flushCount = 0
        const installCompleteDatabase = vi.fn()
        const restoreSelection = vi.fn()
        const materializeDatabase = vi.fn((candidateRevision: number) =>
            candidateRevision === 7 ? firstMaterialization.promise : Promise.resolve(fresh),
        )

        const installation = installMaximumCompatibilityWorkingSet({
            getSelectedCharacterId: () => selectedCharacterId,
            getSelectedConversationId: () => selectedConversationId,
            flushPendingData: async () => {
                flushCount++
                if (flushCount === 2) revision = 8
            },
            getRevision: () => revision,
            getMutationGeneration: () => mutationGeneration,
            getNavigationGeneration: () => navigationGeneration,
            materializeDatabase,
            installCompleteDatabase,
            restoreSelection,
            adoptMaterializedDatabase: (candidateRevision, candidateGeneration) =>
                candidateRevision === revision && candidateGeneration === mutationGeneration,
        })
        await vi.waitFor(() => expect(materializeDatabase).toHaveBeenCalledWith(7))

        mutationGeneration++
        navigationGeneration++
        selectedCharacterId = 'char-b'
        selectedConversationId = 'chat-b'
        firstMaterialization.resolve(stale)
        await installation

        expect(materializeDatabase.mock.calls.map(([candidateRevision]) => candidateRevision)).toEqual([
            7,
            8,
        ])
        expect(installCompleteDatabase).toHaveBeenCalledOnce()
        expect(installCompleteDatabase).toHaveBeenCalledWith(fresh)
        expect(restoreSelection).toHaveBeenCalledWith('char-b', 'chat-b')
    })

    it('leaves the current working set installed when bounded materialization retries stay stale', async () => {
        const database = {
            username: 'Candidate',
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        } as unknown as Database
        let navigationGeneration = 0
        const installCompleteDatabase = vi.fn()
        const restoreSelection = vi.fn()
        const materializeDatabase = vi.fn(async () => {
            navigationGeneration++
            return database
        })

        await expect(installMaximumCompatibilityWorkingSet({
            getSelectedCharacterId: () => 'char-a',
            getSelectedConversationId: () => 'chat-a',
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 7,
            getMutationGeneration: () => 0,
            getNavigationGeneration: () => navigationGeneration,
            materializeDatabase,
            installCompleteDatabase,
            restoreSelection,
            adoptMaterializedDatabase: vi.fn(() => true),
        })).rejects.toThrow('Working set changed during maximum compatibility materialization')

        expect(materializeDatabase).toHaveBeenCalledTimes(3)
        expect(installCompleteDatabase).not.toHaveBeenCalled()
        expect(restoreSelection).not.toHaveBeenCalled()
    })
})
