import { safeStructuredClone } from '../polyfill'
import {
    ActiveConversationSession,
    cloneConversationMetadata,
    conversationMetadataEqual,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    requireCurrentConversationSession,
    type ActiveConversationPin,
    type ConversationMetadata,
    type ConversationPosition,
    type MessageLocator,
} from '../storage/activeConversationSession'
import type { Chat, Database, Message } from '../storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT } from '../storage/persistentDataStore'

export type ConversationOperationMode = 'prefetched' | 'compatibility'

/** Only a successfully committed operation can advance a caller's captured target. */
export interface ConversationOperationCommit {
    follows(session: ActiveConversationSession | null, version: number | null, chat: Chat): boolean
}

export type ConversationCommitObserver = (commit: ConversationOperationCommit) => void

export interface ConversationReplaceRangeBatchEntry {
    type: 'replace-range'
    startIndex: number
    deleteCount: number
    messages: Message[]
    position: ConversationPosition
}

export interface ConversationMetadataBatchEntry {
    type: 'update-metadata'
    metadata: ConversationMetadata
}

export type ConversationMutationBatch = readonly (
    | ConversationReplaceRangeBatchEntry
    | ConversationMetadataBatchEntry
)[]

const CONVERSATION_OPERATION_PREFETCH_MAX_BYTES = 4 * 1024 * 1024

