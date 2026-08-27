import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import {
    SelectedConversationPromotionStaleError,
    type CompleteConversationLease,
    type SelectedConversationTarget,
} from './storage/activeWorkingSet.svelte'
import {
    createSelectedConversationOperations,
    type SelectedConversationOperationsDependencies,
} from './selectedConversationOperations'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function makeSelection(
    characterId: string,
    conversationId: string,
    navigationGeneration = 1,
    storeRevision = 7,
): SelectedConversationTarget {
    return {
        characterId,
        conversationId,
        navigationGeneration,
        storeRevision,
    } as SelectedConversationTarget
}

function makeCurrent(
    characterId = 'character-a',
    conversationId = 'conversation-a',
    messages: Message[] = [
        { role: 'user', data: 'zero' },
        { role: 'char', data: 'one' },
        { role: 'user', data: 'two' },
    ],
) {
    const conversation = {
        id: conversationId,
        name: 'Conversation',
        note: '',
        localLore: [],
        message: messages,
    } as Chat
    const character = {
        type: 'character',
        chaId: characterId,
        chatPage: 0,
        chats: [conversation],
    } as Database['characters'][number]
    const session = new ActiveConversationSession({
        characterId,
        conversationId,
        conversation,
        storeRevision: 7,
    })
    return { character, conversation, session }
}

function makeLease(
    current: ReturnType<typeof makeCurrent>,
    target = makeSelection(current.character.chaId, current.conversation.id),
) {
    const release = vi.fn()
    const lease: CompleteConversationLease = {
        reason: 'test',
        session: current.session,
        target,
        release,
    }
    return { lease, release }
}

function makeHarness(options: {
    current?: ReturnType<typeof makeCurrent>
    selection?: SelectedConversationTarget | null
    onAcquire?: (
        reason: string,
        target: SelectedConversationTarget,
    ) => CompleteConversationLease | Promise<CompleteConversationLease>
} = {}) {
    let current = options.current ?? makeCurrent()
    let selection = options.selection === undefined
        ? makeSelection(current.character.chaId, current.conversation.id)
        : options.selection
    let session: ActiveConversationSession | null = current.session
    const defaultLease = makeLease(current, selection ?? undefined)
    const acquireCompleteConversation = vi.fn(async (
        reason: string,
        target: SelectedConversationTarget,
    ) => options.onAcquire?.(reason, target) ?? defaultLease.lease)
    const dependencies: SelectedConversationOperationsDependencies = {
        captureSelectedConversationTarget: () => selection,
        acquireCompleteConversation,
        captureCurrent: () => ({
            character: current.character,
            conversation: current.conversation,
        }),
        getCurrentSession: () => session,
    }
    return {
        operations: createSelectedConversationOperations(dependencies),
        dependencies,
        acquireCompleteConversation,
        defaultLease,
        get current() {
            return current
        },
        setCurrent(next: ReturnType<typeof makeCurrent>) {
            current = next
            session = next.session
        },
        setSelection(next: SelectedConversationTarget | null) {
            selection = next
        },
        setSession(next: ActiveConversationSession | null) {
            session = next
        },
    }
}

