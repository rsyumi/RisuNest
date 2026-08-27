import {
    ConversationSessionInactiveError,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    MessageLocatorNotFoundError,
} from './storage/activeConversationSession'
import type { Message } from './storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT } from './storage/persistentDataStore'
import { safeStructuredClone } from './polyfill'
import {
    assertConversationMutationTargetCurrent,
    captureConversationMutationTarget,
    type ConversationMutationTarget,
} from './conversationMutations'

export type ConversationRerollDirection = 'reroll' | 'unreroll'

export interface ConversationRerollTransition {
    characterId: string
    conversationId: string
    totalMessages: number
    evidenceStartIndex: number
    evidence: readonly Message[]
    absoluteIndex: number
    overlayStartIndex: number
    replacement: readonly Message[]
    direction: ConversationRerollDirection
}

export interface ConversationRerollHistory {
    characterId: string
    conversationId: string
    navigationGeneration: number
    totalMessages: number
    evidenceStartIndex: number
    evidence: readonly Message[]
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
    const evidence = captureRerollEvidence(target, Math.max(1, effectiveLength))
    return {
        characterId: target.character.chaId,
        conversationId: target.conversation.id,
        totalMessages: target.messageCount,
        evidenceStartIndex: evidence.startIndex,
        evidence: evidence.messages,
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
        const position = target.session.positionAt(transition.absoluteIndex)
        if (transition.direction === 'reroll') {
            target.session.reroll(position, effectiveReplacement)
        } else {
            target.session.replaceTail(position, effectiveReplacement)
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
    navigationGeneration = 0,
): ConversationRerollHistory {
    return bindConversationRerollHistory(
        [safeStructuredClone([...tail])],
        0,
        target,
        navigationGeneration,
    )
}

export function appendConversationRerollHistory(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    tail: readonly Message[],
    previousTarget: ConversationMutationTarget = target,
): ConversationRerollHistory {
    if (
        !isConversationRerollHistoryOwner(history, target) ||
        !isConversationRerollHistoryEvidenceCurrent(
            history,
            previousTarget,
            previousTarget === target,
        )
    ) {
        throw new ConversationRerollHistoryStaleError()
    }
    const entries = [...history.entries, safeStructuredClone([...tail])]
    return bindConversationRerollHistory(
        entries,
        entries.length - 1,
        target,
        history.navigationGeneration,
    )
}

export function refreshConversationRerollHistory(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    previousTarget: ConversationMutationTarget = target,
): ConversationRerollHistory | null {
    if (
        !isConversationRerollHistoryOwner(history, target) ||
        !isConversationRerollHistoryEvidenceCurrent(
            history,
            previousTarget,
            previousTarget === target,
        )
    ) return null
    return bindConversationRerollHistory(
        history.entries,
        history.index,
        target,
        history.navigationGeneration,
    )
}

export function isConversationRerollHistoryCurrent(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    navigationGeneration = history.navigationGeneration,
): boolean {
    if (
        history.navigationGeneration !== navigationGeneration
        || !isConversationRerollHistoryOwner(history, target)
    ) return false
    try {
        assertConversationMutationTargetCurrent(target)
    } catch {
        return false
    }
    return isConversationRerollHistoryEvidenceCurrent(history, target)
}

export function moveConversationRerollHistory(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    direction: ConversationRerollDirection,
): ConversationRerollHistory | null {
    if (!isConversationRerollHistoryCurrent(history, target)) return null
    const available = direction === 'reroll' ? history.forward : history.backward
    if (!available) return history
    try {
        const nextIndex = history.index + (direction === 'reroll' ? 1 : -1)
        const transition = captureConversationRerollTransition(
            target,
            history.entries[nextIndex],
            direction,
        )
        applyConversationRerollTail(target, transition)
        const refreshedTarget = captureConversationMutationTarget(
            target.character,
            target.conversation,
            target.session,
        )
        return bindConversationRerollHistory(
            history.entries,
            nextIndex,
            refreshedTarget,
            history.navigationGeneration,
        )
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
    navigationGeneration: number,
): ConversationRerollHistory {
    assertConversationMutationTargetCurrent(target)
    const evidenceLength = Math.max(1, ...entries.map((entry) => entry.length))
    const evidence = captureRerollEvidence(target, evidenceLength)
    return {
        characterId: target.character.chaId,
        conversationId: target.conversation.id,
        navigationGeneration,
        totalMessages: target.messageCount,
        evidenceStartIndex: evidence.startIndex,
        evidence: evidence.messages,
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
        history.conversationId === target.conversation.id
}

function isConversationRerollTransitionCurrent(
    transition: ConversationRerollTransition,
    target: ConversationMutationTarget,
): boolean {
    return transition.characterId === target.character.chaId &&
        transition.conversationId === target.conversation.id &&
        transition.totalMessages === target.messageCount &&
        isRerollEvidenceCurrent(
            transition.evidenceStartIndex,
            transition.evidence,
            target,
        )
}

function isConversationRerollHistoryEvidenceCurrent(
    history: ConversationRerollHistory,
    target: ConversationMutationTarget,
    requireCurrent = true,
): boolean {
    if (!isConversationRerollHistoryOwner(history, target)) return false
    if (requireCurrent) {
        try {
            assertConversationMutationTargetCurrent(target)
        } catch {
            return false
        }
    }
    return history.totalMessages === target.messageCount &&
        isRerollEvidenceCurrent(
            history.evidenceStartIndex,
            history.evidence,
            target,
            requireCurrent,
        )
}

function isRerollEvidenceCurrent(
    startIndex: number,
    evidence: readonly Message[],
    target: ConversationMutationTarget,
    requireCurrent = true,
): boolean {
    if (startIndex !== target.messageCount - evidence.length) return false
    const current = requireCurrent
        ? captureConversationRerollTail(target, startIndex)
        : safeStructuredClone(target.messages.slice(startIndex))
    return exactValueEqual(current, evidence)
}

function captureRerollEvidence(
    target: ConversationMutationTarget,
    requestedLength: number,
): { startIndex: number; messages: Message[] } {
    const length = Math.min(
        target.messageCount,
        CONVERSATION_RANGE_MAX_LIMIT,
        Math.max(1, requestedLength),
    )
    const startIndex = target.messageCount - length
    return {
        startIndex,
        messages: captureConversationRerollTail(target, startIndex),
    }
}

function exactValueEqual(left: unknown, right: unknown): boolean {
    if (Object.is(left, right)) return true
    if (typeof left !== 'object' || left === null || typeof right !== 'object' || right === null) {
        return false
    }
    if (Array.isArray(left) || Array.isArray(right)) {
        if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
            return false
        }
        return left.every((value, index) => exactValueEqual(value, right[index]))
    }
    const leftRecord = left as Record<string, unknown>
    const rightRecord = right as Record<string, unknown>
    const leftKeys = Object.keys(leftRecord)
    const rightKeys = Object.keys(rightRecord)
    return leftKeys.length === rightKeys.length &&
        leftKeys.every((key) => Object.hasOwn(rightRecord, key) &&
            exactValueEqual(leftRecord[key], rightRecord[key]))
}

function isStaleRerollError(error: unknown): boolean {
    return error instanceof ConversationRerollHistoryStaleError ||
        error instanceof ConversationSessionInactiveError ||
        error instanceof ConversationSessionStaleError ||
        error instanceof MessageLocatorMismatchError ||
        error instanceof MessageLocatorNotFoundError
}
