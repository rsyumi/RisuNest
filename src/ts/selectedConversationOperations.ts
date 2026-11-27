import {
    queryChatMessageTargetsByIdsWithStatus,
    type AnchoredChatMessageUiContext,
    type CapturedChatMessageTarget,
    type CurrentChatMessageTarget,
} from './chatMessageUi'
import type { ActiveConversationSession } from './storage/activeConversationSession'
import type { Message } from './storage/database.svelte'
import type {
    ConversationViewportKey,
    ConversationViewportSource,
} from './conversationViewportSource'
import { safeStructuredClone } from './safeStructuredClone'
import isEqual from 'lodash/isEqual'
import {
    SelectedConversationPromotionStaleError,
    type CompleteConversationLease,
    type SelectedConversationTarget,
    type WindowedConversationMutationController,
} from './storage/activeWorkingSet.svelte'

type CapturedSessionChatMessageTarget = Extract<
    CapturedChatMessageTarget,
    { kind: 'session' }
>

export interface SelectedConversationOperationsDependencies {
    captureWindowedMessageMutation?(target: SelectedConversationTarget, index: number, evidence: Readonly<Message>): Promise<WindowedConversationMutationController | null>
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    acquireCompleteConversation(
        reason: string,
        target: SelectedConversationTarget,
    ): Promise<CompleteConversationLease>
    captureCurrent(): CurrentChatMessageTarget | null
    getCurrentSession(): ActiveConversationSession | null
    getCurrentViewportSource(): ConversationViewportSource | null
    acquirePersistentRevision?: AnchoredChatMessageUiContext['acquirePersistentRevision']
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

export interface AcquiredMessageMutationTarget {
    readonly target: CapturedChatMessageTarget
    release(): void
}

export interface PreparedWindowedMessageEditor {
    readonly controller: Promise<WindowedConversationMutationController | null>
    location(): number | null | undefined
    retain(): PreparedWindowedMessageEditor
    release(): void
}

export interface SelectedConversationMessageEditIntent {
    readonly windowedEditor?: PreparedWindowedMessageEditor
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
    acquireMessageMutation(input: CaptureSelectedConversationMessageEditIntentInput, reason: string): Promise<{ target: CapturedChatMessageTarget; release(): void } | null>
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
    rebindMessageEditIntent(
        intent: SelectedConversationMessageEditIntent,
        input: CaptureSelectedConversationMessageEditIntentInput,
    ): SelectedConversationMessageEditIntent
    acquireCompleteMessageTargetForIntent(
        intent: SelectedConversationMessageEditIntent,
        reason: string,
    ): Promise<AcquiredMessageMutationTarget | null>
    /** The only message carrying `messageId`; null when it is missing, repeated or the selection moved. */
    findUniqueMessage(messageId: string): Promise<{ absoluteIndex: number; message: Readonly<Message> } | null>
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
        async acquireMessageMutation(input, reason) {
            const intent = this.captureMessageEditIntent(input)
            if (!intent) return null
            try {
                const acquired = await this.acquireCompleteMessageTargetForIntent(intent, reason)
                if (!acquired) { intent.windowedEditor?.release(); return null }
                return { target: acquired.target, release() { acquired.release(); intent.windowedEditor?.release() } }
            } catch (error) {
                intent.windowedEditor?.release()
                throw error
            }
        },
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
            let windowedEditor: SelectedConversationMessageEditIntent['windowedEditor']
            if (!dependencies.getCurrentSession() && dependencies.captureWindowedMessageMutation) {
                let controller: WindowedConversationMutationController | null | undefined
                let references = 0
                const preparation = dependencies.captureWindowedMessageMutation(selection, input.absoluteIndex, messageEvidence).then(value => {
                    if (!references) value?.release()
                    controller = references ? value : null
                    return controller
                })
                // Preparation begins on editor open. The save or remount observes its rejection.
                void preparation.catch(() => { controller = null })
                const location = () => {
                    if (!references) return null
                    if (!controller) {
                        // A compound editor may promote only while its captured row key still holds.
                        const live = dependencies.getCurrentViewportSource()?.snapshot()
                        return live?.sourceToken === input.sourceToken && live.keyAt(input.absoluteIndex) === input.rowKey
                            ? input.absoluteIndex : controller === undefined ? undefined : null
                    }
                    return controller.editorRow?.()?.absoluteIndex ?? (controller.isCurrent() ? input.absoluteIndex : null)
                }
                const retain = (): PreparedWindowedMessageEditor => {
                    references++
                    let released = false
                    return { controller: preparation, location, retain, release() {
                        if (released) return
                        released = true
                        if (--references === 0) controller?.release()
                    } }
                }
                windowedEditor = retain()
            }
            return Object.freeze({
                ...(windowedEditor ? { windowedEditor } : {}),
                selection,
                absoluteIndex: input.absoluteIndex,
                sourceToken: input.sourceToken,
                sourceVersion: input.sourceVersion,
                rowKey: input.rowKey,
                messageEvidence,
            })
        },