function exceedsCloneBudget(value: unknown, maxBytes: number): boolean {
    let bytes = 0
    const seen = new WeakSet<object>()
    const visit = (current: unknown): boolean => {
        if (typeof current === 'string') {
            bytes += current.length * 2
            return bytes > maxBytes
        }
        if (typeof current === 'number' || typeof current === 'bigint') bytes += 8
        else if (typeof current === 'boolean') bytes += 4
        else if (current === null || current === undefined) bytes += 1
        else if (typeof current === 'object') {
            if (seen.has(current)) return false
            seen.add(current)
            for (const [key, child] of Object.entries(current)) {
                bytes += key.length * 2
                if (bytes > maxBytes || visit(child)) return true
            }
        }
        return bytes > maxBytes
    }
    return visit(value)
}

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

    private readonly originalMetadata: ConversationMetadata
    private readonly originalMessages: Message[]
    private readonly sourceMessages: Message[]
    private readonly sourceMessageIdentities: readonly Message[]
    private readonly pin: ActiveConversationPin
    private readonly precedingGenerationVersion: number
    private released = false

    get characterId(): string {
        return this.session.characterId
    }

    get conversationId(): string {
        return this.session.conversationId
    }

    constructor(
        private readonly session: ActiveConversationSession,
        private readonly sourceChat: Chat,
        private readonly onCommitted?: ConversationCommitObserver,
    ) {
        if (session.materializeCompatibilityArray() !== sourceChat.message) {
            throw new Error(
                'Conversation operation source is not the active session chat',
            )
        }

        this.baseVersion = session.version
        this.precedingGenerationVersion = session.generationInvalidationVersion
        this.sourceMessages = sourceChat.message
        this.sourceMessageIdentities = [...sourceChat.message]
        const totalMessages = session.totalMessages
        const pinReason =
            totalMessages <= CONVERSATION_RANGE_MAX_LIMIT &&
            !exceedsCloneBudget(
                sourceChat,
                CONVERSATION_OPERATION_PREFETCH_MAX_BYTES,
            )
                ? 'transaction'
                : 'compatibility'
        this.mode = pinReason === 'transaction' ? 'prefetched' : 'compatibility'
        this.pin = session.acquirePin(pinReason)

        const compatibilitySnapshot =
            this.mode === 'compatibility'
                ? session.materializeCompatibilitySnapshot()
                : null
        try {
            const messages =
                totalMessages === 0
                    ? []
                    : this.mode === 'prefetched'
                      ? session.readRange(0, totalMessages).messages
                      : compatibilitySnapshot!.takeMessages()
            if (session.version !== this.baseVersion) {
                throw new ConversationSessionStaleError(
                    this.baseVersion,
                    session.version,
                )
            }
            this.originalMessages = messages
            this.originalMetadata = cloneConversationMetadata(sourceChat)
            this.chat = {
                ...safeStructuredClone(this.originalMetadata),
                message: safeStructuredClone(messages),
            } as unknown as Chat
        } catch (error) {
            this.pin.release()
            this.released = true
            throw error
        } finally {
            compatibilitySnapshot?.dispose()
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
        return !valuesEqual(this.originalMessages, this.chat.message) ||
            !conversationMetadataEqual(
                this.originalMetadata,
                cloneConversationMetadata(this.chat),
            )
    }

    hasPendingMetadataMutations(): boolean {
        this.assertOpen()
        return !conversationMetadataEqual(
            this.originalMetadata,
            cloneConversationMetadata(this.chat),
        )
    }

    adoptMessageId(locator: MessageLocator, messageId: string): void {
        this.assertOpen()
        if (!messageId) throw new Error('Cannot adopt an empty message ID')
        if (this.session.version !== this.baseVersion) {
            throw new ConversationSessionStaleError(
                this.baseVersion,
                this.session.version,
            )
        }
        const absoluteIndex = this.session.resolveLocator(locator)
        const liveMessage = this.session.resolveMessage(locator)
        if (liveMessage.chatId !== messageId) {
            throw new MessageLocatorMismatchError(
                'Prompt message ID no longer matches its locator',
            )
        }
        const originalMessage = this.originalMessages[absoluteIndex]
        const projectedMessage = this.chat.message[absoluteIndex]
        if (!originalMessage) {
            throw new MessageLocatorMismatchError(
                'Prompt message ID is outside the operation baseline',
            )
        }
        if (originalMessage.chatId === messageId) return
        if (
            this.chat.message.length !== this.originalMessages.length ||
            !projectedMessage ||
            !valuesEqual(originalMessage, projectedMessage)
        ) {
            throw new MessageLocatorMismatchError(
                'Prompt message ID cannot be projected after a structural mutation',
            )
        }
        if (originalMessage.chatId || (
            projectedMessage.chatId && projectedMessage.chatId !== messageId
        )) {
            throw new MessageLocatorMismatchError(
                'Prompt message ID changed from a non-empty baseline',
            )
        }
        const expectedLiveMessage = {
            ...safeStructuredClone(originalMessage),
            chatId: messageId,
        }
        if (!valuesEqual(expectedLiveMessage, liveMessage)) {
            throw new MessageLocatorMismatchError(
                'Prompt message changed while adopting its generated ID',
            )
        }
        originalMessage.chatId = messageId
        projectedMessage.chatId = messageId
    }

    collectMutationBatch(): ConversationMutationBatch {
        this.assertOpen()
        if (this.session.version !== this.baseVersion) {
            throw new ConversationSessionStaleError(
                this.baseVersion,
                this.session.version,
            )
        }
        const liveMessages = this.session.materializeCompatibilityArray()
        if (
            liveMessages !== this.sourceMessages ||
            liveMessages.length !== this.sourceMessageIdentities.length ||
            liveMessages.some(
                (message, index) => message !== this.sourceMessageIdentities[index],
            ) ||
            !valuesEqual(this.originalMessages, liveMessages)
        ) {
            throw new MessageLocatorMismatchError(
                'Conversation operation baseline changed without a session command',
            )
        }
        if (!conversationMetadataEqual(
            this.originalMetadata,
            cloneConversationMetadata(this.sourceChat),
        )) {
            throw new MessageLocatorMismatchError(
                'Conversation operation metadata baseline changed',
            )
        }
        const nextMessages = this.chat.message
        const batch: (
            | ConversationReplaceRangeBatchEntry
            | ConversationMetadataBatchEntry
        )[] = []
        let startIndex = 0
        while (
            startIndex < this.originalMessages.length &&
            startIndex < nextMessages.length &&
            valuesEqual(this.originalMessages[startIndex], nextMessages[startIndex])
        ) {
            startIndex += 1
        }

        if (
            startIndex !== this.originalMessages.length ||
            startIndex !== nextMessages.length
        ) {
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

            batch.push({
                type: 'replace-range',
                startIndex,
                deleteCount: originalEnd - startIndex,
                messages: safeStructuredClone(nextMessages.slice(startIndex, nextEnd)),
                position: this.session.positionAt(startIndex),
            })
        }

        const metadata = cloneConversationMetadata(this.chat)
        if (!conversationMetadataEqual(this.originalMetadata, metadata)) {
            batch.push({
                type: 'update-metadata',
                metadata,
            })
        }
        return batch
    }

    commit(
        currentSession: ActiveConversationSession | null,
        options: { origin?: 'display' } = {},
    ): ConversationMutationBatch {
        try {
            requireCurrentConversationSession(this.session, currentSession)
            if (this.session.version !== this.baseVersion) {
                throw new ConversationSessionStaleError(
                    this.baseVersion,
                    this.session.version,
                )
            }
            const batch = this.collectMutationBatch()
            const range = batch.find(
                (mutation): mutation is ConversationReplaceRangeBatchEntry =>
                    mutation.type === 'replace-range',
            )
            const metadata = batch.find(
                (mutation): mutation is ConversationMetadataBatchEntry =>
                    mutation.type === 'update-metadata',
            )
            this.session.applyOperation({
                ...options,
                expectedVersion: this.baseVersion,
                expectedMetadata: this.originalMetadata,
                metadata: metadata?.metadata ?? this.originalMetadata,
                ...(range === undefined
                    ? {}
                    : {
                          range: {
                              position: range.position,
                              deleteCount: range.deleteCount,
                              messages: range.messages,
                          },
                      }),
            })
            this.reportCommit(this.baseVersion + (batch.length > 0 ? 1 : 0))
            return batch
        } finally {
            this.release()
        }
    }

    commitMetadata(
        currentSession: ActiveConversationSession | null,
    ): ConversationMutationBatch {
        try {
            requireCurrentConversationSession(this.session, currentSession)
            if (this.session.version !== this.baseVersion) {
                throw new ConversationSessionStaleError(
                    this.baseVersion,
                    this.session.version,
                )
            }
            const currentMetadata = cloneConversationMetadata(this.sourceChat)
            if (
                !conversationMetadataEqual(
                    this.originalMetadata,
                    currentMetadata,
                )
            ) {
                throw new MessageLocatorMismatchError(
                    'Conversation operation metadata baseline changed',
                )
            }
            const metadata = cloneConversationMetadata(this.chat)
            const batch: ConversationMutationBatch = conversationMetadataEqual(
                this.originalMetadata,
                metadata,
            )
                ? []
                : [{ type: 'update-metadata', metadata }]
            this.session.applyOperation({
                expectedVersion: this.baseVersion,
                expectedMetadata: this.originalMetadata,
                metadata,
            })
            this.reportCommit(this.baseVersion + (batch.length > 0 ? 1 : 0))
            return batch
        } finally {
            this.release()
        }
    }

    private reportCommit(version: number): void {
        this.onCommitted?.({
            follows: (session, previousVersion, chat) =>
                session === this.session &&
                previousVersion !== null &&
                previousVersion >= this.precedingGenerationVersion &&
                previousVersion <= this.baseVersion &&
                chat === this.sourceChat &&
                session.isActive &&
                session.version === version &&
                session.materializeCompatibilityArray() === chat.message,
        })
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
    onCommitted?: ConversationCommitObserver,
): ConversationOperationContext {
    return new ConversationOperationContext(session, sourceChat, onCommitted)
}
