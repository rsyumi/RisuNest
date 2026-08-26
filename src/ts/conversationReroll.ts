import {
    ConversationSessionInactiveError,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    MessageLocatorNotFoundError,
    type ActiveConversationSession,
    type ConversationPosition,
} from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT } from './storage/persistentDataStore'
import { safeStructuredClone } from './polyfill'
import {
    assertConversationMutationTargetCurrent,
    captureConversationMutationTarget,
    type ConversationMutationTarget,
} from './conversationMutations'

type ConversationCharacter = Database['characters'][number]

export type ConversationRerollDirection = 'reroll' | 'unreroll'

export interface ConversationRerollTransition {
    characterId: string
    conversationId: string
    character: ConversationCharacter
    conversation: Chat
    session: ActiveConversationSession | null
    sessionVersion: number | null
    messages: Message[]
    totalMessages: number
    position: ConversationPosition | null
    absoluteIndex: number
    overlayStartIndex: number
    replacement: readonly Message[]
    direction: ConversationRerollDirection
}

export interface ConversationRerollHistory {
    characterId: string
    conversationId: string
    character: ConversationCharacter
    conversation: Chat
    session: ActiveConversationSession | null
    sessionVersion: number | null
    messages: Message[]
    totalMessages: number
    entries: readonly Message[][]
    index: number
    backward: ConversationRerollTransition | null
    forward: ConversationRerollTransition | null
}

export class ConversationRerollHistoryStaleError extends Error {
    constructor() {
        super('Conversation reroll history is stale')
        this.name = 'ConversationRerollHistoryStaleError'
    }
}

export function captureConversationRerollTransition(
    target: ConversationMutationTarget,
    tail: readonly Message[],
    direction: ConversationRerollDirection,
): ConversationRerollTransition {
    assertConversationMutationTargetCurrent(target)
    const replacement = safeStructuredClone([...tail])
    const effectiveLength = Math.min(replacement.length, target.messages.length)
    const absoluteIndex = target.messages.length - effectiveLength
    return {
        characterId: target.character.chaId,
        conversationId: target.conversation.id,
        character: target.character,
        conversation: target.conversation,
        session: target.session,
        sessionVersion: target.sessionVersion,
        messages: target.messages,
        totalMessages: target.messageCount,
        position: target.session?.positionAt(absoluteIndex) ?? null,
        absoluteIndex,
        overlayStartIndex: target.messageCount - replacement.length,
        replacement,
        direction,
    }
}

export function applyConversationRerollTail(
    target: ConversationMutationTarget,
    transition: ConversationRerollTransition,
): void {
    assertConversationMutationTargetCurrent(target)
    if (!isConversationRerollTransitionCurrent(transition, target)) {
        throw new ConversationRerollHistoryStaleError()
    }
    if (target.session) {
        const ignored = transition.replacement.length -
            Math.min(transition.replacement.length, target.messages.length)
        const effectiveReplacement = transition.replacement.slice(ignored)
        if (!transition.position) throw new ConversationRerollHistoryStaleError()
        if (transition.direction === 'reroll') {
            target.session.reroll(transition.position, effectiveReplacement)
        } else {
            target.session.replaceTail(transition.position, effectiveReplacement)
        }
        return
    }
    const messages = target.conversation.message
    const replacement = safeStructuredClone(transition.replacement)
    for (let index = 0; index < replacement.length; index++) {
        messages[transition.overlayStartIndex + index] = replacement[index]
    }
    target.conversation.message = messages
}

export function createConversationRerollHistory(
    target: ConversationMutationTarget,
    tail: readonly Message[],
): ConversationRerollHistory {
    return bindConversationRerollHistory(
        [safeStructuredClone([...tail])],
        0,
        target,
    )
}

export function appendConversationRerollHistory(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    tail: readonly Message[],
): ConversationRerollHistory {
    if (!isConversationRerollHistoryOwner(history, target)) {
        throw new ConversationRerollHistoryStaleError()
    }
    const entries = [...history.entries, safeStructuredClone([...tail])]
    return bindConversationRerollHistory(entries, entries.length - 1, target)
}

