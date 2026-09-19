import type { Chat, Message } from '../storage/database.svelte'

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
}

export type SummaryAwarePromptHistoryDecision =
    | { route: 'summary-aware'; plan: SummaryAwarePromptHistoryPlan }
    | { route: 'complete'; reason: string }

function effectiveMessages(messages: readonly Message[]): Message[] {
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
    const raw = chat.hypaV3Data as { summaries?: StoredHypaSummary[] } | undefined
    if (!raw || !Array.isArray(raw.summaries) || raw.summaries.length === 0) {
        return { route: 'complete', reason: 'no-summary' }
    }
    const messages = effectiveMessages(chat.message)
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
    const coveredMessages = messages.slice(0, boundaryIndex + 1)
    if (coveredMessages.some((message) => !parserInert(message))) {
        return { route: 'complete', reason: 'summarized-message-has-dynamic-processing' }
    }
    return {
        route: 'summary-aware',
        plan: {
            boundaryMemo,
            coveredMessageIds: new Set(memos.slice(0, boundaryIndex + 1)),
            effectiveMessageMemos: memos,
            totalMessages: chat.message.length,
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
