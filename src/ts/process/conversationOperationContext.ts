import { safeStructuredClone } from '../polyfill'
import {
    ActiveConversationSession,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    requireCurrentConversationSession,
    type ActiveConversationPin,
    type ConversationPosition,
} from '../storage/activeConversationSession'
import type { Chat, Database, Message } from '../storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT } from '../storage/persistentDataStore'

export type ConversationOperationMode = 'prefetched' | 'compatibility'

export interface ConversationReplaceRangeBatchEntry {
    type: 'replace-range'
    startIndex: number
    deleteCount: number
    messages: Message[]
    position: ConversationPosition
}

export type ConversationMutationBatch = readonly ConversationReplaceRangeBatchEntry[]

function valuesEqual(left: unknown, right: unknown): boolean {
    if (Object.is(left, right)) return true
    if (typeof left !== typeof right || left === null || right === null) return false
    if (Array.isArray(left) || Array.isArray(right)) {
        if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
            return false
        }
        return left.every((value, index) => valuesEqual(value, right[index]))
    }
    if (typeof left !== 'object') return false
    const leftRecord = left as Record<string, unknown>
    const rightRecord = right as Record<string, unknown>
    const leftKeys = Object.keys(leftRecord)
    const rightKeys = Object.keys(rightRecord)
    if (leftKeys.length !== rightKeys.length) return false
    return leftKeys.every((key) =>
        Object.prototype.hasOwnProperty.call(rightRecord, key) &&
        valuesEqual(leftRecord[key], rightRecord[key]),
    )
}

export class ConversationOperationContext {
    readonly baseVersion: number
    readonly chat: Chat
    readonly mode: ConversationOperationMode

    private readonly originalMessages: Message[]
    private readonly pin: ActiveConversationPin
    private released = false

    constructor(
        private readonly session: ActiveConversationSession,
        sourceChat: Chat,
    ) {
        if (session.materializeCompatibilityArray() !== sourceChat.message) {
            throw new Error('Conversation operation source is not the active session chat')
        }

        this.baseVersion = session.version
        const totalMessages = session.totalMessages
        const pinReason = totalMessages <= CONVERSATION_RANGE_MAX_LIMIT
            ? 'transaction'
            : 'compatibility'
        this.mode = pinReason === 'transaction' ? 'prefetched' : 'compatibility'
        this.pin = session.acquirePin(pinReason)

        try {
            const messages = totalMessages === 0
                ? []
                : this.mode === 'prefetched'
                    ? session.readRange(0, totalMessages).messages
                    : safeStructuredClone(session.materializeCompatibilityArray())
            if (session.version !== this.baseVersion) {
                throw new ConversationSessionStaleError(this.baseVersion, session.version)
            }
            this.originalMessages = safeStructuredClone(messages)
            this.chat = safeStructuredClone(sourceChat)
            this.chat.message = messages
        } catch (error) {
            this.pin.release()
            this.released = true
            throw error
        }
    }

    createDatabaseView(database: Database): Database {
        const characterIndex = database.characters.findIndex(
            (character) => character.chaId === this.session.characterId,
        )
        if (characterIndex === -1) return database

        const character = database.characters[characterIndex]
        const conversationIndex = character.chats.findIndex(
            (conversation) => conversation.id === this.session.conversationId,
        )
        if (conversationIndex === -1) return database

        const characters = database.characters.slice()
        const chats = character.chats.slice()
        chats[conversationIndex] = this.chat
        characters[characterIndex] = {
            ...character,
            chats,
        }
        return {
            ...database,
            characters,
        }
    }

    hasPendingMutations(): boolean {
        this.assertOpen()
        return !valuesEqual(this.originalMessages, this.chat.message)
    }

    collectMutationBatch(): ConversationMutationBatch {
        this.assertOpen()
        if (this.session.version !== this.baseVersion) {
            throw new ConversationSessionStaleError(
                this.baseVersion,
                this.session.version,
            )
        }
        if (!valuesEqual(
            this.originalMessages,
            this.session.materializeCompatibilityArray(),
        )) {
            throw new MessageLocatorMismatchError(
                'Conversation operation baseline changed without a session command',
            )
        }
        const nextMessages = this.chat.message
        let startIndex = 0
        while (
            startIndex < this.originalMessages.length &&
            startIndex < nextMessages.length &&
            valuesEqual(this.originalMessages[startIndex], nextMessages[startIndex])
        ) {
            startIndex += 1
        }

        if (
            startIndex === this.originalMessages.length &&
            startIndex === nextMessages.length
        ) return []

        let originalEnd = this.originalMessages.length
        let nextEnd = nextMessages.length
        while (
            originalEnd > startIndex &&
            nextEnd > startIndex &&
            valuesEqual(this.originalMessages[originalEnd - 1], nextMessages[nextEnd - 1])
        ) {
            originalEnd -= 1
            nextEnd -= 1
        }

        return [{
            type: 'replace-range',
            startIndex,
            deleteCount: originalEnd - startIndex,
            messages: safeStructuredClone(nextMessages.slice(startIndex, nextEnd)),
            position: this.session.positionAt(startIndex),
        }]
    }

    commit(currentSession: ActiveConversationSession | null): ConversationMutationBatch {
        try {
            requireCurrentConversationSession(this.session, currentSession)
            if (this.session.version !== this.baseVersion) {
                throw new ConversationSessionStaleError(
                    this.baseVersion,
                    this.session.version,
                )
            }
            const batch = this.collectMutationBatch()
            for (const mutation of batch) {
                this.session.replaceRange(
                    mutation.position,
                    mutation.deleteCount,
                    mutation.messages,
                )
            }
            return batch
        } finally {
            this.release()
        }
    }

    release(): void {
        if (this.released) return
        this.released = true
        this.pin.release()
    }

    private assertOpen(): void {
        if (this.released) throw new Error('Conversation operation context is released')
    }
}

export function createConversationOperationContext(
    session: ActiveConversationSession,
    sourceChat: Chat,
): ConversationOperationContext {
    return new ConversationOperationContext(session, sourceChat)
}
