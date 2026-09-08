import type {
    CapturedChatMessageTarget,
    CurrentChatMessageTarget,
} from './chatMessageUi'
import type { ActiveConversationSession } from './storage/activeConversationSession'
import type { Message } from './storage/database.svelte'
import type {
    ConversationViewportKey,
    ConversationViewportSource,
} from './conversationViewportSource'
import { safeStructuredClone } from './polyfill'
import isEqual from 'lodash/isEqual'
import {
    SelectedConversationPromotionStaleError,
    type CompleteConversationLease,
    type SelectedConversationTarget,
} from './storage/activeWorkingSet.svelte'

type CapturedSessionChatMessageTarget = Extract<
    CapturedChatMessageTarget,
    { kind: 'session' }
>

export interface SelectedConversationOperationsDependencies {
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    acquireCompleteConversation(
        reason: string,
        target: SelectedConversationTarget,
    ): Promise<CompleteConversationLease>
    captureCurrent(): CurrentChatMessageTarget | null
    getCurrentSession(): ActiveConversationSession | null
    getCurrentViewportSource(): ConversationViewportSource | null
}

export interface CompleteSelectedConversationAuthority extends CurrentChatMessageTarget {
    readonly selection: SelectedConversationTarget
    readonly session: ActiveConversationSession
}

export interface CompleteSelectedConversationContext {
    /** Recapture immediately before each authority access, including after every await. */
    requireCurrent(): CompleteSelectedConversationAuthority
}

export interface AcquiredCompleteMessageTarget {
    readonly target: CapturedSessionChatMessageTarget
    release(): void
}

export interface SelectedConversationMessageEditIntent {
    readonly selection: SelectedConversationTarget
    readonly absoluteIndex: number
    readonly sourceToken: string
    readonly sourceVersion: number
    readonly rowKey: ConversationViewportKey
    readonly messageEvidence: Readonly<Message>
}

export interface CaptureSelectedConversationMessageEditIntentInput {
    readonly absoluteIndex: number
    readonly sourceToken: string
    readonly sourceVersion: number
    readonly rowKey: ConversationViewportKey
    readonly message: Readonly<Message>
}

export interface SelectedConversationOperations {
    withCompleteSelectedConversation<T>(
        reason: string,
        operation: (context: CompleteSelectedConversationContext) => T | Promise<T>,
    ): Promise<T | null>
    acquireCompleteMessageTarget(
        absoluteIndex: number,
        reason: string,
    ): Promise<AcquiredCompleteMessageTarget | null>
    captureMessageEditIntent(
        input: CaptureSelectedConversationMessageEditIntentInput,
    ): SelectedConversationMessageEditIntent | null
    acquireCompleteMessageTargetForIntent(
        intent: SelectedConversationMessageEditIntent,
        reason: string,
    ): Promise<AcquiredCompleteMessageTarget | null>
}

interface StartedCompleteConversation<T> {
    readonly lease: CompleteConversationLease
    readonly value: T
}

