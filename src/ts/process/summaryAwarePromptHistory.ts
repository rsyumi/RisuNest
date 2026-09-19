import type { Chat, Message, customscript } from '../storage/database.svelte'
import type { ConversationMessageMetadata } from '../storage/persistentDataStore'
import { getRegexExecutionPlan } from './regexExecutionPlan'

interface StoredHypaSummary {
    chatMemos?: unknown
}

function validStoredSummary(summary: unknown): summary is { chatMemos: string[] } {
    if (!summary || typeof summary !== 'object' || !('chatMemos' in summary)) return false
    const chatMemos = summary.chatMemos
    return Array.isArray(chatMemos) && chatMemos.every((memo) => typeof memo === 'string')
}

export interface SummaryAwarePromptHistoryPlan {
    boundaryMemo: string
    coveredMessageIds: ReadonlySet<string>
    effectiveMessageMemos: readonly string[]
    totalMessages: number
    bodyStartIndex: number
}

export type SummaryAwarePromptHistoryDecision =
    | { route: 'summary-aware'; plan: SummaryAwarePromptHistoryPlan }
    | { route: 'complete'; reason: string }

function effectiveMessages<T extends Pick<Message, 'disabled'>>(messages: readonly T[]): T[] {
    let startIndex = 0
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        if (messages[index]?.disabled === 'allBefore') {
            startIndex = index + 1
            break
        }
    }
    return messages
        .slice(startIndex)
        .filter((message) => message.disabled !== true && message.disabled !== 'allBefore')
}

export function planSummaryAwarePromptMetadata(
    chat: Omit<Chat, 'message'>,
    messages: readonly ConversationMessageMetadata[],
    preserveOrphanedMemory: boolean,
): SummaryAwarePromptHistoryDecision {
    const ids = new Set<string>()
    for (const message of messages) {
        if (!message.chatId) return { route: 'complete', reason: 'missing-message-id' }
        if (ids.has(message.chatId) || message.chatId === 'NewChat') {
            return { route: 'complete', reason: 'duplicate-message-id' }
        }
        ids.add(message.chatId)
    }
    const decision = planSummaryAwareMessages(chat, messages, preserveOrphanedMemory)
    if (decision.route === 'complete') return decision
    const boundaryAbsoluteIndex = messages.findIndex(
        (message) => message.chatId === decision.plan.boundaryMemo,
    )
    if (boundaryAbsoluteIndex < 0) {
        return { route: 'complete', reason: 'unresolved-summary-boundary' }
    }
    if (messages.some((message) => !message.parserInert)) {
        return { route: 'complete', reason: 'summarized-message-has-dynamic-processing' }
    }
    if (messages.slice(0, boundaryAbsoluteIndex + 1).some(
        (message) => message.disabled === 'allBefore',
    )) return { route: 'complete', reason: 'all-before-before-summary-boundary' }
    return {
        route: 'summary-aware',
        plan: { ...decision.plan, bodyStartIndex: boundaryAbsoluteIndex + 1 },
    }
}

function parserInert(message: Message): boolean {
    return typeof message.data === 'string'
        && !message.data.includes('{{')
        && !message.data.includes('}}')
        && !message.data.includes('<Thoughts>')
        && !message.data.includes('</Thoughts>')
}

export function planSummaryAwarePromptHistory(
    chat: Chat,
    preserveOrphanedMemory: boolean,
): SummaryAwarePromptHistoryDecision {
    const decision = planSummaryAwareMessages(chat, chat.message, preserveOrphanedMemory)
    if (decision.route === 'complete') return decision
    const boundaryAbsoluteIndex = chat.message.findIndex(
        (message) => message.chatId === decision.plan.boundaryMemo,
    )
    const covered = chat.message.slice(0, boundaryAbsoluteIndex + 1)
    if (covered.some((message) => !parserInert(message))) {
        return { route: 'complete', reason: 'summarized-message-has-dynamic-processing' }
    }
    return {
        route: 'summary-aware',
        plan: { ...decision.plan, bodyStartIndex: boundaryAbsoluteIndex + 1 },
    }
}