describe('selected conversation complete-operation gateway', () => {
    it('runs an operation against an already-complete exact session and releases once', async () => {
        const harness = makeHarness()
        const operation = vi.fn(({ character, conversation, session, selection }) => {
            expect(character).toBe(harness.current.character)
            expect(conversation).toBe(harness.current.conversation)
            expect(session).toBe(harness.current.session)
            expect(selection).toEqual(expect.objectContaining({
                characterId: 'character-a',
                conversationId: 'conversation-a',
            }))
            return 'done'
        })

        await expect(
            harness.operations.withCompleteSelectedConversation('already-complete', operation),
        ).resolves.toBe('done')
        expect(operation).toHaveBeenCalledOnce()
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('uses only post-promotion character and conversation identities', async () => {
        const windowed = makeCurrent('character-a', 'conversation-a', [])
        const complete = makeCurrent()
        const selection = makeSelection('character-a', 'conversation-a')
        let promoted = false
        let current = windowed
        let session: ActiveConversationSession | null = null
        const lease = makeLease(complete, selection)
        const dependencies: SelectedConversationOperationsDependencies = {
            captureSelectedConversationTarget: () => selection,
            captureCurrent: () => {
                expect(promoted).toBe(true)
                return { character: current.character, conversation: current.conversation }
            },
            getCurrentSession: () => session,
            acquireCompleteConversation: async () => {
                promoted = true
                current = complete
                session = complete.session
                return lease.lease
            },
        }
        const operations = createSelectedConversationOperations(dependencies)

        const captured = await operations.withCompleteSelectedConversation(
            'promote-windowed',
            (target) => target,
        )

        expect(captured?.character).toBe(complete.character)
        expect(captured?.conversation).toBe(complete.conversation)
        expect(captured?.character).not.toBe(windowed.character)
        expect(captured?.conversation).not.toBe(windowed.conversation)
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('holds the complete lease for the full async operation duration', async () => {
        const harness = makeHarness()
        const operationStarted = deferred<void>()
        const operationResult = deferred<string>()

        const pending = harness.operations.withCompleteSelectedConversation(
            'async-operation',
            async () => {
                operationStarted.resolve()
                return operationResult.promise
            },
        )
        await operationStarted.promise

        expect(harness.defaultLease.release).not.toHaveBeenCalled()
        operationResult.resolve('finished')
        await expect(pending).resolves.toBe('finished')
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('releases exactly once when an operation throws', async () => {
        const harness = makeHarness()
        const expected = new Error('operation failed')

        await expect(harness.operations.withCompleteSelectedConversation(
            'throwing-operation',
            () => {
                throw expected
            },
        )).rejects.toBe(expected)
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('releases and throws a typed stale error after navigation during promotion', async () => {
        const current = makeCurrent()
        const initial = makeSelection('character-a', 'conversation-a', 1)
        const lease = makeLease(current, initial)
        const harness = makeHarness({
            current,
            selection: initial,
            onAcquire: async () => {
                harness.setSelection(makeSelection('character-b', 'conversation-b', 2))
                return lease.lease
            },
        })
        const operation = vi.fn(() => 'must not run')

        await expect(harness.operations.withCompleteSelectedConversation(
            'stale-navigation',
            operation,
        )).rejects.toBeInstanceOf(SelectedConversationPromotionStaleError)
        expect(operation).not.toHaveBeenCalled()
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('returns null without promotion when there is no selected conversation', async () => {
        const harness = makeHarness({ selection: null })
        const operation = vi.fn()

        await expect(harness.operations.withCompleteSelectedConversation(
            'no-selection',
            operation,
        )).resolves.toBeNull()
        await expect(harness.operations.acquireCompleteMessageTarget(
            0,
            'no-selection-message',
        )).resolves.toBeNull()
        expect(operation).not.toHaveBeenCalled()
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
    })

    it('releases and returns null for an out-of-range absolute message index', async () => {
        const harness = makeHarness()

        await expect(harness.operations.acquireCompleteMessageTarget(
            99,
            'out-of-range',
        )).resolves.toBeNull()
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('returns null for an invalid absolute index without acquiring a lease', async () => {
        const harness = makeHarness()

        await expect(harness.operations.acquireCompleteMessageTarget(
            -1,
            'invalid-index',
        )).resolves.toBeNull()
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(harness.defaultLease.release).not.toHaveBeenCalled()
    })

    it.each(['missing', 'mismatch'] as const)(
        'rejects a post-promotion %s session authority and releases',
        async (mode) => {
            const harness = makeHarness()
            harness.setSession(
                mode === 'missing'
                    ? null
                    : makeCurrent('character-a', 'conversation-a').session,
            )

            await expect(harness.operations.withCompleteSelectedConversation(
                'session-mismatch',
                () => 'must not run',
            )).rejects.toBeInstanceOf(SelectedConversationPromotionStaleError)
            expect(harness.defaultLease.release).toHaveBeenCalledOnce()
        },
    )

    it('returns an exact session message target and an idempotent release', async () => {
        const harness = makeHarness()

        const acquired = await harness.operations.acquireCompleteMessageTarget(
            1,
            'message-target',
        )

        expect(acquired?.target).toMatchObject({
            kind: 'session',
            absoluteIndex: 1,
            character: harness.current.character,
            conversation: harness.current.conversation,
            session: harness.current.session,
            message: { role: 'char', data: 'one' },
            locator: { absoluteIndex: 1 },
        })
        expect(acquired?.target.message).not.toBe(harness.current.conversation.message[1])
        expect(harness.defaultLease.release).not.toHaveBeenCalled()
        acquired?.release()
        acquired?.release()
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })
})