export function createSelectedConversationOperations(
    dependencies: SelectedConversationOperationsDependencies,
): SelectedConversationOperations {
    const startCompleteConversation = async <T>(
        reason: string,
        continuation: (
            context: CompleteSelectedConversationContext,
            authority: CompleteSelectedConversationAuthority,
        ) => T,
        captured = dependencies.captureSelectedConversationTarget(),
    ): Promise<StartedCompleteConversation<T> | null> => {
        if (!captured) return null

        const lease = await dependencies.acquireCompleteConversation(reason, captured)
        try {
            const requireCurrent = () => captureExactCompleteAuthority(
                dependencies,
                captured,
                lease,
            )
            const authority = requireCurrent()
            const context: CompleteSelectedConversationContext = { requireCurrent }
            return {
                lease,
                value: continuation(context, authority),
            }
        } catch (error) {
            lease.release()
            throw error
        }
    }

    return {
        async withCompleteSelectedConversation<T>(reason, operation): Promise<T | null> {
            const started = await startCompleteConversation(
                reason,
                (context) => operation(context),
            )
            if (!started) return null
            try {
                return await started.value
            } finally {
                started.lease.release()
            }
        },

        async acquireCompleteMessageTarget(
            absoluteIndex,
            reason,
        ): Promise<AcquiredCompleteMessageTarget | null> {
            const selection = dependencies.captureSelectedConversationTarget()
            if (!selection) return null
            return acquireCompleteMessageTarget(selection, absoluteIndex, reason)
        },

        captureMessageEditIntent(input): SelectedConversationMessageEditIntent | null {
            const selection = dependencies.captureSelectedConversationTarget()
            const snapshot = dependencies.getCurrentViewportSource()?.snapshot()
            const row = snapshot?.rowAt(input.absoluteIndex)
            if (
                !selection ||
                !Number.isSafeInteger(input.absoluteIndex) ||
                input.absoluteIndex < 0 ||
                typeof input.sourceToken !== 'string' ||
                input.sourceToken.length === 0 ||
                !Number.isSafeInteger(input.sourceVersion) ||
                input.sourceVersion < 0 ||
                typeof input.rowKey !== 'string' ||
                input.rowKey.length === 0 ||
                !snapshot ||
                snapshot.sourceToken !== input.sourceToken ||
                snapshot.version !== input.sourceVersion ||
                row?.absoluteIndex !== input.absoluteIndex ||
                row.key !== input.rowKey ||
                row.sourceVersion !== input.sourceVersion ||
                !isEqual(row.message, input.message)
            ) return null
            let messageEvidence: Readonly<Message>
            try {
                messageEvidence = Object.freeze(safeStructuredClone(input.message))
            } catch {
                return null
            }
            return Object.freeze({
                selection,
                absoluteIndex: input.absoluteIndex,
                sourceToken: input.sourceToken,
                sourceVersion: input.sourceVersion,
                rowKey: input.rowKey,
                messageEvidence,
            })
        },

        async acquireCompleteMessageTargetForIntent(
            intent,
            reason,
        ): Promise<AcquiredCompleteMessageTarget | null> {
            const selection = dependencies.captureSelectedConversationTarget()
            if (!selection || !matchesSelection(intent.selection, selection)) {
                throw new SelectedConversationPromotionStaleError()
            }
            const acquired = await acquireCompleteMessageTarget(
                selection,
                intent.absoluteIndex,
                reason,
            )
            if (!acquired) return null
            let matchesEvidence = false
            try {
                matchesEvidence = isEqual(acquired.target.message, intent.messageEvidence)
            } catch {
                matchesEvidence = false
            }
            if (matchesEvidence) return acquired
            acquired.release()
            return null
        },
    }

    async function acquireCompleteMessageTarget(
        selection: SelectedConversationTarget,
        absoluteIndex: number,
        reason: string,
    ): Promise<AcquiredCompleteMessageTarget | null> {
            if (!Number.isSafeInteger(absoluteIndex) || absoluteIndex < 0) return null
            const started = await startCompleteConversation(
                reason,
                (_context, { character, conversation, session }) => {
                    if (absoluteIndex >= session.totalMessages) return null
                    try {
                        const locator = session.locate(absoluteIndex)
                        return {
                            kind: 'session' as const,
                            absoluteIndex,
                            character,
                            conversation,
                            message: session.readMessage(locator),
                            session,
                            locator,
                        }
                    } catch {
                        throw new SelectedConversationPromotionStaleError()
                    }
                },
                selection,
            )
            if (!started) return null
            const release = idempotentRelease(started.lease)
            if (!started.value) {
                release()
                return null
            }
            return {
                target: started.value,
                release,
            }
    }
}

function captureExactCompleteAuthority(
    dependencies: SelectedConversationOperationsDependencies,
    captured: SelectedConversationTarget,
    lease: CompleteConversationLease,
): CompleteSelectedConversationAuthority {
    const recaptured = dependencies.captureSelectedConversationTarget()
    const current = dependencies.captureCurrent()
    const session = dependencies.getCurrentSession()
    if (
        recaptured === null ||
        current === null ||
        session === null ||
        !matchesSelection(captured, recaptured) ||
        !matchesSelection(captured, lease.target) ||
        lease.session !== session ||
        // Promotion can flush pending saves; later saves can advance the leased session.
        // Validate the live revision against that session, not the captured revision.
        session.storeRevision !== recaptured.storeRevision ||
        current.character.chaId !== recaptured.characterId ||
        current.conversation.id !== recaptured.conversationId ||
        current.character.chats[current.character.chatPage] !==
            current.conversation ||
        !session.matchesConversation(
            recaptured.characterId,
            current.conversation,
        )
    )
        throw new SelectedConversationPromotionStaleError()
    return {
        ...current,
        selection: recaptured,
        session,
    }
}

function matchesSelection(
    left: SelectedConversationTarget,
    right: SelectedConversationTarget,
): boolean {
    return (
        left.characterId === right.characterId &&
        left.conversationId === right.conversationId &&
        left.navigationGeneration === right.navigationGeneration
    )
}

function idempotentRelease(lease: CompleteConversationLease): () => void {
    let released = false
    return () => {
        if (released) return
        released = true
        lease.release()
    }
}