export function planSummaryAwareProcessedHistory(
    chat: Chat,
    scripts: customscript[],
    preserveOrphanedMemory: boolean,
    minimumTailMessages: number,
    characterId: string,
): SummaryAwarePromptHistoryDecision {
    if (chat.message.some((message) => typeof message.data !== 'string'
        || /[<{]/.test(message.data)
        || (message.saying && message.saying !== characterId))) {
        return { route: 'complete', reason: 'message-processing-dependency' }
    }
    const plan = getRegexExecutionPlan(scripts, 'editprocess')
    if (!plan.workerEligible || plan.entries.some((entry) => entry.compileError !== undefined)) {
        return { route: 'complete', reason: 'stateful-or-dynamic-regex' }
    }
    const decision = planSummaryAwarePromptHistory(chat, preserveOrphanedMemory)
    if (decision.route === 'complete') return decision
    if (decision.plan.effectiveMessageMemos.includes('NewChat')) {
        return { route: 'complete', reason: 'ambiguous-preamble-id' }
    }
    const tailCount = effectiveMessages(chat.message.slice(decision.plan.bodyStartIndex)).length
    if (tailCount < minimumTailMessages) {
        return { route: 'complete', reason: 'memory-query-needs-covered-history' }
    }
    return decision
}

function planSummaryAwareMessages(
    chat: Omit<Chat, 'message'>,
    sourceMessages: readonly Pick<Message, 'chatId' | 'disabled'>[],
    preserveOrphanedMemory: boolean,
): SummaryAwarePromptHistoryDecision {
    const raw = chat.hypaV3Data as { summaries?: StoredHypaSummary[] } | undefined
    if (!raw || !Array.isArray(raw.summaries) || raw.summaries.length === 0) {
        return { route: 'complete', reason: 'no-summary' }
    }
    const messages = effectiveMessages(sourceMessages)
    const memos: string[] = []
    const memoSet = new Set<string>()
    for (const message of messages) {
        if (typeof message.chatId !== 'string' || message.chatId.length === 0) {
            return { route: 'complete', reason: 'missing-message-id' }
        }
        if (memoSet.has(message.chatId)) {
            return { route: 'complete', reason: 'duplicate-message-id' }
        }
        memoSet.add(message.chatId)
        memos.push(message.chatId)
    }
    if (!raw.summaries.every(validStoredSummary)) {
        return { route: 'complete', reason: 'invalid-summary-shape' }
    }
    const summaries = raw.summaries.flatMap((summary) => {
        const chatMemos = summary.chatMemos as string[]
        if (!preserveOrphanedMemory && chatMemos.some((memo) => !memoSet.has(memo))) return []
        return [chatMemos]
    })
    if (summaries.length === 0) return { route: 'complete', reason: 'no-valid-summary' }
    const boundaryMemo = summaries.at(-1)?.at(-1)
    if (!boundaryMemo) return { route: 'complete', reason: 'empty-summary-boundary' }
    const boundaryIndex = memos.indexOf(boundaryMemo)
    if (boundaryIndex === -1) return { route: 'complete', reason: 'unresolved-summary-boundary' }
    return {
        route: 'summary-aware',
        plan: {
            boundaryMemo,
            coveredMessageIds: new Set(memos.slice(0, boundaryIndex + 1)),
            effectiveMessageMemos: memos,
            totalMessages: sourceMessages.length,
            bodyStartIndex: 0,
        },
    }
}

export function assertSummaryAwarePromptHistoryCurrent(
    chat: Chat,
    plan: SummaryAwarePromptHistoryPlan,
): void {
    if (chat.message.length !== plan.totalMessages) {
        throw new Error('Summary-aware prompt history changed after admission')
    }
    const current = effectiveMessages(chat.message).map((message) => message.chatId)
    if (current.length !== plan.effectiveMessageMemos.length
        || current.some((memo, index) => memo !== plan.effectiveMessageMemos[index])) {
        throw new Error('Summary-aware prompt history identity changed after admission')
    }
}
