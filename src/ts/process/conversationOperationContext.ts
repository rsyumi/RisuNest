import { safeStructuredClone } from '../safeStructuredClone'
import {
    ActiveConversationSession,
    cloneConversationMetadata,
    conversationContentMetadataEqual,
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
import type { WindowedConversationMutationController } from '../storage/activeWorkingSet.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT } from '../storage/persistentDataStore'

export type ConversationOperationMode = 'prefetched' | 'compatibility'

/** Only a successfully committed operation can advance a caller's captured target. */
export interface ConversationOperationCommit {
    follows(session: ActiveConversationSession | null, version: number | null, chat: Chat): boolean
    mapSessionIndex?(index: number): number | null
    remapWindowTarget?(controller: WindowedConversationMutationController, message: Message): number | null
}

export type ConversationCommitObserver = (commit: ConversationOperationCommit) => void

export interface ConversationOperationCommitOptions {
    origin?: 'display'
    /** Chat variables written with the operation, applied over the session's current values. */
    variables?: ReadonlyMap<string, string>
}

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

export function valuesEqual(left: unknown, right: unknown): boolean {
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

/** `database` with the given conversation in place of the stored one, without copying either. */
export function createConversationDatabaseView(
    database: Database,
    characterId: string,
    conversationId: string,
    chat: Chat,
): Database {
    const characterIndex = database.characters.findIndex(
        (character) => character.chaId === characterId,
    )
    if (characterIndex === -1) return database

    const character = database.characters[characterIndex]
    const conversationIndex = character.chats.findIndex(
        (conversation) => conversation.id === conversationId,
    )
    if (conversationIndex === -1) return database

    const characters = database.characters.slice()
    const chats = character.chats.slice()
    chats[conversationIndex] = chat
    characters[characterIndex] = {
        ...character,
        chats,
    }
    return {
        ...database,
        characters,
    }
}

/**
 * A copy of the conversation an operation edits while it awaits. Variable-only commits
 * made meanwhile keep it current: its commit writes only the variables it changed.
 */
export class ConversationOperationContext {
    readonly chat: Chat
    readonly mode: ConversationOperationMode

    readonly baseVersion: number
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
            this.requireContentUnchanged()
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
        return createConversationDatabaseView(
            database,
            this.session.characterId,
            this.session.conversationId,
            this.chat,
        )
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
        this.requireContentUnchanged()
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

    collectMutationBatch(
        variables?: ReadonlyMap<string, string>,
    ): ConversationMutationBatch {
        this.assertOpen()
        this.requireContentUnchanged()
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
        if (!conversationContentMetadataEqual(
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

        const metadata = this.committedMetadata(variables)
        if (!conversationMetadataEqual(this.originalMetadata, metadata)) {
            batch.push({
                type: 'update-metadata',
                metadata,
            })
        }
        return batch
    }

    private requireContentUnchanged(): void {
        if (!this.session.isContentUnchangedSince(this.baseVersion)) {
            throw new ConversationSessionStaleError(
                this.baseVersion,
                this.session.version,
            )
        }
    }

    private committedMetadata(
        variables?: ReadonlyMap<string, string>,
    ): ConversationMetadata {
        const metadata = cloneConversationMetadata(this.chat)
        for (const [key, value] of variables ?? []) {
            const scriptstate = (metadata.scriptstate ??= {}) as Record<string, unknown>
            scriptstate['$' + key] = value
        }
        return metadata
    }

    /**
     * The baseline the session diffs a commit against. Written variables are compared
     * with their live values, so each one lands even if another commit changed it meanwhile.
     */
    private commitBaseline(
        variables?: ReadonlyMap<string, string>,
    ): ConversationMetadata {
        if (!variables?.size) return this.originalMetadata
        const scriptstate: Record<string, unknown> = {
            ...(this.originalMetadata.scriptstate as Record<string, unknown> | undefined),
        }
        for (const key of variables.keys()) {
            const live = this.session.readChatVariable(key)
            if (live === undefined) delete scriptstate['$' + key]
            else scriptstate['$' + key] = live
        }
        return { ...this.originalMetadata, scriptstate }
    }

    commit(
        currentSession: ActiveConversationSession | null,
        options: ConversationOperationCommitOptions = {},
    ): ConversationMutationBatch {
        try {
            requireCurrentConversationSession(this.session, currentSession)
            const batch = this.collectMutationBatch(options.variables)
            const range = batch.find(
                (mutation): mutation is ConversationReplaceRangeBatchEntry =>
                    mutation.type === 'replace-range',
            )
            const preCommitVersion = this.session.version
            this.session.applyOperation({
                ...(options.origin ? { origin: options.origin } : {}),
                expectedVersion: this.baseVersion,
                expectedMetadata: this.commitBaseline(options.variables),
                metadata: this.committedMetadata(options.variables),
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
            this.reportCommit(preCommitVersion)
            return batch
        } finally {
            this.release()
        }
    }

    commitMetadata(
        currentSession: ActiveConversationSession | null,
        variables?: ReadonlyMap<string, string>,
    ): ConversationMutationBatch {
        try {
            requireCurrentConversationSession(this.session, currentSession)
            this.requireContentUnchanged()
            const currentMetadata = cloneConversationMetadata(this.sourceChat)
            if (
                !conversationContentMetadataEqual(
                    this.originalMetadata,
                    currentMetadata,
                )
            ) {
                throw new MessageLocatorMismatchError(
                    'Conversation operation metadata baseline changed',
                )
            }
            const metadata = this.committedMetadata(variables)
            const batch: ConversationMutationBatch = conversationMetadataEqual(
                this.originalMetadata,
                metadata,
            )
                ? []
                : [{ type: 'update-metadata', metadata }]
            const preCommitVersion = this.session.version
            this.session.applyOperation({
                expectedVersion: this.baseVersion,
                expectedMetadata: this.commitBaseline(variables),
                metadata,
            })
            this.reportCommit(preCommitVersion)
            return batch
        } finally {
            this.release()
        }
    }

    /** `preCommitVersion` may be past the base: variable-only commits do not change what a caller holds. */
    private reportCommit(preCommitVersion: number): void {
        const version = this.session.version
        this.onCommitted?.({
            follows: (session, previousVersion, chat) =>
                session === this.session &&
                previousVersion !== null &&
                previousVersion >= this.precedingGenerationVersion &&
                previousVersion <= preCommitVersion &&
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
