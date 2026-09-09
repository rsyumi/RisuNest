import isEqual from 'lodash/isEqual'
import {
    ChatParserHistoryProjectionStaleError,
    createChatParserHistoryProjection,
    type BoundedChatParserHistoryProjection,
    type ChatParserCompleteProjectionLease,
    type ChatParserCompleteProjectionReason,
} from './chatParserHistoryProjection'
import {
    classifyChatParserHistory,
    type ChatParserUnsafeHistoryDependency,
} from './chatParserHistory'
import type { ConversationViewportRow } from './conversationViewportSource'
import type { CurrentChatMessageTarget } from './chatMessageUi'
import type { ProcessScriptCaptureContext } from './process/scripts'
import { getRegexExecutionPlan } from './process/regexExecutionPlan'
import type {
    CompleteConversationLease,
    SelectedConversationTarget,
} from './storage/activeWorkingSet.svelte'
import type { PersistentDataRuntime } from './storage/persistentDataRuntime'
import type { customscript, triggerscript } from './storage/database.svelte'

export function collectLiveChatParserUnsafeDependencies(input: {
    readonly triggers: readonly triggerscript[]
    readonly pluginV2EditDisplay: boolean
    readonly regexScripts: readonly customscript[]
}): ChatParserUnsafeHistoryDependency[] {
    const dependencies: ChatParserUnsafeHistoryDependency[] = []
    if (input.triggers.some((trigger) => trigger.effect?.[0]?.type === 'triggerlua')) {
        dependencies.push('lua')
    }
    if (input.triggers.some((trigger) => trigger.type === 'display')) {
        dependencies.push('display-trigger')
    }
    if (input.pluginV2EditDisplay) dependencies.push('plugin-v2')
    const plan = getRegexExecutionPlan([...input.regexScripts], 'editdisplay')
    if (plan.entries.some((entry) => (
        entry.actions.includes('inject') || entry.replacement.startsWith('@@inject')
    ))) dependencies.push('inject')
    return dependencies
}

export interface LiveChatParserProjectionRequest {
    readonly row: ConversationViewportRow
    readonly totalMessages: number
    readonly signal?: AbortSignal
    readonly isCurrent?: () => boolean
}

export interface LiveChatParserConversationStartRequest {
    readonly greeting: string
    readonly totalMessages: number
    readonly signal?: AbortSignal
    readonly isCurrent?: () => boolean
}

export type BoundedLiveChatParserProjection = BoundedChatParserHistoryProjection

export interface CompleteLiveChatParserProjection {
    readonly kind: 'complete'
    readonly characterId: string
    readonly conversationId: string
    readonly revision: number
    readonly totalMessages: number
    readonly chatID: number
    readonly projectedChatID: number
    readonly historyOffset: 0
    readonly reasons: readonly ChatParserCompleteProjectionReason[]
    release(): void
}

export type LiveChatParserProjection =
    | BoundedLiveChatParserProjection
    | CompleteLiveChatParserProjection

export interface LiveChatParserProjectionResolver {
    resolve(input: LiveChatParserProjectionRequest): Promise<LiveChatParserProjection>
    acquireConversationStart?(
        input: LiveChatParserConversationStartRequest,
    ): Promise<{ release(): void } | null>
}

type LiveParserRuntime = Pick<
    PersistentDataRuntime,
    | 'store'
    | 'captureSelectedConversationTarget'
    | 'captureSelectedConversationAuthority'
    | 'acquireCompleteConversation'
>

export interface SelectedConversationLiveParserProjectionDependencies {
    readonly runtime: LiveParserRuntime
    readonly maxProjectionMessages: number
    captureCurrent(): CurrentChatMessageTarget | null
    createBoundedContextSeed(current: CurrentChatMessageTarget): ProcessScriptCaptureContext
    createCompleteContext(current: CurrentChatMessageTarget): ProcessScriptCaptureContext
    parserSource(current: CurrentChatMessageTarget): unknown
    parserIndirections?(current: CurrentChatMessageTarget): Readonly<Record<string, unknown>>
    unsafeDependencies(
        current: CurrentChatMessageTarget,
    ): readonly ChatParserUnsafeHistoryDependency[]
}

export function bindCompleteLiveParserContextAuthority(
    context: ProcessScriptCaptureContext,
    current: CurrentChatMessageTarget,
): ProcessScriptCaptureContext {
    const characters = [...context.parserContext.database.characters]
    characters[context.parserContext.selectedCharID] = current.character
    return {
        ...context,
        parserContext: {
            ...context.parserContext,
            database: {
                ...context.parserContext.database,
                characters,
            },
            character: current.character,
        },
    }
}

