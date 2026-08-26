import type { Chat, Message } from '../storage/database.svelte'
import type {
    ActiveConversationSession,
    MessageLocator,
} from '../storage/activeConversationSession'
import type { ConversationHistoryOperation } from '../storage/conversationHistoryOperation'

export const PROMPT_HISTORY_PAGE_SIZE = 128

export interface PromptHistorySelection {
    startIndex: number
    endIndex: number
    totalMessages: number
    messageCount: number
    resetByAllBefore: boolean
}

export interface PromptHistoryEntry {
    absoluteIndex: number
    relativeIndex: number
    message: Message
    locator?: MessageLocator
}

export interface PromptHistoryCompatibilitySnapshot {
    readonly entries: PromptHistoryEntry[]
    dispose(): void
}

export function ensurePromptHistoryEntryId(
    history: ConversationHistoryOperation,
    entry: PromptHistoryEntry,
    createId: () => string,
): Message {
    if (!entry.locator) {
        throw new Error(`Prompt history message ${entry.absoluteIndex} has no locator`)
    }
    return history.ensureMessageId(entry.locator, createId)
}

export function ensurePromptHistoryMessageId(
    message: Message,
    createId: () => string,
): string {
    const id = message.chatId || createId()
    message.chatId = id
    return id
}

export function createLivePromptHistoryCompatibilitySnapshot(
    messages: Message[],
    selection: PromptHistorySelection,
): PromptHistoryCompatibilitySnapshot {
    const entries: PromptHistoryEntry[] = []
    for (
        let absoluteIndex = selection.startIndex;
        absoluteIndex < selection.endIndex;
        absoluteIndex += 1
    ) {
        const message = messages[absoluteIndex]
        if (!message) {
            throw new RangeError(`Prompt history message ${absoluteIndex} is missing`)
        }
        if (message.disabled === true || message.disabled === 'allBefore') continue
        entries.push({
            absoluteIndex,
            relativeIndex: entries.length,
            message,
        })
    }
    return {
        entries,
        dispose() {
            entries.length = 0
        },
    }
}

export function adoptTriggeredChat(
    session: ActiveConversationSession,
    expectedVersion: number,
    expectedMessages: readonly Message[],
    replacement: Chat,
): Chat {
    return session.adoptConversationReplacement(
        expectedVersion,
        expectedMessages,
        replacement,
    )
}

export function selectPromptHistory(
    history: ConversationHistoryOperation,
    pageSize = PROMPT_HISTORY_PAGE_SIZE,
): PromptHistorySelection {
    let startIndexExclusive = history.totalMessages
    let startIndex = 0
    let messageCount = 0
    let resetByAllBefore = false

    while (startIndexExclusive > 0) {
        const page = history.scanBackward(
            startIndexExclusive,
            Math.min(pageSize, startIndexExclusive),
        )
        if (page.entries.length === 0) break
        for (const entry of page.entries) {
            if (entry.message.disabled === true) continue
            if (entry.message.disabled === 'allBefore') {
                startIndex = entry.absoluteIndex + 1
                resetByAllBefore = true
                history.assertCurrent()
                return {
                    startIndex,
                    endIndex: history.totalMessages,
                    totalMessages: history.totalMessages,
                    messageCount,
                    resetByAllBefore,
                }
            }
            messageCount += 1
        }
        startIndexExclusive = page.entries[page.entries.length - 1].absoluteIndex
    }

    history.assertCurrent()
    return {
        startIndex,
        endIndex: history.totalMessages,
        totalMessages: history.totalMessages,
        messageCount,
        resetByAllBefore,
    }
}

export function* iteratePromptHistory(
    history: ConversationHistoryOperation,
    selection: PromptHistorySelection,
    pageSize = PROMPT_HISTORY_PAGE_SIZE,
): Generator<PromptHistoryEntry> {
    let relativeIndex = 0
    for (let startIndex = selection.startIndex; startIndex < selection.endIndex;) {
        const page = history.readRange(
            startIndex,
            Math.min(pageSize, selection.endIndex - startIndex),
        )
        if (page.messages.length === 0) break
        for (let offset = 0; offset < page.messages.length; offset += 1) {
            const message = page.messages[offset]
            if (message.disabled === true || message.disabled === 'allBefore') continue
            yield {
                absoluteIndex: page.startIndex + offset,
                relativeIndex,
                message,
                locator: page.locators[offset],
            }
            relativeIndex += 1
        }
        startIndex = page.endIndex
    }
    history.assertCurrent()
}
