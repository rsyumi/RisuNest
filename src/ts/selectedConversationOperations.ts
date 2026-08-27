import type {
    CapturedChatMessageTarget,
    CurrentChatMessageTarget,
} from './chatMessageUi'
import type { ActiveConversationSession } from './storage/activeConversationSession'
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
}

export interface CompleteSelectedConversationContext extends CurrentChatMessageTarget {
    readonly selection: SelectedConversationTarget
    readonly session: ActiveConversationSession
}

export interface AcquiredCompleteMessageTarget {
    readonly target: CapturedSessionChatMessageTarget
    release(): void
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
}

interface ValidatedCompleteConversation {
    readonly lease: CompleteConversationLease
    readonly context: CompleteSelectedConversationContext
}

export function createSelectedConversationOperations(
    dependencies: SelectedConversationOperationsDependencies,
): SelectedConversationOperations {
    const acquireValidatedConversation = async (
        reason: string,
    ): Promise<ValidatedCompleteConversation | null> => {
        const captured = dependencies.captureSelectedConversationTarget()
        if (!captured) return null

        const lease = await dependencies.acquireCompleteConversation(reason, captured)
        try {
            const recaptured = dependencies.captureSelectedConversationTarget()
            const current = dependencies.captureCurrent()
            const session = dependencies.getCurrentSession()
            if (!isExactCompleteAuthority(captured, recaptured, current, session, lease)) {
                throw new SelectedConversationPromotionStaleError()
            }
            return {
                lease,
                context: {
                    ...current,
                    selection: recaptured,
                    session,
                },
            }
        } catch (error) {
            lease.release()
            throw error
        }
    }

    return {
        async withCompleteSelectedConversation<T>(reason, operation): Promise<T | null> {
            const acquired = await acquireValidatedConversation(reason)
            if (!acquired) return null
            try {
                return await operation(acquired.context)
            } finally {
                acquired.lease.release()
            }
        },

        async acquireCompleteMessageTarget(
            absoluteIndex,
            reason,
        ): Promise<AcquiredCompleteMessageTarget | null> {
            if (!Number.isSafeInteger(absoluteIndex) || absoluteIndex < 0) return null
            const acquired = await acquireValidatedConversation(reason)
            if (!acquired) return null
            const release = idempotentRelease(acquired.lease)
            const { character, conversation, session } = acquired.context
            if (absoluteIndex >= session.totalMessages) {
                release()
                return null
            }
            try {
                const locator = session.locate(absoluteIndex)
                return {
                    target: {
                        kind: 'session',
                        absoluteIndex,
                        character,
                        conversation,
                        message: session.readMessage(locator),
                        session,
                        locator,
                    },
                    release,
                }
            } catch {
                release()
                throw new SelectedConversationPromotionStaleError()
            }
        },
    }
}

function isExactCompleteAuthority(
    captured: SelectedConversationTarget,
    recaptured: SelectedConversationTarget | null,
    current: CurrentChatMessageTarget | null,
    session: ActiveConversationSession | null,
    lease: CompleteConversationLease,
): recaptured is SelectedConversationTarget {
    return recaptured !== null
        && current !== null
        && session !== null
        && matchesSelection(captured, recaptured)
        && matchesSelection(captured, lease.target)
        && lease.session === session
        && session.storeRevision === recaptured.storeRevision
        && current.character.chaId === recaptured.characterId
        && current.conversation.id === recaptured.conversationId
        && current.character.chats[current.character.chatPage] === current.conversation
        && session.matchesConversation(recaptured.characterId, current.conversation)
}

function matchesSelection(
    left: SelectedConversationTarget,
    right: SelectedConversationTarget,
): boolean {
    return left.characterId === right.characterId
        && left.conversationId === right.conversationId
        && left.navigationGeneration === right.navigationGeneration
        && left.storeRevision === right.storeRevision
}

function idempotentRelease(lease: CompleteConversationLease): () => void {
    let released = false
    return () => {
        if (released) return
        released = true
        lease.release()
    }
}