        rebindMessageEditIntent(intent, input) {
            if (intent.windowedEditor || !isEqual(intent.messageEvidence, input.message)) return intent
            const rebound = this.captureMessageEditIntent(input)
            if (!rebound || !matchesConversation(intent.selection, rebound.selection)) return intent
            return Object.freeze({ ...rebound, messageEvidence: intent.messageEvidence })
        },
        async acquireCompleteMessageTargetForIntent(
            intent,
            reason,
        ): Promise<AcquiredMessageMutationTarget | null> {
            const selection = dependencies.captureSelectedConversationTarget()
            // A committed refresh starts a new navigation generation; the evidence below
            // still has to match before the same message is edited.
            if (!selection || !matchesConversation(intent.selection, selection)) {
                throw new SelectedConversationPromotionStaleError()
            }
            const editor = intent.windowedEditor
            const controller = editor ? await editor.controller :
                await dependencies.captureWindowedMessageMutation?.(selection, intent.absoluteIndex, intent.messageEvidence)
            if (controller) {
                const current = dependencies.captureCurrent()
                if (!current || !controller.isCurrent()) { if (!editor) controller.release(); return null }
                const row = controller.editorRow?.() ?? { absoluteIndex: intent.absoluteIndex, message: controller.chat.message[0] }
                return { target: { ...current, kind: 'windowed' as const, session: null, locator: null,
                    ...row, controller }, release: () => { if (!editor) controller.release() } }
            }
            if (editor && editor.location() !== intent.absoluteIndex) return null
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
        async findUniqueMessage(messageId) {
            const acquirePersistentRevision = dependencies.acquirePersistentRevision
            if (!acquirePersistentRevision) return null
            const context: AnchoredChatMessageUiContext = {
                captureCurrent: dependencies.captureCurrent,
                getCurrentSession: dependencies.getCurrentSession,
                captureSelectedConversationTarget: dependencies.captureSelectedConversationTarget,
                acquirePersistentRevision,
                acquireCompleteConversation(reason, target) {
                    const selection = target ?? dependencies.captureSelectedConversationTarget()
                    if (!selection) throw new SelectedConversationPromotionStaleError()
                    return dependencies.acquireCompleteConversation(reason, selection)
                },
            }
            const first = await queryChatMessageTargetsByIdsWithStatus(context, [messageId], 'first')
            const last = await queryChatMessageTargetsByIdsWithStatus(context, [messageId], 'last')
            const found = first.targets[0]
            if (
                first.status !== 'current' || last.status !== 'current' ||
                !found || last.targets[0]?.absoluteIndex !== found.absoluteIndex
            ) return null
            return { absoluteIndex: found.absoluteIndex, message: found.message }
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

function matchesConversation(
    left: SelectedConversationTarget,
    right: SelectedConversationTarget,
): boolean {
    return left.characterId === right.characterId && left.conversationId === right.conversationId
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