export function createSelectedConversationLiveParserProjectionResolver(
    dependencies: SelectedConversationLiveParserProjectionDependencies,
): LiveChatParserProjectionResolver {
    return {
        async acquireConversationStart(input) {
            assertConversationStartCurrent(input)
            const target = dependencies.runtime.captureSelectedConversationTarget()
            if (!target) throw new ChatParserHistoryProjectionStaleError()
            const current = captureExactCurrent(dependencies, target)
            const classification = classifyChatParserHistory({
                source: [input.greeting, dependencies.parserSource(current)],
                indirections: dependencies.parserIndirections?.(current),
                unsafeDependencies: dependencies.unsafeDependencies(current),
            })
            if (
                !classification.requiresFullHistory &&
                classification.absoluteMessageIndices.length === 0
            )
                return null

            // A greeting is not a persisted message row, including in an empty
            // conversation. Its live scripts still need the same owned history
            // as ordinary rows, for the entire asynchronous component lifetime.
            const lease = await dependencies.runtime.acquireCompleteConversation(
                'live-chat-greeting',
                target,
            )
            try {
                assertConversationStartCurrent(input)
                // Promotion can flush pending data and retarget the same
                // conversation to a newer store revision. The acquired lease
                // owns that revision; navigation must still match the request.
                if (
                    target.characterId !== lease.target.characterId ||
                    target.conversationId !== lease.target.conversationId ||
                    target.navigationGeneration !== lease.target.navigationGeneration
                )
                    throw new ChatParserHistoryProjectionStaleError()
                captureExactCurrent(dependencies, lease.target, lease)
                if (lease.session.totalMessages !== input.totalMessages) {
                    throw new ChatParserHistoryProjectionStaleError()
                }
                return { release: idempotentRelease(lease) }
            } catch (error) {
                lease.release()
                throw error
            }
        },
        async resolve(input): Promise<LiveChatParserProjection> {
            assertRequestCurrent(input)
            const target = dependencies.runtime.captureSelectedConversationTarget()
            if (!target) throw new ChatParserHistoryProjectionStaleError()
            const current = captureExactCurrent(dependencies, target)
            const authority = dependencies.runtime.captureSelectedConversationAuthority()

            if (!authority) {
                const lease = await acquireExactCompleteLease(dependencies, target, input)
                return completeResultFromLease(lease, input, [])
            }
            if (
                authority.characterId !== target.characterId
                || authority.conversationId !== target.conversationId
                || authority.storeRevision !== target.storeRevision
                || authority.totalMessages !== input.totalMessages
            ) throw new ChatParserHistoryProjectionStaleError()

            const projection = await createChatParserHistoryProjection({
                characterId: authority.characterId,
                conversationId: authority.conversationId,
                revision: authority.storeRevision,
                totalMessages: authority.totalMessages,
                currentAbsoluteIndex: input.row.absoluteIndex,
                maxProjectionMessages: dependencies.maxProjectionMessages,
                parserSource: dependencies.parserSource(current),
                parserIndirections: dependencies.parserIndirections?.(current),
                unsafeDependencies: dependencies.unsafeDependencies(current),
                contextSeed: dependencies.createBoundedContextSeed(current),
                reader: {
                    revision: authority.storeRevision,
                    readConversationWindow: (query) => (
                        dependencies.runtime.store.readConversationWindow(query)
                    ),
                },
                signal: input.signal,
                isCurrent: () => requestMatchesSelection(dependencies, target, input),
                acquireCompleteProjection: async () => {
                    const lease = await acquireExactCompleteLease(dependencies, target, input)
                    let context: ProcessScriptCaptureContext
                    try {
                        context = dependencies.createCompleteContext(
                            captureExactCurrent(dependencies, target, lease),
                        )
                    } catch (error) {
                        lease.release()
                        throw error
                    }
                    return completeProjectionLease(lease, context, input.totalMessages)
                },
            })
            if (projection.kind === 'complete') {
                return completeResultFromLease(
                    projection.lease,
                    input,
                    projection.reasons,
                )
            }
            if (!isEqual(
                projection.messages[projection.projectedChatID],
                input.row.message,
            )) throw new ChatParserHistoryProjectionStaleError()
            return projection
        },
    }
}

