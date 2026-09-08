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
import type {
    ConversationViewportKey,
    ConversationViewportSnapshot,
    ConversationViewportSource,
} from './conversationViewportSource'

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
    let viewportRow = {
        key: 'persistent-source|3|1' as ConversationViewportKey,
        absoluteIndex: 1,
        message: { role: 'char', data: 'one' } as Message,
        sourceVersion: 3,
    }
    let viewportSnapshot: ConversationViewportSnapshot = {
        sourceToken: 'persistent-source',
        version: 3,
        storeRevision: 7,
        totalMessages: 3,
        keyAt: (absoluteIndex: number) => absoluteIndex === 1 ? viewportRow.key : undefined,
        indexOfKey: (key: ConversationViewportKey) => key === viewportRow.key ? 1 : -1,
        rowAt: (absoluteIndex: number) => absoluteIndex === 1 ? viewportRow : undefined,
    }
    const viewportSource = {
        snapshot: () => viewportSnapshot,
    } as unknown as ConversationViewportSource
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
        getCurrentViewportSource: () => viewportSource,
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
        setViewportRow(next: typeof viewportRow) {
            viewportRow = next
        },
        setViewportSnapshot(next: typeof viewportSnapshot) {
            viewportSnapshot = next
        },
    }
}

describe('selected conversation complete-operation gateway', () => {
    it('runs an operation against an already-complete exact session and releases once', async () => {
        const harness = makeHarness()
        const operation = vi.fn((context) => {
            const { character, conversation, session, selection } = context.requireCurrent()
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
            getCurrentViewportSource: () => null,
            acquireCompleteConversation: async () => {
                promoted = true
                current = complete
                session = complete.session
                return lease.lease
            },
        }
        const operations = createSelectedConversationOperations(dependencies)
        let capturedCharacter: Database['characters'][number] | null = null
        let capturedConversation: Chat | null = null

        const result = await operations.withCompleteSelectedConversation(
            'promote-windowed',
            (context) => {
                const currentAuthority = context.requireCurrent()
                capturedCharacter = currentAuthority.character
                capturedConversation = currentAuthority.conversation
                return 'done'
            },
        )

        expect(result).toBe('done')
        expect(capturedCharacter).toBe(complete.character)
        expect(capturedConversation).toBe(complete.conversation)
        expect(capturedCharacter).not.toBe(windowed.character)
        expect(capturedConversation).not.toBe(windowed.conversation)
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

    it('promotes an edit intent against its original selected conversation target', async () => {
        const original = makeSelection('character-a', 'conversation-a', 1, 7)
        const harness = makeHarness({ selection: original })
        const intent = harness.operations.captureMessageEditIntent({
            absoluteIndex: 1,
            sourceToken: 'persistent-source',
            sourceVersion: 3,
            rowKey: 'persistent-source|3|1' as ConversationViewportKey,
            message: { role: 'char', data: 'one' },
        })
        expect(intent).not.toBeNull()

        harness.setSelection(makeSelection('character-a', 'conversation-a', 1, 7))
        const acquired = await harness.operations.acquireCompleteMessageTargetForIntent(
            intent!,
            'save-windowed-edit',
        )

        expect(harness.acquireCompleteConversation).toHaveBeenCalledWith(
            'save-windowed-edit',
            original,
        )
        expect(acquired?.target.message.data).toBe('one')
        expect(harness.defaultLease.release).not.toHaveBeenCalled()
        acquired?.release()
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('rejects an edit intent when the promoted message no longer matches its evidence', async () => {
        const harness = makeHarness()
        harness.setViewportRow({
            key: 'persistent-source|3|1' as ConversationViewportKey,
            absoluteIndex: 1,
            message: { role: 'char', data: 'expected' },
            sourceVersion: 3,
        })
        const intent = harness.operations.captureMessageEditIntent({
            absoluteIndex: 1,
            sourceToken: 'persistent-source',
            sourceVersion: 3,
            rowKey: 'persistent-source|3|1' as ConversationViewportKey,
            message: { role: 'char', data: 'expected' },
        })

        await expect(harness.operations.acquireCompleteMessageTargetForIntent(
            intent!,
            'stale-windowed-edit',
        )).resolves.toBeNull()
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('does not retarget an edit intent after the selected conversation changes', async () => {
        const original = makeSelection('character-a', 'conversation-a', 1, 7)
        const harness = makeHarness({ selection: original })
        const intent = harness.operations.captureMessageEditIntent({
            absoluteIndex: 1,
            sourceToken: 'persistent-source',
            sourceVersion: 3,
            rowKey: 'persistent-source|3|1' as ConversationViewportKey,
            message: { role: 'char', data: 'one' },
        })
        harness.setSelection(makeSelection('character-b', 'conversation-b', 2, 8))

        await expect(harness.operations.acquireCompleteMessageTargetForIntent(
            intent!,
            'stale-windowed-edit',
        )).rejects.toBeInstanceOf(SelectedConversationPromotionStaleError)
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
    })

    it('captures immutable edit evidence and compares it structurally after promotion', async () => {
        const originalMessage: Message = {
            data: 'one',
            role: 'char',
        }
        const current = makeCurrent('character-a', 'conversation-a', [
            { role: 'user', data: 'zero' },
            { role: 'char', data: 'one' },
        ])
        const harness = makeHarness({ current })
        harness.setViewportRow({
            key: 'persistent-source|3|1' as ConversationViewportKey,
            absoluteIndex: 1,
            message: originalMessage,
            sourceVersion: 3,
        })
        const intent = harness.operations.captureMessageEditIntent({
            absoluteIndex: 1,
            sourceToken: 'persistent-source',
            sourceVersion: 3,
            rowKey: 'persistent-source|3|1' as ConversationViewportKey,
            message: originalMessage,
        })
        expect(intent).not.toBeNull()
        originalMessage.data = 'mutated after capture'

        const acquired = await harness.operations.acquireCompleteMessageTargetForIntent(
            intent!,
            'structural-evidence',
        )

        expect(acquired).not.toBeNull()
        acquired?.release()
    })

    it.each([
        ['source token', { sourceToken: 'other-source' }],
        ['source version', { sourceVersion: 4 }],
        ['row key', { key: 'other-key' as ConversationViewportKey }],
        ['absolute index', { absoluteIndex: 2 }],
        ['message evidence', { message: { role: 'char', data: 'other' } as Message }],
    ] as const)('rejects edit intent capture when the current viewport %s differs', (_label, change) => {
        const harness = makeHarness()
        const baselineRow = {
            key: 'persistent-source|3|1' as ConversationViewportKey,
            absoluteIndex: 1,
            message: { role: 'char', data: 'one' } as Message,
            sourceVersion: 3,
        }
        const row = { ...baselineRow, ...change }
        harness.setViewportRow(row)
        harness.setViewportSnapshot({
            sourceToken: 'sourceToken' in change ? change.sourceToken : 'persistent-source',
            version: 3,
            storeRevision: 7,
            totalMessages: 3,
            keyAt: (absoluteIndex) => absoluteIndex === row.absoluteIndex ? row.key : undefined,
            indexOfKey: (key) => key === row.key ? row.absoluteIndex : -1,
            rowAt: (absoluteIndex) => absoluteIndex === row.absoluteIndex ? row : undefined,
        })

        expect(harness.operations.captureMessageEditIntent({
            absoluteIndex: 1,
            sourceToken: 'persistent-source',
            sourceVersion: 3,
            rowKey: 'persistent-source|3|1' as ConversationViewportKey,
            message: { role: 'char', data: 'one' },
        })).toBeNull()
        expect(harness.acquireCompleteConversation).not.toHaveBeenCalled()
    })

    it('invokes the operation before a queued post-acquire navigation invalidates authority', async () => {
        const harness = makeHarness()
        const operation = vi.fn((context) => {
            const current = context.requireCurrent()
            expect(current.session.isActive).toBe(true)
            return current.conversation.id
        })

        const pending = harness.operations.withCompleteSelectedConversation(
            'post-acquire-navigation',
            operation,
        )
        queueMicrotask(() => {
            harness.current.session.invalidate()
            harness.setSession(null)
            harness.setSelection(makeSelection('character-b', 'conversation-b', 2))
        })

        await expect(pending).resolves.toBe('conversation-a')
        expect(operation).toHaveBeenCalledOnce()
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('releases exactly once when message count inspection fails', async () => {
        const harness = makeHarness()
        vi.spyOn(harness.current.session, 'totalMessages', 'get').mockImplementation(() => {
            throw new Error('message count unavailable')
        })

        await expect(harness.operations.acquireCompleteMessageTarget(
            1,
            'message-count-failure',
        )).rejects.toThrow('message count unavailable')
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it('keeps the same complete session authoritative after a save during an operation', async () => {
        const harness = makeHarness()
        const operationStarted = deferred<void>()
        const resumeOperation = deferred<void>()
        const pending = harness.operations.withCompleteSelectedConversation(
            'generation-save',
            async (context) => {
                const before = context.requireCurrent()
                operationStarted.resolve()
                await resumeOperation.promise
                const after = context.requireCurrent()
                expect(after.session).toBe(before.session)
                expect(after.conversation).toBe(before.conversation)
                return after.selection.storeRevision
            },
        )
        await operationStarted.promise
        harness.current.session.advanceStoreRevision(8)
        harness.setSelection(
            makeSelection('character-a', 'conversation-a', 1, 8),
        )
        resumeOperation.resolve()

        await expect(pending).resolves.toBe(8)
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })

    it.each(['navigation', 'session', 'revision-mismatch'] as const)(
        'rejects %s changes after an operation starts even with the same conversation IDs',
        async (change) => {
            const harness = makeHarness()
            const operationStarted = deferred<void>()
            const resumeOperation = deferred<void>()
            const pending = harness.operations.withCompleteSelectedConversation(
                'generation-stale-authority',
                async (context) => {
                    context.requireCurrent()
                    operationStarted.resolve()
                    await resumeOperation.promise
                    context.requireCurrent().conversation.message[0].data =
                        'stale mutation'
                },
            )
            await operationStarted.promise
            if (change === 'session') harness.setCurrent(makeCurrent())
            harness.setSelection(
                makeSelection(
                    'character-a',
                    'conversation-a',
                    change === 'navigation' ? 2 : 1,
                    change === 'revision-mismatch' ? 8 : 7,
                ),
            )
            resumeOperation.resolve()

            await expect(pending).rejects.toBeInstanceOf(
                SelectedConversationPromotionStaleError,
            )
            expect(harness.current.conversation.message[0].data).toBe('zero')
            expect(harness.defaultLease.release).toHaveBeenCalledOnce()
        },
    )

    it('requires fresh authority before mutating after an async suspension', async () => {
        const harness = makeHarness()
        const operationStarted = deferred<void>()
        const resumeOperation = deferred<void>()
        const originalMessage = harness.current.conversation.message[0].data

        const pending = harness.operations.withCompleteSelectedConversation(
            'async-freshness',
            async (context) => {
                operationStarted.resolve()
                await resumeOperation.promise
                context.requireCurrent().conversation.message[0].data = 'stale mutation'
            },
        )
        await operationStarted.promise
        harness.current.session.invalidate()
        harness.setSession(null)
        harness.setSelection(makeSelection('character-b', 'conversation-b', 2))
        resumeOperation.resolve()

        await expect(pending).rejects.toBeInstanceOf(SelectedConversationPromotionStaleError)
        expect(harness.current.conversation.message[0].data).toBe(originalMessage)
        expect(harness.defaultLease.release).toHaveBeenCalledOnce()
    })
})
