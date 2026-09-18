import { IDBFactory, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { ActiveWorkingSet } from '../activeWorkingSet.svelte'
import type { Chat, Database, character, groupChat } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
} from '../persistentDataRuntime'
import { isCatalogCharacterStub } from '../workingSetCatalog'
import { WorkingSetResidencyRegistry } from '../workingSetResidency'
import type {
    ConversationPage,
    PersistentConversationMetadata,
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
        readCharacterSummary: vi.fn(async () => null),
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
                    folderId: chat.folderId,
                    bindedPersona: chat.bindedPersona,
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
        readConversationMetadata: vi.fn(async () => null),
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
        release: vi.fn(async () => undefined),
    }
}

function makeHarness(
    lease: PersistentRevisionLease,
    options: { hydrateFullCharacter?: boolean } = {},
) {
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
        replacePersistentDatabase: vi.fn(async () => ({
            kind: 'committed', revision: 1, projection: 'applied',
        } as const)),
        adoptHydratedCharacter: vi.fn(() => true),
        markPersistentDataDirty: vi.fn(),
        recordActiveConversationMutation: vi.fn(),
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
        getResidentCharacter: (id) =>
            database.characters.find((character) => character.chaId === id) ?? null,
        publishCharacter,
        publishCharacterSet,
        publishConversation: (characterId, conversation, nextCharacter) => {
            if (nextCharacter) {
                const index = database.characters.findIndex(
                    (character) => character.chaId === characterId,
                )
                if (index >= 0) database.characters[index] = nextCharacter
            }
            publishedConversations.push({ characterId, conversation })
        },
        canActivateWorkingSet: () => workingSetActivationAllowed,
        canDeactivateWorkingSet: () => workingSetReleaseAllowed,
        canDeactivateCharacter: () => releaseAllowed,
        releaseInactiveCharacter,
        shouldHydrateFullCharacter: () => options.hydrateFullCharacter === true,
        canReleaseConversation: (character, conversationId) =>
            releaseAllowed && !character.chats.find(
                (conversation) => conversation.id === conversationId,
            )?.isStreaming,
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

function makeWindowedHarness(input: {
    characters: character[]
    selectedCharacterId?: string | null
    adoptionResult?: boolean
}) {
    const database = {
        username: 'Windowed fixture',
        characters: structuredClone(input.characters),
    } as unknown as Database
    const authoritative = structuredClone(input.characters)
    let selectedCharacterId = input.selectedCharacterId ?? null
    let storeRevision = 1
    let windowedAllowed = true
    const readConversation = vi.fn(async () => {
        throw new Error(
            'windowed activation performed a full conversation read',
        )
    })
    const readConversationMetadata = vi.fn(
        async (characterId: string, conversationId: string) => {
            const owner = authoritative.find(
                (candidate) => candidate.chaId === characterId,
            )
            const conversation = owner?.chats.find(
                (candidate) => candidate.id === conversationId,
            )
            if (!conversation) return null
            const { message, ...metadata } = structuredClone(conversation)
            return {
                revision: storeRevision,
                value: {
                    characterId,
                    conversationId,
                    conversation: metadata,
                    totalMessages: message.length,
                } satisfies PersistentConversationMetadata,
            }
        },
    )
    const store = {
        readCharacter: vi.fn(async (id: string) => {
            const owner = authoritative.find(
                (candidate) => candidate.chaId === id,
            )
            if (!owner) return null
            const { chats: _chats, ...detail } = owner
            return { revision: storeRevision, value: detail }
        }),
        queryConversations: vi.fn(
            async ({ characterId }: { characterId: string }) => {
                const owner = authoritative.find(
                    (candidate) => candidate.chaId === characterId,
                )
                return {
                    revision: storeRevision,
                    items: (owner?.chats ?? []).map(
                        (conversation, configuredIndex) => ({
                            id: conversation.id!,
                            characterId,
                            name: conversation.name,
                            folderId: conversation.folderId,
                            bindedPersona: conversation.bindedPersona,
                            configuredIndex,
                            recentAt: conversation.lastDate ?? 17,
                            messageCount: conversation.message.length,
                            fmIndex: conversation.fmIndex,
                        }),
                    ),
                }
            },
        ),
        readConversationMetadata,
        readConversation,
        readConversationWindow: vi.fn(
            async ({
                characterId,
                conversationId,
                startIndex = 0,
                limit = 64,
            }) => {
                const owner = authoritative.find(
                    (candidate) => candidate.chaId === characterId,
                )
                const conversation = owner?.chats.find(
                    (candidate) => candidate.id === conversationId,
                )
                if (!conversation) return null
                const messages = conversation.message.slice(
                    startIndex,
                    startIndex + limit,
                )
                return {
                    revision: storeRevision,
                    value: {
                        characterId,
                        conversationId,
                        messages,
                        startIndex,
                        endIndex: startIndex + messages.length,
                        totalMessages: conversation.message.length,
                        hasMoreBefore: startIndex > 0,
                        hasMoreAfter:
                            startIndex + messages.length <
                            conversation.message.length,
                    },
                }
            },
        ),
    } as unknown as PersistentDataStore
    let workingSet!: ActiveWorkingSet
    const coordinator = {
        revision: 1,
        mutationGeneration: 0,
        hasPendingPersistenceWork: false,
        initialize: vi.fn(),
        flushPendingData: vi.fn(async () => undefined),
        replacePersistentDatabase: vi.fn(async () => undefined),
        adoptHydratedCharacter: vi.fn(() => true),
        runSelectedConversationTransition: vi.fn(<T>(transition: () => T) =>
            transition(),
        ),
        adoptWindowedSelectedConversation: vi.fn(
            () => input.adoptionResult ?? true,
        ),
        advanceWindowedSelectedConversationRevision: vi.fn(() => true),
        markPersistentDataDirty: vi.fn(),
        recordActiveConversationMutation: vi.fn(),
    }
    const publishCharacter = vi.fn((next: character | groupChat) => {
        const index = database.characters.findIndex(
            (candidate) => candidate.chaId === next.chaId,
        )
        if (index < 0) database.characters.push(next)
        else database.characters[index] = next
        selectedCharacterId = next.chaId
    })
    const publishConversation = vi.fn(
        (
            characterId: string,
            conversation: Chat,
            nextCharacter?: character | groupChat,
        ) => {
            const index = database.characters.findIndex(
                (candidate) => candidate.chaId === characterId,
            )
            if (index < 0)
                throw new Error('published conversation owner is missing')
            if (nextCharacter) database.characters[index] = nextCharacter
            else {
                const owner = database.characters[index]
                const conversationIndex = owner.chats.findIndex(
                    (candidate) => candidate.id === conversation.id,
                )
                owner.chats[conversationIndex] = conversation
                owner.chatPage = conversationIndex
            }
            selectedCharacterId = characterId
        },
    )
    const captureActivationRollback = vi.fn(
        (characterIds: readonly string[]) => {
            const selectedBefore = selectedCharacterId
            const entries = new Map(
                characterIds.map((id) => [
                    id,
                    database.characters.find(
                        (candidate) => candidate.chaId === id,
                    ) ?? null,
                ]),
            )
            return () => {
                for (const [id, entry] of entries) {
                    const index = database.characters.findIndex(
                        (candidate) => candidate.chaId === id,
                    )
                    if (entry && index >= 0) database.characters[index] = entry
                }
                selectedCharacterId = selectedBefore
            }
        },
    )
    workingSet = new ActiveWorkingSet({
        store,
        coordinator:
            coordinator as unknown as import('../activeWorkingSet.svelte').WorkingSetCoordinator,
        getSelectedCharacterId: () => selectedCharacterId,
        getResidentCharacter: (id) =>
            database.characters.find((candidate) => candidate.chaId === id) ??
            null,
        publishCharacter,
        publishCharacterSet: (primary) => publishCharacter(primary),
        publishConversation,
        captureActivationRollback,
        canActivateWorkingSet: () => true,
        canUseWindowedSelectedConversation: () => windowedAllowed,
        isConversationOperationActive: () => false,
        shouldHydrateFullCharacter: () => false,
        canReleaseConversation: () => true,
        conversationViewportRowBudget: 32,
    })
    return {
        workingSet,
        database,
        coordinator,
        readConversation,
        readConversationMetadata,
        captureActivationRollback,
        publishConversation,
        setStoreRevision(revision: number) {
            storeRevision = revision
        },
        setWindowedAllowed(allowed: boolean) {
            windowedAllowed = allowed
        },
    }
}

describe('ActiveWorkingSet', () => {
    it('propagates a windowed transition failure after restoring the previous selection', async () => {
        const harness = makeWindowedHarness({
            characters: [makeCharacter('a', [makeChat('chat-a')]), makeCharacter('b', [makeChat('chat-b')])],
        })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        const previous = harness.database.characters[0]
        const error = new Error('Synthetic pending persistence failure')
        harness.coordinator.runSelectedConversationTransition.mockImplementationOnce(() => { throw error })
        await expect(harness.workingSet.activateCharacter('b')).rejects.toBe(error)
        expect(harness.database.characters[0]).toBe(previous)
        expect(harness.workingSet.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'a', conversationId: 'chat-a',
        })
        await expect(harness.workingSet.activateCharacter('b')).resolves.toBe(true)
    })

    it('directly activates a large selected conversation from metadata', async () => {
        const conversation = {
            ...makeChat('chat-large'),
            note: 'preserved note',
            fmIndex: 7,
            message: Array.from({ length: 10_000 }, (_, index) => ({
                role: 'user',
                data: `message-${index}`,
            })),
        } as Chat
        const target = makeCharacter('char-large', [conversation])
        const harness = makeWindowedHarness({ characters: [target] })

        await expect(
            harness.workingSet.activateCharacter('char-large'),
        ).resolves.toBe(true)

        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(harness.readConversationMetadata).toHaveBeenCalledOnce()
        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(
            harness.workingSet.captureSelectedConversationAuthority(),
        ).toMatchObject({
            characterId: 'char-large',
            conversationId: 'chat-large',
            totalMessages: 10_000,
        })
        expect(
            harness.workingSet.activeConversationViewportSource?.snapshot(),
        ).toMatchObject({
            totalMessages: 10_000,
        })
        expect(
            harness.workingSet.activeConversationViewportSource
                ?.snapshot()
                .rowAt(0),
        ).toBeUndefined()
        const selected = harness.database.characters[0].chats[0]
        expect(selected.note).toBe('preserved note')
        expect(selected.fmIndex).toBe(7)
        expect(() => selected.message).toThrow('metadata-only')
    })

    it('fences captured targets while retaining the selected viewport owner', async () => {
        const target = makeCharacter('char-a', [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'persisted' }],
            } as Chat,
        ])
        const harness = makeWindowedHarness({ characters: [target] })
        await harness.workingSet.activateCharacter('char-a')
        const staleTarget =
            harness.workingSet.captureSelectedConversationTarget()!
        const source = harness.workingSet.activeConversationViewportSource

        const generation = harness.workingSet.fenceNavigation()
        const currentTarget =
            harness.workingSet.captureSelectedConversationTarget()!

        expect(generation).toBe(staleTarget.navigationGeneration + 1)
        expect(currentTarget.navigationGeneration).toBe(generation)
        expect(currentTarget).not.toEqual(staleTarget)
        expect(harness.workingSet.activeConversationViewportSource).toBe(source)
        expect(
            harness.workingSet.captureSelectedConversationAuthority(),
        ).not.toBeNull()
        await expect(
            harness.workingSet.acquireCompleteConversation(
                'stale',
                staleTarget,
            ),
        ).rejects.toBeInstanceOf(Error)
    })

    it('switches windowed conversations without full reads and keeps repeat selection usable', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second' }],
        } as Chat
        const target = makeCharacter('char-a', [chatA, chatB])
        const harness = makeWindowedHarness({ characters: [target] })
        await harness.workingSet.activateCharacter('char-a')
        const previousSource =
            harness.workingSet.activeConversationViewportSource!

        await expect(
            harness.workingSet.activateConversation('chat-b'),
        ).resolves.toBe(true)
        await expect(
            harness.workingSet.activateConversation('chat-b'),
        ).resolves.toBe(true)

        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(harness.publishConversation).toHaveBeenCalledOnce()
        expect(previousSource.snapshot().totalMessages).toBe(0)
        expect(
            harness.workingSet.captureSelectedConversationAuthority(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-b',
            totalMessages: 1,
        })
        expect(harness.database.characters[0].chatPage).toBe(1)
        expect(() => harness.database.characters[0].chats[1].message).toThrow(
            'metadata-only',
        )
    })

    it('keeps the previous windowed owner usable when destination adoption is rejected', async () => {
        const characterA = makeCharacter('char-a', [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'first' }],
            } as Chat,
        ])
        const characterB = makeCharacter('char-b', [
            {
                ...makeChat('chat-b'),
                message: [{ role: 'char', data: 'second' }],
            } as Chat,
        ])
        const harness = makeWindowedHarness({
            characters: [characterA, characterB],
        })
        await harness.workingSet.activateCharacter('char-a')
        const previousSource =
            harness.workingSet.activeConversationViewportSource!
        harness.coordinator.adoptWindowedSelectedConversation.mockReturnValueOnce(
            false,
        )

        await expect(
            harness.workingSet.activateCharacter('char-b'),
        ).resolves.toBe(false)

        expect(harness.workingSet.activeConversationViewportSource).toBe(
            previousSource,
        )
        expect(previousSource.snapshot().totalMessages).toBe(1)
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
        })
        expect(harness.database.characters[0].chaId).toBe('char-a')
        expect(harness.database.characters[1].chats[0].message).toEqual(
            characterB.chats[0].message,
        )
    })

    it('restores the post-flush owner when adoption rejects after its revision advances', async () => {
        const characterA = makeCharacter('char-a', [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'first' }],
            } as Chat,
        ])
        const characterB = makeCharacter('char-b', [
            {
                ...makeChat('chat-b'),
                message: [{ role: 'char', data: 'second' }],
            } as Chat,
        ])
        const harness = makeWindowedHarness({
            characters: [characterA, characterB],
        })
        await harness.workingSet.activateCharacter('char-a')
        const preFlushSource =
            harness.workingSet.activeConversationViewportSource!
        let postFlushSource = preFlushSource
        harness.setStoreRevision(2)
        harness.coordinator.flushPendingData.mockImplementationOnce(
            async () => {
                harness.coordinator.revision = 2
                harness.workingSet.advanceStoreRevision(2)
                postFlushSource =
                    harness.workingSet.activeConversationViewportSource!
            },
        )
        harness.coordinator.adoptWindowedSelectedConversation.mockReturnValueOnce(
            false,
        )

        await expect(
            harness.workingSet.activateCharacter('char-b'),
        ).resolves.toBe(false)

        expect(preFlushSource.snapshot().totalMessages).toBe(0)
        expect(harness.workingSet.activeConversationViewportSource).toBe(
            postFlushSource,
        )
        expect(postFlushSource.snapshot()).toMatchObject({
            storeRevision: 2,
            totalMessages: 1,
        })
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
            storeRevision: 2,
        })
    })

    it('does not let a stale complete fallback replace a newer character navigation', async () => {
        const characters = ['a', 'b', 'c'].map((suffix) =>
            makeCharacter(`char-${suffix}`, [
                {
                    ...makeChat(`chat-${suffix}`),
                    message: [{ role: 'user', data: suffix }],
                } as Chat,
            ]),
        )
        const harness = makeWindowedHarness({ characters })
        await harness.workingSet.activateCharacter('char-a')
        let newerNavigation: Promise<boolean> | null = null

        const staleNavigation = harness.workingSet.activateCharacter('char-b', {
            normalize(candidate) {
                harness.setWindowedAllowed(false)
                queueMicrotask(() => {
                    harness.setWindowedAllowed(true)
                    newerNavigation =
                        harness.workingSet.activateCharacter('char-c')
                })
                return candidate
            },
        })

        await expect(staleNavigation).resolves.toBe(false)
        await vi.waitFor(() => expect(newerNavigation).not.toBeNull())
        await expect(newerNavigation!).resolves.toBe(true)
        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-c',
            conversationId: 'chat-c',
        })
    })

    it('discards delayed metadata when the mutation generation advances', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second' }],
        } as Chat
        const harness = makeWindowedHarness({
            characters: [makeCharacter('char-a', [chatA, chatB])],
        })
        await harness.workingSet.activateCharacter('char-a')
        const pending = deferred<{
            revision: number
            value: PersistentConversationMetadata
        } | null>()
        harness.readConversationMetadata.mockImplementationOnce(
            () => pending.promise,
        )
        const activation = harness.workingSet.activateConversation('chat-b')
        await vi.waitFor(() =>
            expect(harness.readConversationMetadata).toHaveBeenCalledTimes(2),
        )
        harness.coordinator.mutationGeneration = 1
        const { message, ...conversation } = structuredClone(chatB)
        pending.resolve({
            revision: 1,
            value: {
                characterId: 'char-a',
                conversationId: 'chat-b',
                conversation,
                totalMessages: message.length,
            },
        })

        await expect(activation).resolves.toBe(false)
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
        })
    })

    it('passes exact detached normalization evidence and schedules persistence after adoption', async () => {
        const target = {
            ...makeCharacter('char-a', [
                {
                    ...makeChat('chat-a'),
                    note: 'before',
                    message: [{ role: 'user', data: 'persisted' }],
                } as Chat,
            ]),
            lastInteraction: 1,
        } as character
        const harness = makeWindowedHarness({ characters: [target] })

        await expect(
            harness.workingSet.activateCharacter('char-a', {
                normalize(candidate) {
                    candidate.lastInteraction = 2
                    candidate.chats[0].note = 'after'
                    return candidate
                },
            }),
        ).resolves.toBe(true)

        expect(
            harness.coordinator.adoptWindowedSelectedConversation,
        ).toHaveBeenCalledWith(
            1,
            0,
            harness.database.characters[0],
            expect.objectContaining({ conversationId: 'chat-a' }),
            {
                character: {
                    before: expect.objectContaining({
                        chaId: 'char-a',
                        lastInteraction: 1,
                    }),
                    after: expect.objectContaining({
                        chaId: 'char-a',
                        lastInteraction: 2,
                    }),
                },
                conversation: {
                    before: expect.objectContaining({
                        id: 'chat-a',
                        note: 'before',
                    }),
                    after: expect.objectContaining({
                        id: 'chat-a',
                        note: 'after',
                    }),
                },
            },
        )
        expect(
            harness.coordinator.markPersistentDataDirty,
        ).toHaveBeenCalledOnce()
        expect(
            harness.coordinator.markPersistentDataDirty.mock
                .invocationCallOrder[0],
        ).toBeGreaterThan(
            harness.coordinator.runSelectedConversationTransition.mock
                .invocationCallOrder[0],
        )
    })
    it('hydrates only the selected conversation body in the scalable working set', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            folderId: 'folder-a',
            bindedPersona: 'persona-a',
            message: [{ role: 'user', data: 'inactive body' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'user', data: 'selected body' }],
        } as Chat
        const lease = makeLease({
            characterId: 'char-a',
            chats: [chatA, chatB],
            readCharacter: vi.fn(async () => ({
                revision: 1,
                value: { ...makeCharacterDetail('char-a'), chatPage: 1 },
            })),
        })
        const harness = makeHarness(lease)

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(lease.readConversation).toHaveBeenCalledOnce()
        expect(lease.readConversation).toHaveBeenCalledWith('char-a', 'chat-b')
        expect(harness.publishedCharacters[0]).toMatchObject({
            chaId: 'char-a',
            chatPage: 1,
            chats: [
                {
                    id: 'chat-a',
                    name: 'chat-a',
                    folderId: 'folder-a',
                    bindedPersona: 'persona-a',
                    message: [],
                },
                { id: 'chat-b', name: 'chat-b', message: chatB.message },
            ],
        })
    })

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

    it('keeps the selected conversation target current when leave becomes blocked during flush', async () => {
        const harness = makeHarness(makeLease({
            characterId: 'char-a',
            chats: [makeChat('chat-a')],
        }), { hydrateFullCharacter: true })
        await harness.workingSet.activateCharacter('char-a')
        const flushing = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(flushing.promise)

        const leaving = harness.workingSet.deactivate()
        harness.setWorkingSetReleaseAllowed(false)
        flushing.resolve()

        await expect(leaving).resolves.toBe(false)
        harness.setWorkingSetReleaseAllowed(true)
        const target = harness.workingSet.captureSelectedConversationTarget()
        expect(target).not.toBeNull()
        const lease = await harness.workingSet.acquireCompleteConversation(
            'after-blocked-leave',
            target!,
        )
        lease.release()
    })

    it('keeps the selected conversation target current when activation becomes blocked during flush', async () => {
        const harness = makeHarness(makeLease({
            characterId: 'char-a',
            chats: [makeChat('chat-a'), makeChat('chat-b')],
        }), { hydrateFullCharacter: true })
        await harness.workingSet.activateCharacter('char-a')
        const flushing = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(flushing.promise)

        const activating = harness.workingSet.activateConversation('chat-b')
        harness.setWorkingSetActivationAllowed(false)
        flushing.resolve()

        await expect(activating).resolves.toBe(false)
        harness.setWorkingSetActivationAllowed(true)
        const target = harness.workingSet.captureSelectedConversationTarget()
        expect(target).not.toBeNull()
        const lease = await harness.workingSet.acquireCompleteConversation(
            'after-blocked-activation',
            target!,
        )
        lease.release()
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

    it('hydrates every conversation body for a maximum-compatibility pin', async () => {
        const chats = [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'full A' }],
            } as Chat,
            {
                ...makeChat('chat-b'),
                message: [{ role: 'char', data: 'full B' }],
            } as Chat,
        ]
        const lease = makeLease({ characterId: 'char-a', chats })
        const harness = makeHarness(lease, { hydrateFullCharacter: true })

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(harness.publishedCharacters[0]).toMatchObject({
            chaId: 'char-a',
            chats,
        })
        expect(lease.readConversation).toHaveBeenCalledTimes(2)
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
                markPersistentDataDirty: vi.fn(),
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

    it('publishes a group after every unique member detail is hydrated without member chat reads', async () => {
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

        const [publishedGroup, publishedMembers] = harness.publishCharacterSet.mock.calls[0]
        expect(publishedGroup).toEqual(expect.objectContaining({ chaId: 'group-a' }))
        expect(publishedMembers).toEqual([
            expect.objectContaining({ chaId: 'member-a', name: 'MEMBER-A' }),
            expect.objectContaining({ chaId: 'member-b', name: 'MEMBER-B' }),
        ])
        expect(publishedMembers[0]).not.toHaveProperty('chats')
        expect(publishedMembers[1]).not.toHaveProperty('chats')
        expect(harness.store.queryConversations).toHaveBeenCalledTimes(1)
        expect(harness.store.queryConversations).toHaveBeenCalledWith(
            expect.objectContaining({ characterId: 'group-a' }),
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
        const harness = makeHarness(lease, { hydrateFullCharacter: true })
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
        preparation.resolve({ database: harness.database, reason: 'character-detail-replace' })

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
            return { kind: 'committed', revision: 1, projection: 'applied' }
        })
        harness.coordinator.flushPendingData.mockImplementation(() =>
            replacementStarted ? replacement.promise : Promise.resolve(),
        )

        const first = harness.workingSet.activateCharacter('char-a', {
            prepare: async () => ({
                database: harness.database,
                reason: 'character-detail-replace',
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

    it('rejects conversation navigation when the selected character changes during flush', async () => {
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
        expect(await activation).toBe(false)

        expect(lease.readConversation).not.toHaveBeenCalled()
        expect(harness.publishedConversations).toEqual([])
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('coalesces duplicate stable-ID conversation hydration into one flight', async () => {
        const conversationRead = deferred<{ revision: number; value: Chat } | null>()
        const lease = makeLease({
            characterId: 'char-a',
            readConversation: vi.fn(() => conversationRead.promise),
        })
        const harness = makeHarness(lease)
        harness.setSelectedCharacterId('char-a')

        const first = harness.workingSet.activateConversation('chat-a')
        const second = harness.workingSet.activateConversation('chat-a')
        await vi.waitFor(() => expect(lease.readConversation).toHaveBeenCalled())
        conversationRead.resolve({ revision: 1, value: makeChat('chat-a') })

        await expect(Promise.all([first, second])).resolves.toEqual([true, true])
        expect(lease.readConversation).toHaveBeenCalledOnce()
        expect(harness.publishedConversations).toHaveLength(1)
    })

    it('starts a fresh conversation flight when a later navigation returns to the same ID', async () => {
        const firstB = deferred<{ revision: number; value: Chat } | null>()
        const chatC = deferred<{ revision: number; value: Chat } | null>()
        const latestB = deferred<{ revision: number; value: Chat } | null>()
        const bReads = [firstB, latestB]
        const readConversation = vi.fn((_characterId: string, conversationId: string) => {
            if (conversationId === 'chat-c') return chatC.promise
            return bReads.shift()!.promise
        })
        const lease = makeLease({ characterId: 'char-a', readConversation })
        const harness = makeHarness(lease)
        harness.setSelectedCharacterId('char-a')

        const staleBActivation = harness.workingSet.activateConversation('chat-b')
        await vi.waitFor(() => expect(readConversation).toHaveBeenCalledTimes(1))
        const staleCActivation = harness.workingSet.activateConversation('chat-c')
        await vi.waitFor(() => expect(readConversation).toHaveBeenCalledTimes(2))
        const latestBActivation = harness.workingSet.activateConversation('chat-b')

        await vi.waitFor(() => expect(readConversation).toHaveBeenCalledTimes(3))
        firstB.resolve({ revision: 1, value: {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'stale B' }],
        } as Chat })
        chatC.resolve({ revision: 1, value: makeChat('chat-c') })
        latestB.resolve({ revision: 1, value: {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'latest B' }],
        } as Chat })

        await expect(Promise.all([
            staleBActivation,
            staleCActivation,
            latestBActivation,
        ])).resolves.toEqual([false, false, true])
        expect(harness.publishedConversations).toEqual([{
            characterId: 'char-a',
            conversation: expect.objectContaining({
                id: 'chat-b',
                message: [{ role: 'char', data: 'latest B' }],
            }),
        }])
    })

    it('flushes the previous body before publishing its summary stub', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'dirty body' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'authoritative body' }],
        } as Chat
        let resident = makeCharacter('char-a', [chatA, makeChat('chat-b')])
        resident.chatPage = 0
        const flush = deferred<void>()
        const events: string[] = []
        const lease = makeLease({ characterId: 'char-a', chats: [chatA, chatB] })
        const coordinator = {
            revision: 1,
            mutationGeneration: 4,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => {
                events.push('flush:start')
                await flush.promise
                events.push('flush:done')
            }),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => {
                events.push('baseline')
                return true
            }),
            markPersistentDataDirty: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: {
                readConversation: vi.fn(async () => {
                    events.push('read')
                    return { revision: 1, value: structuredClone(chatB) }
                }),
            } as unknown as PersistentDataStore,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            canReleaseConversation: () => true,
            publishCharacter: vi.fn(),
            publishCharacterSet: vi.fn(),
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                events.push('publish')
                resident = nextCharacter as character
            },
        })

        const activation = workingSet.activateConversation('chat-b')
        await vi.waitFor(() => expect(coordinator.flushPendingData).toHaveBeenCalledOnce())
        expect(resident.chats[0].message).toEqual(chatA.message)
        flush.resolve()

        await expect(activation).resolves.toBe(true)
        expect(events).toEqual(['flush:start', 'flush:done', 'read', 'baseline', 'publish'])
        expect(resident.chatPage).toBe(1)
        expect(resident.chats).toMatchObject([
            { id: 'chat-a', message: [] },
            { id: 'chat-b', message: chatB.message },
        ])
    })

    it('publishes a full-array session for the live conversation after navigation', async () => {
        const chatA = makeChat('chat-a')
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'user', data: 'authoritative body', chatId: 'message-b' }],
        } as Chat
        let resident = makeCharacter('char-a', [chatA, makeChat('chat-b')])
        resident.chatPage = 0
        const coordinator = {
            revision: 1,
            mutationGeneration: 0,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => undefined),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
            markPersistentDataDirty: vi.fn(),
            recordActiveConversationMutation: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: {
                readConversation: vi.fn(async () => ({ revision: 1, value: chatB })),
            } as unknown as PersistentDataStore,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            publishCharacter: vi.fn(),
            publishCharacterSet: vi.fn(),
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                resident = nextCharacter as character
            },
        })

        await expect(workingSet.activateConversation('chat-b')).resolves.toBe(true)

        const session = workingSet.activeConversationSession!
        expect(session.storeRevision).toBe(1)
        expect(session.materializeCompatibilityArray()).toBe(resident.chats[1].message)
        const appended = session.append({
            role: 'char',
            data: 'session append',
            chatId: 'session-append',
        })
        expect(coordinator.recordActiveConversationMutation).toHaveBeenCalledWith(
            expect.objectContaining({
                characterId: 'char-a',
                conversationId: 'chat-b',
                sessionVersion: 1,
            }),
        )
        const mutation = coordinator.recordActiveConversationMutation.mock.calls[0][0]
        const persistence = workingSet.beginConversationMutationPersistence(mutation)
        expect(persistence).not.toBeNull()
        expect(session.pinCount('pending-save')).toBe(1)
        expect(workingSet.acknowledgeConversationMutationPersisted({
            characterId: 'char-a',
            conversationId: 'chat-b',
            sessionToken: mutation.sessionToken,
            sessionVersion: 1,
            revision: 2,
        })).toBe(true)
        persistence!.release()
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.persistedVersion).toBe(1)
        const edited = session.edit(appended, {
            role: 'char',
            data: 'session edit',
            chatId: 'session-append',
        })
        expect(resident.chats[1].message.at(-1)?.data).toBe('session edit')
        session.delete(edited)
        expect(resident.chats[1].message).toEqual(chatB.message)

        await expect(workingSet.activateConversation('chat-b')).resolves.toBe(true)
        expect(workingSet.activeConversationSession).not.toBe(session)
        expect(session.isActive).toBe(false)
        expect(() => session.append({ role: 'char', data: 'detached navigation write' })).toThrow(
            /inactive/,
        )

        const releasedSession = workingSet.activeConversationSession!
        await expect(workingSet.deactivate()).resolves.toBe(true)
        expect(releasedSession.isActive).toBe(false)
        expect(() => releasedSession.append({ role: 'char', data: 'detached release write' })).toThrow(
            /inactive/,
        )
    })

    it('keeps the session command body resident when its pending save fails before eviction', async () => {
        const previous = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'unsaved' }],
        } as Chat
        const resident = makeCharacter('char-a', [previous, makeChat('chat-b')])
        resident.chatPage = 0
        const database = {
            username: 'Fixture',
            characters: [resident],
        } as unknown as Database
        const readConversation = vi.fn()
        const publishConversation = vi.fn()
        const coordinator = {
            revision: 1,
            mutationGeneration: 1,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => { throw new Error('commit failed') }),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
            markPersistentDataDirty: vi.fn(),
            recordActiveConversationMutation: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: { readConversation } as unknown as PersistentDataStore,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            canReleaseConversation: () => true,
            publishCharacter: vi.fn(),
            publishCharacterSet: vi.fn(),
            publishConversation,
        })
        workingSet.installCommittedWorkingSet(database, 1)
        const session = workingSet.activeConversationSession!
        session.append({ role: 'char', data: 'pending command' })

        await expect(workingSet.activateConversation('chat-b')).rejects.toThrow('commit failed')
        expect(coordinator.recordActiveConversationMutation).toHaveBeenCalledOnce()
        expect(session.persistedVersion).toBe(0)
        expect(workingSet.activeConversationSession).toBe(session)
        expect(readConversation).not.toHaveBeenCalled()
        expect(publishConversation).not.toHaveBeenCalled()
        expect(resident.chats[0]).toBe(previous)
        expect(resident.chats[0].message).toEqual([
            { role: 'user', data: 'unsaved' },
            { role: 'char', data: 'pending command' },
        ])
        const currentTarget = workingSet.captureSelectedConversationTarget()
        expect(currentTarget).not.toBeNull()
        const currentLease = await workingSet.acquireCompleteConversation(
            'after-failed-activation',
            currentTarget!,
        )
        currentLease.release()
    })

    it('keeps a streaming previous body pinned while selecting the hydrated target', async () => {
        const streaming = {
            ...makeChat('chat-a'),
            isStreaming: true,
            message: [{ role: 'char', data: 'partial stream' }],
        } as Chat
        const target = {
            ...makeChat('chat-b'),
            message: [{ role: 'user', data: 'target' }],
        } as Chat
        let resident = makeCharacter('char-a', [streaming, makeChat('chat-b')])
        resident.chatPage = 0
        const workingSet = new ActiveWorkingSet({
            store: {
                readConversation: vi.fn(async () => ({ revision: 1, value: target })),
            } as unknown as PersistentDataStore,
            coordinator: {
                revision: 1,
                mutationGeneration: 0,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
                markPersistentDataDirty: vi.fn(),
            },
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            canReleaseConversation: (character, conversationId) =>
                !character.chats.find((conversation) => conversation.id === conversationId)
                    ?.isStreaming,
            publishCharacter: vi.fn(),
            publishCharacterSet: vi.fn(),
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                resident = nextCharacter as character
            },
        })

        await expect(workingSet.activateConversation('chat-b')).resolves.toBe(true)
        expect(resident.chats[0]).toBe(streaming)
        expect(resident.chats[0].message).toEqual([
            { role: 'char', data: 'partial stream' },
        ])
        expect(resident.chats[1]).toBe(target)
    })

    it('keeps the committed previous body authoritative after a later selected save', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'conversation-residency-commit-before-release',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second' }],
        } as Chat
        const chatC = {
            ...makeChat('chat-c'),
            fmIndex: 2,
            scriptstate: { source: 'authoritative' },
            note: 'authoritative note',
            localLore: [{ key: 'lore', content: 'authoritative lore' }],
            message: [{ role: 'user', data: 'must survive structural save' }],
        } as unknown as Chat
        let database = {
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [chatA, chatB, chatC])],
        } as unknown as Database
        database.characters[0].chatPage = 0
        const imported = await store.replaceFromDatabase(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => 'char-a',
                getSelectedConversationId: () =>
                    database.characters[0].chats[database.characters[0].chatPage].id,
                replaceDatabase: (value) => { database = value },
                publishCharacter: (value) => { database.characters[0] = value },
                publishConversation: (_characterId, conversation, nextCharacter) => {
                    if (nextCharacter) {
                        database.characters[0] = nextCharacter
                        return
                    }
                    const character = database.characters[0]
                    const index = character.chats.findIndex((chat) => chat.id === conversation.id)
                    character.chats[index] = conversation
                    character.chatPage = index
                },
                shouldHydrateFullCharacter: () => false,
                canReleaseConversation: () => true,
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        expect(runtime.revision).toBe(imported.revision)
        expect(await runtime.activateCharacter('char-a')).toBe(true)

        database.characters[0].chats[0].message.push({ role: 'char', data: 'saved before release' })
        runtime.markPersistentDataDirty(64)
        expect(await runtime.activateConversation('chat-b')).toBe(true)
        expect(runtime.getActiveConversationSession()).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-b',
            storeRevision: imported.revision + 1,
        })
        expect(database.characters[0].chats[0].message).toEqual([])
        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first' },
            { role: 'char', data: 'saved before release' },
        ])

        const activeSession = runtime.getActiveConversationSession()!
        activeSession.append({ role: 'user', data: 'later save' })
        await runtime.flushPendingData('conversation-residency-test')
        expect(activeSession.persistedVersion).toBe(1)
        expect(activeSession.storeRevision).toBe(imported.revision + 2)

        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first' },
            { role: 'char', data: 'saved before release' },
        ])
        expect((await store.readConversation('char-a', 'chat-b'))?.value.message).toEqual([
            { role: 'char', data: 'second' },
            { role: 'user', data: 'later save' },
        ])

        database.characters[0].chats[2].name = 'Renamed summary'
        database.characters[0].chats[2].fmIndex = -1
        database.characters[0].chats[2].scriptstate = { source: 'synthetic' }
        runtime.markPersistentDataDirty(16)
        await runtime.flushPendingData('conversation-summary-metadata-test')
        expect(await store.readConversation('char-a', 'chat-c')).toMatchObject({
            value: {
                name: 'Renamed summary',
                fmIndex: 2,
                scriptstate: { source: 'authoritative' },
                note: 'authoritative note',
                localLore: [{ key: 'lore', content: 'authoritative lore' }],
                message: [{ role: 'user', data: 'must survive structural save' }],
            },
        })

        database.characters[0].chats.splice(0, 1)
        database.characters[0].chatPage = 0
        runtime.markPersistentDataDirty(16)
        await runtime.flushPendingData('conversation-structure-test')

        expect((await store.readConversation('char-a', 'chat-c'))?.value).toMatchObject({
            fmIndex: 2,
            scriptstate: { source: 'authoritative' },
            message: [{ role: 'user', data: 'must survive structural save' }],
        })
    })

    it('persists navigation and selected edits without rebuilding inactive conversations', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'conversation-selection-after-navigation',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const readConversation = vi.spyOn(store, 'readConversation')
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first body' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second body' }],
        } as Chat
        let database = {
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [chatA, chatB])],
        } as unknown as Database
        database.characters[0].chatPage = 0
        await store.replaceFromDatabase(database)
        const commit = vi.spyOn(store, 'commit')
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => 'char-a',
                getSelectedConversationId: () =>
                    database.characters[0].chats[database.characters[0].chatPage].id,
                replaceDatabase: (value) => { database = value },
                publishCharacter: (value) => { database.characters[0] = value },
                publishConversation: (_characterId, conversation, nextCharacter) => {
                    if (nextCharacter) {
                        database.characters[0] = nextCharacter
                        return
                    }
                    const character = database.characters[0]
                    const index = character.chats.findIndex((chat) => chat.id === conversation.id)
                    character.chats[index] = conversation
                    character.chatPage = index
                },
                shouldHydrateFullCharacter: () => false,
                canReleaseConversation: () => true,
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        expect(await runtime.activateCharacter('char-a')).toBe(true)
        expect(await runtime.activateConversation('chat-b')).toBe(true)
        readConversation.mockClear()

        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('conversation-selection-test')

        expect(readConversation).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            character: { chaId: 'char-a', chatPage: 1 },
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect((await store.readCharacter('char-a'))?.value.chatPage).toBe(1)
        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first body' },
        ])
        expect((await store.readConversation('char-a', 'chat-b'))?.value.message).toEqual([
            { role: 'char', data: 'second body' },
        ])

        commit.mockClear()
        expect(await runtime.activateConversation('chat-a')).toBe(true)
        readConversation.mockClear()
        runtime.getActiveConversationSession()!.append({
            role: 'char',
            data: 'selected edit',
        })
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('conversation-selection-and-edit-test')

        expect(readConversation).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            character: { chaId: 'char-a', chatPage: 0 },
            conversations: [{
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'chat-a',
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'char', data: 'selected edit' }],
            }],
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect((await store.readCharacter('char-a'))?.value.chatPage).toBe(0)
        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first body' },
            { role: 'char', data: 'selected edit' },
        ])
        expect((await store.readConversation('char-a', 'chat-b'))?.value.message).toEqual([
            { role: 'char', data: 'second body' },
        ])
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
        const previous = {
            ...makeChat('chat-old'),
            message: [{ role: 'user', data: 'must remain resident' }],
        } as Chat
        const resident = makeCharacter('char-a', [previous, makeChat('chat-a')])
        resident.chatPage = 0
        harness.database.characters = [resident]

        const activation = harness.workingSet.activateConversation('chat-a')
        await vi.waitFor(() => expect(harness.store.readConversation).toHaveBeenCalledOnce())
        harness.coordinator.mutationGeneration++
        conversationRead.resolve({ revision: 1, value: makeChat('chat-a') })

        expect(await activation).toBe(false)
        expect(harness.publishedConversations).toEqual([])
        expect(harness.database.characters[0].chats[0]).toBe(previous)
        expect(harness.database.characters[0].chats[0].message).toEqual([
            { role: 'user', data: 'must remain resident' },
        ])
    })

    it('keeps the previous body resident when the selected character changes during hydration', async () => {
        const conversationRead = deferred<{ revision: number; value: Chat } | null>()
        const lease = makeLease({
            characterId: 'char-a',
            readConversation: vi.fn(() => conversationRead.promise),
        })
        const harness = makeHarness(lease)
        harness.setSelectedCharacterId('char-a')
        const previous = {
            ...makeChat('chat-old'),
            message: [{ role: 'user', data: 'must remain resident' }],
        } as Chat
        const resident = makeCharacter('char-a', [previous, makeChat('chat-a')])
        resident.chatPage = 0
        harness.database.characters = [resident]

        const activation = harness.workingSet.activateConversation('chat-a')
        await vi.waitFor(() => expect(lease.readConversation).toHaveBeenCalledOnce())
        harness.setSelectedCharacterId('char-b')
        conversationRead.resolve({ revision: 1, value: makeChat('chat-a') })

        expect(await activation).toBe(false)
        expect(harness.publishedConversations).toEqual([])
        expect(harness.database.characters[0].chats[0]).toBe(previous)
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
            markPersistentDataDirty: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: reopened,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            publishCharacter: (value) => published.push(value),
            publishCharacterSet: vi.fn(),
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

        expect(published[0]).toEqual({
            ...fixtureDatabase.characters[1],
            chats: [
                fixtureDatabase.characters[1].chats[0],
                { ...fixtureDatabase.characters[1].chats[1], message: [] },
            ],
        })
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
                markPersistentDataDirty: vi.fn(),
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