export function refreshConversationRerollHistory(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
): ConversationRerollHistory | null {
    if (!isConversationRerollHistoryOwner(history, target)) return null
    return bindConversationRerollHistory(history.entries, history.index, target)
}

export function isConversationRerollHistoryCurrent(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
): boolean {
    if (!isConversationRerollHistoryOwner(history, target)) return false
    try {
        assertConversationMutationTargetCurrent(target)
    } catch {
        return false
    }
    return history.sessionVersion === target.sessionVersion &&
        history.messages === target.messages &&
        history.totalMessages === target.messageCount
}

export function moveConversationRerollHistory(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    direction: ConversationRerollDirection,
): ConversationRerollHistory | null {
    if (!isConversationRerollHistoryCurrent(history, target)) return null
    const transition = direction === 'reroll' ? history.forward : history.backward
    if (!transition) return history
    try {
        applyConversationRerollTail(target, transition)
        const refreshedTarget = captureConversationMutationTarget(
            target.character,
            target.conversation,
            target.session,
        )
        const nextIndex = history.index + (direction === 'reroll' ? 1 : -1)
        return bindConversationRerollHistory(history.entries, nextIndex, refreshedTarget)
    } catch (error) {
        if (isStaleRerollError(error)) return null
        throw error
    }
}

export function truncateConversationForReroll(
    target: ConversationMutationTarget,
): boolean {
    assertConversationMutationTargetCurrent(target)
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
    assertConversationMutationTargetCurrent(target)
    const absoluteIndex = target.conversation.message.length - 1
    const message = target.conversation.message[absoluteIndex]
    if (!message) return false
    if (target.session) {
        const transition = captureConversationRerollTransition(
            target,
            [{ ...message, data }],
            direction,
        )
        applyConversationRerollTail(target, transition)
    } else {
        message.data = data
    }
    return true
}

export function captureConversationRerollTail(
    target: ConversationMutationTarget,
    startIndex: number,
): Message[] {
    assertConversationMutationTargetCurrent(target)
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

function bindConversationRerollHistory(
    entries: readonly Message[][],
    index: number,
    target: ConversationMutationTarget,
): ConversationRerollHistory {
    assertConversationMutationTargetCurrent(target)
    return {
        characterId: target.character.chaId,
        conversationId: target.conversation.id,
        character: target.character,
        conversation: target.conversation,
        session: target.session,
        sessionVersion: target.sessionVersion,
        messages: target.messages,
        totalMessages: target.messageCount,
        entries,
        index,
        backward: index > 0
            ? captureConversationRerollTransition(target, entries[index - 1], 'unreroll')
            : null,
        forward: index < entries.length - 1
            ? captureConversationRerollTransition(target, entries[index + 1], 'reroll')
            : null,
    }
}

function isConversationRerollHistoryOwner(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
): boolean {
    return history.characterId === target.character.chaId &&
        history.conversationId === target.conversation.id &&
        history.character === target.character &&
        history.conversation === target.conversation &&
        history.session === target.session
}

function isConversationRerollTransitionCurrent(
    transition: ConversationRerollTransition,
    target: ConversationMutationTarget,
): boolean {
    return transition.characterId === target.character.chaId &&
        transition.conversationId === target.conversation.id &&
        transition.character === target.character &&
        transition.conversation === target.conversation &&
        transition.session === target.session &&
        transition.sessionVersion === target.sessionVersion &&
        transition.messages === target.messages &&
        transition.totalMessages === target.messageCount
}

function isStaleRerollError(error: unknown): boolean {
    return error instanceof ConversationRerollHistoryStaleError ||
        error instanceof ConversationSessionInactiveError ||
        error instanceof ConversationSessionStaleError ||
        error instanceof MessageLocatorMismatchError ||
        error instanceof MessageLocatorNotFoundError
}
