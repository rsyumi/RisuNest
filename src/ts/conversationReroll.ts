import type { Message } from './storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT } from './storage/persistentDataStore'
import { safeStructuredClone } from './polyfill'
import type { ConversationMutationTarget } from './conversationMutations'

export type ConversationRerollDirection = 'reroll' | 'unreroll'

export function applyConversationRerollTail(
    target: ConversationMutationTarget,
    tail: readonly Message[],
    direction: ConversationRerollDirection,
): void {
    const replacement = safeStructuredClone(tail)
    if (target.session) {
        const ignored = Math.max(0, replacement.length - target.session.totalMessages)
        const effectiveReplacement = replacement.slice(ignored)
        const position = target.session.positionAt(
            target.session.totalMessages - effectiveReplacement.length,
        )
        if (direction === 'reroll') target.session.reroll(position, effectiveReplacement)
        else target.session.replaceTail(position, effectiveReplacement)
        return
    }
    const messages = target.conversation.message
    for (let index = 0; index < replacement.length; index++) {
        messages[messages.length - replacement.length + index] = replacement[index]
    }
    target.conversation.message = messages
}

export function truncateConversationForReroll(
    target: ConversationMutationTarget,
): boolean {
    const messages = target.conversation.message
    if (messages.length === 0) return false
    let startIndex = messages.length
    const saying = messages[startIndex - 1].saying
    let sayingQuantity = 2
    while (messages[startIndex - 1].role !== 'user') {
        if (messages[startIndex - 1].saying === saying) {
            sayingQuantity -= 1
            if (sayingQuantity === 0) break
        }
        const message = messages[startIndex - 1]
        startIndex -= 1
        if (!message) return false
    }
    if (target.session) {
        target.session.reroll(target.session.positionAt(startIndex), [])
    } else {
        target.conversation.message = safeStructuredClone(messages.slice(0, startIndex))
    }
    return true
}

export function replaceConversationRerollLastData(
    target: ConversationMutationTarget,
    data: string,
    direction: ConversationRerollDirection,
): boolean {
    const absoluteIndex = target.conversation.message.length - 1
    const message = target.conversation.message[absoluteIndex]
    if (!message) return false
    if (target.session) {
        const replacement = { ...message, data }
        const position = target.session.positionAt(absoluteIndex)
        if (direction === 'reroll') target.session.reroll(position, [replacement])
        else target.session.replaceTail(position, [replacement])
    } else {
        message.data = data
    }
    return true
}

export function captureConversationRerollTail(
    target: ConversationMutationTarget,
    startIndex: number,
): Message[] {
    const count = target.conversation.message.length - startIndex
    if (
        target.session &&
        Number.isSafeInteger(startIndex) &&
        startIndex >= 0 &&
        count > 0 &&
        count <= CONVERSATION_RANGE_MAX_LIMIT
    ) {
        return target.session.readRange(startIndex, count).messages
    }
    return safeStructuredClone(target.conversation.message.slice(startIndex))
}
