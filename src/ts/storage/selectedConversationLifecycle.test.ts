import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import { ActiveWorkingSet } from './activeWorkingSet.svelte'
import type { Chat, Database, character } from './database.svelte'
import type {
    ConversationWindowQuery,
    PersistentDataStore,
} from './persistentDataStore'
import {
    PersistentConversationViewportSource,
} from '../conversationViewportSource'
import {
    isMetadataOnlySelectedConversation,
} from './selectedConversationLifecycle'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'

function makeChat(messageCount: number): Chat {
    return {
        id: 'chat-a',
        name: 'Selected',
        note: 'note',
        localLore: [],
        message: Array.from({ length: messageCount }, (_, index) => ({
            role: index % 2 === 0 ? 'user' as const : 'char' as const,
            data: `message-${index}`,
            chatId: `message-${index}`,
        })),
    }
}

function makeCharacter(conversation: Chat): character {
    return {
        type: 'character',
        chaId: 'char-a',
        name: 'Alpha',
        chatPage: 0,
        chats: [conversation],
    } as unknown as character
}

function makeHarness(messageCount = 10_000) {
    const persistedConversation = makeChat(messageCount)
    let resident = makeCharacter(structuredClone(persistedConversation))
    let selectedCharacterId = resident.chaId
    let allowWindowed = true
    let maximumCompatibility = false
    let operationActive = false
    let pendingPersistence = false
    let transitionActive = false
    const storeRevision = 7
    let coordinatorRevision = storeRevision
    const readConversation = vi.fn(async (characterId: string, conversationId: string) => (
        characterId === resident.chaId && conversationId === persistedConversation.id
            ? { revision: storeRevision, value: structuredClone(persistedConversation) }
            : null
    ))
    const readConversationWindow = vi.fn(async (input: ConversationWindowQuery) => {
        if (
            input.characterId !== resident.chaId ||
            input.conversationId !== persistedConversation.id
        ) return null
        const startIndex = Math.min(messageCount, input.startIndex ?? 0)
        const endIndex = Math.min(messageCount, startIndex + input.limit)
        return {
            revision: storeRevision,
            value: {
                characterId: resident.chaId,
                conversationId: persistedConversation.id!,
                startIndex,
                endIndex,
                totalMessages: messageCount,
                messages: structuredClone(
                    persistedConversation.message.slice(startIndex, endIndex),
                ),
            },
        }
    })
    const store = {
        readConversation,
        readConversationWindow,
    } as unknown as PersistentDataStore
    const coordinator = {
        revision: storeRevision,
        mutationGeneration: 0,
        hasPendingPersistenceWork: false,
        initialize: vi.fn(),
        flushPendingData: vi.fn(async () => undefined),
        replacePersistentDatabase: vi.fn(async () => undefined),
        adoptHydratedCharacter: vi.fn(() => true),
        adoptWindowedSelectedConversation: vi.fn(() => true),
        advanceWindowedSelectedConversationRevision: vi.fn(() => true),
        runSelectedConversationTransition: <T>(transition: () => T): T => {
            if (pendingPersistence || transitionActive) {
                throw new Error('selected conversation transition is busy')
            }
            transitionActive = true
            try {
                return transition()
            } finally {
                transitionActive = false
            }
        },
        recordActiveConversationMutation: vi.fn(),
    }
    Object.defineProperty(coordinator, 'hasPendingPersistenceWork', {
        get: () => pendingPersistence,
    })
    Object.defineProperty(coordinator, 'revision', {
        get: () => coordinatorRevision,
    })
    const published = vi.fn((
        _characterId: string,
        conversation: Chat,
        nextCharacter?: character,
    ) => {
        if (nextCharacter) resident = nextCharacter
        else resident.chats[resident.chatPage ?? 0] = conversation
    })
    const workingSet = new ActiveWorkingSet({
        store,
        coordinator,
        getSelectedCharacterId: () => selectedCharacterId,
        getResidentCharacter: (id) => id === resident.chaId ? resident : null,
        publishCharacter: (character) => {
            resident = character as character
            selectedCharacterId = character.chaId
        },
        publishCharacterSet: (character) => {
            resident = character as character
            selectedCharacterId = character.chaId
        },
        publishConversation: published,
        canUseWindowedSelectedConversation: () => allowWindowed,
        isMaximumCompatibilityMode: () => maximumCompatibility,
        isConversationOperationActive: () => operationActive,
        conversationViewportRowBudget: 64,
    })
    workingSet.installCommittedWorkingSet(
        { username: 'Fixture', characters: [resident] } as unknown as Database,
        storeRevision,
    )
    return {
        coordinator,
        persistedConversation,
        published,
        readConversation,
        readConversationWindow,
        workingSet,
        getResident: () => resident,
        setAllowWindowed: (value: boolean) => {
            allowWindowed = value
        },
        setMaximumCompatibility: (value: boolean) => {
            maximumCompatibility = value
        },
        setOperationActive: (value: boolean) => {
            operationActive = value
        },
        setPendingPersistence: (value: boolean) => {
            pendingPersistence = value
        },
        setCoordinatorRevision: (value: number) => {
            coordinatorRevision = value
        },
        setReadConversation: (implementation: typeof readConversation) => {
            readConversation.mockImplementation(implementation)
        },
    }
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

describe('selected conversation lifecycle', () => {
    it('demotes a 10k complete owner to a throwing metadata shell and persistent source', () => {
        const harness = makeHarness()
        const completeSession = harness.workingSet.activeConversationSession!
        const completeSource = harness.workingSet.activeConversationViewportSource!
        const target = harness.workingSet.captureSelectedConversationTarget()!

        expect(harness.workingSet.tryDemoteSelectedConversation(target)).toBe(true)

        const shell = harness.getResident().chats[0]
        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(completeSession.isActive).toBe(false)
        expect((completeSession as unknown as { conversation: Chat | null }).conversation).toBeNull()
        const releasedSource = completeSource as unknown as {
            captureCurrent: (() => unknown) | null
            identityRegistry: {
                registeredObjectOccurrences: Map<unknown, unknown>
            }
        }
        expect(releasedSource.captureCurrent).toBeNull()
        expect(releasedSource.identityRegistry.registeredObjectOccurrences.size).toBe(0)
        expect(() => completeSource.snapshot().totalMessages).not.toThrow()
        expect(completeSource.snapshot().totalMessages).toBe(0)
        expect(isMetadataOnlySelectedConversation(shell)).toBe(true)
        expect(() => shell.message).toThrow('metadata-only')
        expect(Object.keys(shell)).not.toContain('message')
        const traversedKeys: string[] = []
        for (const key in shell) traversedKeys.push(key)
        expect(traversedKeys).not.toContain('message')
        expect(JSON.stringify(shell)).not.toContain('"message"')
        expect(harness.workingSet.activeConversationViewportSource).toBeInstanceOf(
            PersistentConversationViewportSource,
        )
        expect(
            harness.workingSet.activeConversationViewportSource?.snapshot().totalMessages,
        ).toBe(10_000)
        expect(harness.coordinator.adoptWindowedSelectedConversation).toHaveBeenCalledOnce()
        expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({
            kind: 'windowed',
            characterId: 'char-a',
            conversationId: 'chat-a',
            storeRevision: 7,
            totalMessages: 10_000,
        })
    })

    it('publishes the windowed authority before exposing the metadata shell', () => {
        const harness = makeHarness(3)
        harness.published.mockImplementationOnce((
            _characterId: string,
            conversation: Chat,
            nextCharacter?: character,
        ) => {
            expect(isMetadataOnlySelectedConversation(conversation)).toBe(true)
            expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({
                kind: 'windowed',
                totalMessages: 3,
            })
            expect(() => conversation.message).toThrow('metadata-only')
            if (nextCharacter) {
                Object.assign(harness.getResident(), nextCharacter)
            }
        })

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
    })

    it('promotes one exact windowed target once and returns independent idempotent leases', async () => {
        const harness = makeHarness(10_000)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const windowedTarget = harness.workingSet.captureSelectedConversationTarget()!

        const firstPromise = harness.workingSet.acquireCompleteConversation(
            'legacy-script',
            windowedTarget,
        )
        const secondPromise = harness.workingSet.acquireCompleteConversation(
            'legacy-script',
            windowedTarget,
        )
        const [first, second] = await Promise.all([firstPromise, secondPromise])

        expect(harness.coordinator.flushPendingData).toHaveBeenCalledOnce()
        expect(harness.readConversation).toHaveBeenCalledOnce()
        expect(first.session).toBe(second.session)
        expect(first.target).not.toEqual(windowedTarget)
        expect(first.target).toEqual(harness.workingSet.captureSelectedConversationTarget())
        expect(first.session.pinCount('compatibility')).toBe(2)
        expect(harness.workingSet.selectedConversationMode).toBe('complete')
        expect(harness.getResident().chats[0].message).toHaveLength(10_000)
        first.release()
        first.release()
        expect(first.session.pinCount('compatibility')).toBe(1)
        second.release()
        second.release()
        expect(first.session.pinCount('compatibility')).toBe(0)
    })

    it('discards a promotion read when navigation changes and never publishes the full owner', async () => {
        const harness = makeHarness(3)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const read = deferred<Awaited<ReturnType<typeof harness.readConversation>>>()
        harness.setReadConversation(vi.fn(() => read.promise) as typeof harness.readConversation)
        const publishCount = harness.published.mock.calls.length

        const promotion = harness.workingSet.acquireCompleteConversation('stale', target)
        await Promise.resolve()
        harness.workingSet.invalidateNavigation()
        read.resolve({ revision: 7, value: structuredClone(harness.persistedConversation) })

        await expect(promotion).rejects.toThrow('changed during complete promotion')
        expect(harness.published).toHaveBeenCalledTimes(publishCount)
        expect(harness.workingSet.selectedConversationMode).toBeNull()
    })

    it('rejects the previous complete target while another conversation is activating', async () => {
        const harness = makeHarness(3)
        const flush = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(flush.promise)

        const activation = harness.workingSet.activateConversation('chat-b')
        const staleTarget = harness.workingSet.captureSelectedConversationTarget()!

        expect(staleTarget.navigationGeneration).not.toBe(
            harness.workingSet.navigationGenerationToken,
        )
        expect(harness.workingSet.tryDemoteSelectedConversation(staleTarget)).toBe(false)
        await expect(
            harness.workingSet.acquireCompleteConversation('stale-navigation', staleTarget),
        ).rejects.toThrow('changed during complete promotion')
        flush.resolve()
        await expect(activation).rejects.toThrow('was not found')
    })

    it('rejects the previous windowed target after navigation starts from its promotion', async () => {
        const harness = makeHarness(3)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const flush = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(flush.promise)

        const activation = harness.workingSet.activateConversation('chat-b')
        const staleTarget = harness.workingSet.captureSelectedConversationTarget()!

        expect(staleTarget.navigationGeneration).toBe(
            harness.workingSet.navigationGenerationToken,
        )
        flush.resolve()
        await expect(activation).rejects.toThrow('was not found')
        expect(staleTarget.navigationGeneration).not.toBe(
            harness.workingSet.navigationGenerationToken,
        )
        await expect(
            harness.workingSet.acquireCompleteConversation('stale-navigation', staleTarget),
        ).rejects.toThrow('changed during complete promotion')
    })

    it('keeps the windowed owner when the promotion read returns another revision', async () => {
        const harness = makeHarness(3)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        harness.setReadConversation(vi.fn(async () => ({
            revision: 8,
            value: structuredClone(harness.persistedConversation),
        })) as typeof harness.readConversation)

        await expect(
            harness.workingSet.acquireCompleteConversation('stale-revision', target),
        ).rejects.toThrow('changed during complete promotion')

        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(() => harness.getResident().chats[0].message).toThrow('metadata-only')
        expect(harness.workingSet.captureSelectedConversationAuthority()).not.toBeNull()
    })

    it('keeps the windowed owner when complete adoption fails after publication', async () => {
        const harness = makeHarness(3)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        harness.coordinator.adoptHydratedCharacter.mockReturnValueOnce(false)

        await expect(
            harness.workingSet.acquireCompleteConversation('failed-adoption', target),
        ).rejects.toThrow('complete selected conversation was not adopted')

        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(() => harness.getResident().chats[0].message).toThrow('metadata-only')
        expect(harness.workingSet.captureSelectedConversationAuthority()).not.toBeNull()
    })

    it('clears promotion authority when publication rollback also fails', async () => {
        const harness = makeHarness(3)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        harness.coordinator.adoptHydratedCharacter.mockReturnValueOnce(false)
        harness.published
            .mockImplementationOnce((_characterId, _conversation, nextCharacter) => {
                if (nextCharacter) Object.assign(harness.getResident(), nextCharacter)
            })
            .mockImplementationOnce(() => {
                throw new Error('rollback publication failed')
            })

        await expect(
            harness.workingSet.acquireCompleteConversation('failed-rollback', target),
        ).rejects.toThrow('complete selected conversation was not adopted')

        expect(harness.workingSet.selectedConversationMode).toBeNull()
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(harness.workingSet.captureSelectedConversationAuthority()).toBeNull()
    })

    it.each([
        ['scalable policy', (harness: ReturnType<typeof makeHarness>) =>
            harness.setAllowWindowed(false)],
        ['maximum compatibility', (harness: ReturnType<typeof makeHarness>) =>
            harness.setMaximumCompatibility(true)],
        ['active operation', (harness: ReturnType<typeof makeHarness>) =>
            harness.setOperationActive(true)],
        ['pending persistence', (harness: ReturnType<typeof makeHarness>) =>
            harness.setPendingPersistence(true)],
        ['store revision mismatch', (harness: ReturnType<typeof makeHarness>) =>
            harness.setCoordinatorRevision(8)],
        ['streaming metadata', (harness: ReturnType<typeof makeHarness>) => {
            harness.getResident().chats[0].isStreaming = true
        }],
    ])('keeps complete ownership when the %s gate is closed', (_name, closeGate) => {
        const harness = makeHarness(3)
        const session = harness.workingSet.activeConversationSession!
        closeGate(harness)

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(false)
        expect(harness.workingSet.selectedConversationMode).toBe('complete')
        expect(harness.workingSet.activeConversationSession).toBe(session)
        expect(session.isActive).toBe(true)
        expect(harness.getResident().chats[0].message).toHaveLength(3)
        expect(harness.coordinator.adoptWindowedSelectedConversation).not.toHaveBeenCalled()
    })

    it('keeps complete ownership while the session is dirty or in a transaction', () => {
        const dirtyHarness = makeHarness(3)
        dirtyHarness.workingSet.activeConversationSession!.append({
            role: 'user',
            data: 'dirty',
        })
        expect(dirtyHarness.workingSet.tryDemoteSelectedConversation()).toBe(false)

        const transactionHarness = makeHarness(3)
        transactionHarness.workingSet.activeConversationSession!.transaction(() => {
            expect(transactionHarness.workingSet.tryDemoteSelectedConversation()).toBe(false)
        })
        expect(transactionHarness.workingSet.selectedConversationMode).toBe('complete')
    })

    it.each([
        'viewport',
        'editor',
        'playing-media',
        'dirty',
        'pending-save',
        'streaming',
        'transaction',
        'prompt',
        'compatibility',
    ] as const)('keeps complete ownership while a %s pin is active', (reason) => {
        const harness = makeHarness(3)
        const session = harness.workingSet.activeConversationSession!
        const pin = session.acquirePin(reason)

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(false)
        expect(session.isActive).toBe(true)
        pin.release()
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
    })

    it('rolls back publication when windowed baseline adoption is rejected', () => {
        const harness = makeHarness(3)
        const session = harness.workingSet.activeConversationSession!
        harness.coordinator.adoptWindowedSelectedConversation.mockReturnValueOnce(false)

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(false)

        expect(harness.workingSet.selectedConversationMode).toBe('complete')
        expect(harness.workingSet.activeConversationSession).toBe(session)
        expect(session.isActive).toBe(true)
        expect(harness.getResident().chats[0].message).toHaveLength(3)
        expect(harness.workingSet.captureSelectedConversationAuthority()).toBeNull()
    })

    it('clears demotion ownership when publication rollback also fails', () => {
        const harness = makeHarness(3)
        harness.coordinator.adoptWindowedSelectedConversation.mockReturnValueOnce(false)
        harness.published
            .mockImplementationOnce((_characterId, _conversation, nextCharacter) => {
                if (nextCharacter) Object.assign(harness.getResident(), nextCharacter)
            })
            .mockImplementationOnce(() => {
                throw new Error('rollback publication failed')
            })

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(false)

        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(harness.workingSet.captureSelectedConversationAuthority()).not.toBeNull()
    })

    it('rolls back when the state adapter rejects metadata-shell publication', () => {
        const harness = makeHarness(3)
        const session = harness.workingSet.activeConversationSession!
        harness.published.mockImplementationOnce(() => {
            throw new Error('publish failed')
        })

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(false)

        expect(harness.workingSet.selectedConversationMode).toBe('complete')
        expect(harness.workingSet.activeConversationSession).toBe(session)
        expect(session.isActive).toBe(true)
        expect(harness.getResident().chats[0].message).toHaveLength(3)
    })

    it('keeps complete ownership when metadata-shell preparation fails', () => {
        const harness = makeHarness(3)
        const session = harness.workingSet.activeConversationSession!
        Object.defineProperty(harness.getResident().chats[0], 'note', {
            enumerable: true,
            get() {
                throw new Error('metadata getter failed')
            },
        })

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(false)

        expect(harness.workingSet.selectedConversationMode).toBe('complete')
        expect(harness.workingSet.activeConversationSession).toBe(session)
        expect(session.isActive).toBe(true)
        expect(harness.getResident().chats[0].message).toHaveLength(3)
        expect(harness.coordinator.adoptWindowedSelectedConversation).not.toHaveBeenCalled()
    })

    it('advances windowed source and persistence authority together after a storage-only write', () => {
        const harness = makeHarness(10_000)
        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        const previousSource = harness.workingSet.activeConversationViewportSource!
        harness.setCoordinatorRevision(8)

        harness.workingSet.advanceStoreRevision(8)

        expect(previousSource.snapshot().totalMessages).toBe(0)
        expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({
            storeRevision: 8,
            totalMessages: 10_000,
        })
        expect(harness.workingSet.activeConversationViewportSource).not.toBe(previousSource)
        expect(
            harness.workingSet.activeConversationViewportSource?.snapshot().totalMessages,
        ).toBe(10_000)
        expect(
            harness.coordinator.advanceWindowedSelectedConversationRevision,
        ).toHaveBeenCalledWith(8, expect.objectContaining({ storeRevision: 8 }))
    })

    it('notifies active source subscribers once per state or revision transition', () => {
        const harness = makeHarness(10_000)
        const sources: Array<unknown> = []
        const listener = vi.fn((source) => sources.push(source))
        const unsubscribe = harness.workingSet.subscribeActiveConversationViewportSource(listener)

        expect(harness.workingSet.tryDemoteSelectedConversation()).toBe(true)
        expect(listener).toHaveBeenCalledTimes(1)
        expect(sources[0]).toBe(harness.workingSet.activeConversationViewportSource)

        harness.setCoordinatorRevision(8)
        harness.workingSet.advanceStoreRevision(8)
        expect(listener).toHaveBeenCalledTimes(2)
        expect(sources[1]).toBe(harness.workingSet.activeConversationViewportSource)

        unsubscribe()
        unsubscribe()
        harness.workingSet.invalidateActiveConversationSession()
        expect(listener).toHaveBeenCalledTimes(2)
    })

    it('isolates active source subscriber failures and safely unsubscribes', () => {
        const harness = makeHarness(3)
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const retained = vi.fn()
        const unsubscribeThrowing = harness.workingSet.subscribeActiveConversationViewportSource(
            () => { throw new Error('subscriber failed') },
        )
        const unsubscribeRetained = harness.workingSet.subscribeActiveConversationViewportSource(
            retained,
        )

        harness.workingSet.invalidateActiveConversationSession()

        expect(retained).toHaveBeenCalledOnce()
        expect(consoleError).toHaveBeenCalledOnce()
        expect(() => unsubscribeThrowing()).not.toThrow()
        expect(() => unsubscribeThrowing()).not.toThrow()
        expect(() => unsubscribeRetained()).not.toThrow()
        consoleError.mockRestore()
    })

    it('wires lifecycle authority, source, storage revision and promotion through the runtime', async () => {
        const database = {
            username: 'Runtime fixture',
            characters: [makeCharacter(makeChat(10_000))],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        const store = new IndexedDbPersistentDataStore(
            `selected-lifecycle-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const revision = await store.replaceFromDatabase(database)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => workingCopy.characters[0]?.chats[0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)

        expect(runtime.getSelectedConversationMode()).toBe('complete')
        const runtimeSourceChanges = vi.fn()
        const unsubscribeSource = runtime.subscribeActiveConversationViewportSource(
            runtimeSourceChanges,
        )
        expect(runtime.tryDemoteSelectedConversation()).toBe(true)
        expect(runtimeSourceChanges).toHaveBeenCalledOnce()
        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        expect(runtime.getActiveConversationViewportSource()).toBeInstanceOf(
            PersistentConversationViewportSource,
        )
        expect(() => workingCopy.characters[0].chats[0].message).toThrow('metadata-only')

        workingCopy.username = 'Windowed root edit'
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('windowed-root-edit')
        expect(runtime.captureSelectedConversationAuthority()).toMatchObject({
            storeRevision: revision.revision + 1,
            totalMessages: 10_000,
        })
        expect(runtime.getSelectedConversationMode()).toBe('windowed')

        await runtime.runStorageOnlyMutation(async (expectedRevision) => {
            const root = await store.readRoot()
            expect(root.revision).toBe(expectedRevision)
            return (await store.commit({
                expectedRevision,
                root: root.value,
            })).revision
        })
        expect(runtime.captureSelectedConversationAuthority()).toMatchObject({
            storeRevision: revision.revision + 2,
            totalMessages: 10_000,
        })
        expect(runtimeSourceChanges).toHaveBeenCalledTimes(3)

        const target = runtime.captureSelectedConversationTarget()!
        const lease = await runtime.acquireCompleteConversation('runtime-test', target)
        expect(lease.target).toEqual(runtime.captureSelectedConversationTarget())
        expect(runtime.getSelectedConversationMode()).toBe('complete')
        expect(workingCopy.characters[0].chats[0].message).toHaveLength(10_000)
        expect(runtimeSourceChanges).toHaveBeenCalledTimes(4)
        unsubscribeSource()
        lease.release()
    })

    it('advances complete ownership after an ordinary root commit and remains demotable', async () => {
        const database = {
            username: 'Complete revision fixture',
            characters: [makeCharacter(makeChat(3))],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        const store = new IndexedDbPersistentDataStore(
            `selected-complete-revision-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const initial = await store.replaceFromDatabase(database)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => workingCopy.characters[0]?.chats[0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        workingCopy.username = 'Committed root edit'
        runtime.markPersistentDataDirty(1)

        await runtime.flushPendingData('complete-root-edit')

        expect(runtime.captureSelectedConversationTarget()).toMatchObject({
            storeRevision: initial.revision + 1,
        })
        expect(runtime.tryDemoteSelectedConversation()).toBe(true)
    })

    it('keeps a windowed selected conversation authoritative across a root module append', async () => {
        const database = {
            username: 'Windowed module append fixture',
            modules: [],
            characters: [makeCharacter(makeChat(10_000))],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        const store = new IndexedDbPersistentDataStore(
            `selected-windowed-module-append-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const initial = await store.replaceFromDatabase(database)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => workingCopy.characters[0]?.chats[0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishRootWorkingSet: (root) => {
                Object.assign(workingCopy, root)
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        expect(runtime.tryDemoteSelectedConversation()).toBe(true)

        await runtime.appendPersistentRootModule('windowed-module-append', {
            module: {
                id: 'module-a',
                name: 'Imported module',
                description: '',
            },
            assetAliases: [],
            ownerHead: {
                present: false,
                manifestHash: null,
                entryCount: 0,
            },
        })

        const expectedRevision = initial.revision + 1
        expect(runtime.revision).toBe(expectedRevision)
        expect(runtime.getActiveConversationViewportSource()?.snapshot()).toMatchObject({
            storeRevision: expectedRevision,
            totalMessages: 10_000,
        })
        expect(runtime.captureSelectedConversationAuthority()).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
            storeRevision: expectedRevision,
            totalMessages: 10_000,
        })
        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        expect(workingCopy.modules).toEqual([
            expect.objectContaining({ id: 'module-a', name: 'Imported module' }),
        ])
        expect((await store.readRoot()).value.modules).toEqual(workingCopy.modules)
    })

    it('fails persistence closed after both demotion publication and rollback diverge', async () => {
        const database = {
            username: 'Demotion rollback fixture',
            characters: [makeCharacter(makeChat(3))],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        let publicationCount = 0
        const store = new IndexedDbPersistentDataStore(
            `selected-demotion-rollback-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const initial = await store.replaceFromDatabase(database)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => workingCopy.characters[0]?.chats[0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                publicationCount++
                if (publicationCount === 1 && nextCharacter) {
                    workingCopy.characters[0] = {
                        ...nextCharacter,
                        name: 'Diverged publication',
                    } as character
                    return
                }
                throw new Error('rollback publication failed')
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)

        expect(runtime.tryDemoteSelectedConversation()).toBe(false)
        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        expect(runtime.captureSelectedConversationAuthority()).not.toBeNull()
        workingCopy.username = 'Must not commit'
        runtime.markPersistentDataDirty(1)

        await expect(runtime.flushPendingData('failed-demotion-rollback')).rejects.toThrow(
            /compatibility/i,
        )
        expect((await store.readRoot()).revision).toBe(initial.revision)
        expect((await store.readConversation('char-a', 'chat-a')).value.message).toHaveLength(3)
    })

    it('promotes before navigating from a windowed conversation to another conversation', async () => {
        const chatA = makeChat(3)
        const chatB = { ...makeChat(2), id: 'chat-b', name: 'Second' }
        const selected = makeCharacter(chatA)
        selected.chats.push(chatB)
        const database = {
            username: 'Conversation navigation fixture',
            characters: [selected],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        const store = new IndexedDbPersistentDataStore(
            `selected-conversation-navigation-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase(database)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () =>
                workingCopy.characters[0]?.chats[workingCopy.characters[0].chatPage ?? 0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        expect(runtime.tryDemoteSelectedConversation()).toBe(true)
        const previousSource = runtime.getActiveConversationViewportSource()!

        await expect(runtime.activateConversation('chat-b')).resolves.toBe(true)

        expect(previousSource.snapshot().totalMessages).toBe(0)
        expect(runtime.getSelectedConversationMode()).toBe('complete')
        expect(runtime.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-b',
        })
    })

    it('promotes before navigating from a windowed conversation to another character', async () => {
        const characterA = makeCharacter(makeChat(3))
        const characterB = {
            ...makeCharacter({ ...makeChat(2), id: 'chat-b' }),
            chaId: 'char-b',
            name: 'Beta',
        }
        const database = {
            username: 'Character navigation fixture',
            characters: [characterA, characterB],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        let selectedCharacterId = 'char-a'
        const store = new IndexedDbPersistentDataStore(
            `selected-character-navigation-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase(database)
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () =>
                workingCopy.characters.find((entry) => entry.chaId === selectedCharacterId) ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => selectedCharacterId,
            getSelectedConversationId: () => {
                const selectedCharacter = workingCopy.characters.find(
                    (entry) => entry.chaId === selectedCharacterId,
                )
                return selectedCharacter?.chats[selectedCharacter.chatPage ?? 0]?.id
            },
            replaceDatabase: (next) => {
                workingCopy = next
            },
            publishCharacter: (next) => {
                const index = workingCopy.characters.findIndex(
                    (entry) => entry.chaId === next.chaId,
                )
                workingCopy.characters[index] = next
                selectedCharacterId = next.chaId
            },
            publishConversation: (characterId, conversation, nextCharacter) => {
                const index = workingCopy.characters.findIndex(
                    (entry) => entry.chaId === characterId,
                )
                if (nextCharacter) workingCopy.characters[index] = nextCharacter
                else workingCopy.characters[index].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        const runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        expect(runtime.tryDemoteSelectedConversation()).toBe(true)
        const previousSource = runtime.getActiveConversationViewportSource()!

        await expect(runtime.activateCharacter('char-b')).resolves.toBe(true)

        expect(previousSource.snapshot().totalMessages).toBe(0)
        expect(runtime.getSelectedConversationMode()).toBe('complete')
        expect(runtime.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'char-b',
            conversationId: 'chat-b',
        })
    })

    it('invalidates windowed ownership before installing maximum compatibility data', async () => {
        const database = {
            username: 'Maximum compatibility fixture',
            characters: [makeCharacter(makeChat(3))],
        } as unknown as Database
        let workingCopy = structuredClone(database)
        const store = new IndexedDbPersistentDataStore(
            `selected-maximum-compatibility-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase(database)
        let runtime!: ReturnType<typeof createPersistentDataRuntime>
        const installCompleteDatabase = vi.fn((next: Database) => {
            expect(runtime.getSelectedConversationMode()).toBeNull()
            expect(runtime.captureSelectedConversationAuthority()).toBeNull()
            workingCopy = next
        })
        const state: PersistentDataRuntimeStateAdapter = {
            captureRoot: () => capturePersistentRoot(workingCopy),
            captureSelectedCharacter: () => workingCopy.characters[0] ?? null,
            captureCharacter: (id) =>
                workingCopy.characters.find((character) => character.chaId === id) ?? null,
            getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
            getSelectedConversationId: () => workingCopy.characters[0]?.chats[0]?.id,
            replaceDatabase: (next) => {
                workingCopy = next
            },
            installCompleteDatabase,
            restoreSelection: vi.fn(),
            publishCharacter: (next) => {
                workingCopy.characters[0] = next
            },
            publishConversation: (_characterId, conversation, nextCharacter) => {
                if (nextCharacter) workingCopy.characters[0] = nextCharacter
                else workingCopy.characters[0].chats[0] = conversation
            },
            canUseWindowedSelectedConversation: () => true,
            isMaximumCompatibilityMode: () => false,
            isConversationOperationActive: () => false,
        }
        runtime = createPersistentDataRuntime({
            store,
            state,
            prepareDatabase: async (candidate) => candidate,
        })
        await runtime.initializeActiveWorkingSet(workingCopy)
        expect(runtime.tryDemoteSelectedConversation()).toBe(true)

        await runtime.materializeMaximumCompatibilityWorkingSet()

        expect(installCompleteDatabase).toHaveBeenCalledOnce()
        expect(runtime.getSelectedConversationMode()).toBeNull()
        expect(runtime.captureSelectedConversationAuthority()).toBeNull()
        expect(workingCopy.characters[0].chats[0].message).toHaveLength(3)
        workingCopy.username = 'Safe complete write'
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('after-maximum-compatibility')
        expect((await store.readConversation('char-a', 'chat-a')).value.message).toHaveLength(3)
    })
})