async function acquireExactCompleteLease(
    dependencies: SelectedConversationLiveParserProjectionDependencies,
    target: SelectedConversationTarget,
    input: LiveChatParserProjectionRequest,
): Promise<CompleteConversationLease> {
    const lease = await dependencies.runtime.acquireCompleteConversation(
        'live-chat-parser',
        target,
    )
    try {
        assertRequestCurrent(input)
        const current = captureExactCurrent(dependencies, target, lease)
        if (lease.session.totalMessages !== input.totalMessages) {
            throw new ChatParserHistoryProjectionStaleError()
        }
        const currentMessage = lease.session.readMessage(
            lease.session.locate(input.row.absoluteIndex),
        )
        if (!isEqual(currentMessage, input.row.message)) {
            throw new ChatParserHistoryProjectionStaleError()
        }
        void current
        return lease
    } catch (error) {
        lease.release()
        throw error
    }
}

function captureExactCurrent(
    dependencies: SelectedConversationLiveParserProjectionDependencies,
    target: SelectedConversationTarget,
    lease?: CompleteConversationLease,
): CurrentChatMessageTarget {
    const recaptured = dependencies.runtime.captureSelectedConversationTarget()
    const current = dependencies.captureCurrent()
    if (
        !recaptured
        || !current
        || !matchesSelection(target, recaptured)
        || (lease && !matchesSelection(target, lease.target))
        || current.character.chaId !== target.characterId
        || current.conversation.id !== target.conversationId
        || current.character.chats[current.character.chatPage] !== current.conversation
        || (lease && !lease.session.matchesConversation(target.characterId, current.conversation))
    ) throw new ChatParserHistoryProjectionStaleError()
    return current
}

function completeProjectionLease(
    lease: CompleteConversationLease,
    context: ProcessScriptCaptureContext,
    totalMessages: number,
): ChatParserCompleteProjectionLease {
    return {
        characterId: lease.target.characterId,
        conversationId: lease.target.conversationId,
        revision: lease.target.storeRevision,
        totalMessages,
        context,
        release: idempotentRelease(lease),
    }
}

function completeResultFromLease(
    lease: CompleteConversationLease | ChatParserCompleteProjectionLease,
    input: LiveChatParserProjectionRequest,
    reasons: readonly ChatParserCompleteProjectionReason[],
): CompleteLiveChatParserProjection {
    const release = idempotentRelease(lease)
    const identity = 'target' in lease
        ? {
            characterId: lease.target.characterId,
            conversationId: lease.target.conversationId,
            revision: lease.target.storeRevision,
        }
        : {
            characterId: lease.characterId,
            conversationId: lease.conversationId,
            revision: lease.revision,
        }
    return {
        kind: 'complete',
        ...identity,
        totalMessages: input.totalMessages,
        chatID: input.row.absoluteIndex,
        projectedChatID: input.row.absoluteIndex,
        historyOffset: 0,
        reasons,
        release,
    }
}

function requestMatchesSelection(
    dependencies: SelectedConversationLiveParserProjectionDependencies,
    target: SelectedConversationTarget,
    input: LiveChatParserProjectionRequest,
): boolean {
    if (input.signal?.aborted || input.isCurrent?.() === false) return false
    const recaptured = dependencies.runtime.captureSelectedConversationTarget()
    return recaptured !== null && matchesSelection(target, recaptured)
}

function assertRequestCurrent(input: LiveChatParserProjectionRequest): void {
    if (input.signal?.aborted) {
        throw new DOMException('Live chat parser projection was cancelled', 'AbortError')
    }
    if (input.isCurrent?.() === false) throw new ChatParserHistoryProjectionStaleError()
    if (
        !Number.isSafeInteger(input.totalMessages)
        || input.totalMessages <= 0
        || input.row.absoluteIndex < 0
        || input.row.absoluteIndex >= input.totalMessages
    ) throw new RangeError('Live chat parser projection row is out of range')
}

function assertConversationStartCurrent(input: LiveChatParserConversationStartRequest): void {
    if (input.signal?.aborted) {
        throw new DOMException('Live greeting parser was cancelled', 'AbortError')
    }
    if (input.isCurrent?.() === false) throw new ChatParserHistoryProjectionStaleError()
    if (!Number.isSafeInteger(input.totalMessages) || input.totalMessages < 0) {
        throw new RangeError('Live greeting parser message count must be nonnegative')
    }
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

function idempotentRelease(lease: { release(): void | Promise<void> }): () => void {
    let released = false
    return () => {
        if (released) return
        released = true
        void lease.release()
    }
}
