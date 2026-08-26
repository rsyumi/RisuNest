import { safeStructuredClone } from '../polyfill'
import type { Chat, Message } from './database.svelte'
import { ConversationNotFoundError } from './activeConversationSession'
import {
    CONVERSATION_RANGE_MAX_LIMIT,
    type ConversationWindow,
    type DataRevision,
    type PersistentDataStore,
    type PersistentRevisionLease,
} from './persistentDataStore'
import {
    assertPinnedRevision,
    releasePersistentRevisionLease,
} from './persistentRecordIterator'

export class PinnedPersistentConversationClosedError extends Error {
    constructor() {
        super('Pinned persistent conversation is closed')
        this.name = 'PinnedPersistentConversationClosedError'
    }
}

export class PinnedPersistentConversationCancelledError extends Error {
    constructor() {
        super('Pinned persistent conversation read was cancelled')
        this.name = 'PinnedPersistentConversationCancelledError'
    }
}

export interface PinnedPersistentConversationEntry {
    absoluteIndex: number
    message: Message
}

export interface PinnedPersistentConversationOptions {
    store: PersistentDataStore
    characterId: string
    conversationId: string
    revision: DataRevision
    signal?: AbortSignal
}

export interface PinnedPersistentBackwardOptions {
    startIndexExclusive?: number
    pageSize?: number
    signal?: AbortSignal
}

export interface StrictConversationReplaceRangeOptions {
    store: PersistentDataStore
    characterId: string
    conversationId: string
    expectedRevision: DataRevision
    start: number
    deleteCount: number
    messages: readonly Message[]
    conversation?: Omit<Chat, 'message'>
    signal?: AbortSignal
}

function validateIndex(value: number, name: string): void {
    if (!Number.isSafeInteger(value) || value < 0) {
        throw new RangeError(`${name} must be a nonnegative safe integer`)
    }
}

function validateLimit(value: number, name = 'Conversation range limit'): void {
    if (!Number.isSafeInteger(value) || value <= 0) {
        throw new RangeError(`${name} must be a positive safe integer`)
    }
    if (value > CONVERSATION_RANGE_MAX_LIMIT) {
        throw new RangeError(`${name} cannot exceed ${CONVERSATION_RANGE_MAX_LIMIT}`)
    }
}

function assertNotCancelled(signal?: AbortSignal): void {
    if (signal?.aborted) throw new PinnedPersistentConversationCancelledError()
}

export class DisposableConversationSnapshot {
    private disposed = false

    constructor(readonly messages: Message[]) {}

    get residentMessageCount(): number {
        return this.messages.length
    }

    dispose(): void {
        if (this.disposed) return
        this.disposed = true
        this.messages.splice(0)
    }
}

export class PinnedPersistentConversation {
    readonly revision: DataRevision
    readonly totalMessages: number

    private closed = false

    constructor(
        private readonly lease: PersistentRevisionLease,
        readonly characterId: string,
        readonly conversationId: string,
        totalMessages: number,
        private readonly defaultSignal?: AbortSignal,
    ) {
        this.revision = lease.revision
        this.totalMessages = totalMessages
    }

    async readRange(
        startIndex: number,
        limit: number,
        signal = this.defaultSignal,
    ): Promise<ConversationWindow> {
        this.assertOpen(signal)
        validateIndex(startIndex, 'Conversation range startIndex')
        validateLimit(limit)
        const result = await this.lease.readConversationWindow({
            characterId: this.characterId,
            conversationId: this.conversationId,
            startIndex,
            limit,
        })
        this.assertOpen(signal)
        if (!result) {
            throw new ConversationNotFoundError(this.characterId, this.conversationId)
        }
        assertPinnedRevision(this.revision, result.revision, `Conversation ${this.conversationId}`)
        this.assertWindow(result.value, startIndex, limit)
        return safeStructuredClone(result.value)
    }

    async *iterateBackward(
        options: PinnedPersistentBackwardOptions = {},
    ): AsyncGenerator<PinnedPersistentConversationEntry> {
        const pageSize = options.pageSize ?? 128
        validateLimit(pageSize, 'Conversation backward pageSize')
        const requestedStart = options.startIndexExclusive ?? this.totalMessages
        validateIndex(requestedStart, 'Conversation backward startIndexExclusive')
        let cursor = Math.min(requestedStart, this.totalMessages)
        while (cursor > 0) {
            this.assertOpen(options.signal ?? this.defaultSignal)
            const count = Math.min(pageSize, cursor)
            const startIndex = cursor - count
            const window = await this.readRange(
                startIndex,
                count,
                options.signal ?? this.defaultSignal,
            )
            for (let offset = window.messages.length - 1; offset >= 0; offset--) {
                this.assertOpen(options.signal ?? this.defaultSignal)
                yield {
                    absoluteIndex: window.startIndex + offset,
                    message: safeStructuredClone(window.messages[offset]),
                }
            }
            cursor = startIndex
        }
    }

    async materializeCompatibilitySnapshot(
        pageSize = 128,
        signal = this.defaultSignal,
    ): Promise<DisposableConversationSnapshot> {
        validateLimit(pageSize, 'Conversation snapshot pageSize')
        const messages: Message[] = []
        for (let startIndex = 0; startIndex < this.totalMessages; startIndex += pageSize) {
            this.assertOpen(signal)
            const window = await this.readRange(
                startIndex,
                Math.min(pageSize, this.totalMessages - startIndex),
                signal,
            )
            messages.push(...window.messages)
        }
        this.assertOpen(signal)
        return new DisposableConversationSnapshot(messages)
    }

    async close(): Promise<void> {
        if (this.closed) return
        this.closed = true
        await releasePersistentRevisionLease(this.lease)
    }

    private assertOpen(signal?: AbortSignal): void {
        if (this.closed) throw new PinnedPersistentConversationClosedError()
        assertNotCancelled(signal)
    }

    private assertWindow(
        window: ConversationWindow,
        requestedStart: number,
        limit: number,
    ): void {
        const expectedStart = Math.min(requestedStart, this.totalMessages)
        const expectedEnd = Math.min(this.totalMessages, expectedStart + limit)
        if (
            window.characterId !== this.characterId ||
            window.conversationId !== this.conversationId ||
            window.startIndex !== expectedStart ||
            window.endIndex !== expectedEnd ||
            window.totalMessages !== this.totalMessages ||
            window.messages.length !== expectedEnd - expectedStart ||
            window.hasMoreBefore !== (expectedStart > 0) ||
            window.hasMoreAfter !== (expectedEnd < this.totalMessages)
        ) {
            throw new Error(`Conversation ${this.conversationId} returned mismatched range evidence`)
        }
    }
}

export async function openPinnedPersistentConversation(
    options: PinnedPersistentConversationOptions,
): Promise<PinnedPersistentConversation> {
    assertNotCancelled(options.signal)
    await options.store.open()
    assertNotCancelled(options.signal)
    const lease = await options.store.acquireRevision(options.revision)
    try {
        assertNotCancelled(options.signal)
        const probe = await lease.readConversationWindow({
            characterId: options.characterId,
            conversationId: options.conversationId,
            limit: 1,
        })
        assertNotCancelled(options.signal)
        if (!probe) {
            throw new ConversationNotFoundError(options.characterId, options.conversationId)
        }
        assertPinnedRevision(lease.revision, probe.revision, `Conversation ${options.conversationId}`)
        if (
            probe.value.characterId !== options.characterId ||
            probe.value.conversationId !== options.conversationId ||
            probe.value.endIndex !== probe.value.totalMessages ||
            probe.value.startIndex !== Math.max(0, probe.value.totalMessages - 1) ||
            probe.value.messages.length !== Math.min(1, probe.value.totalMessages)
        ) {
            throw new Error(`Conversation ${options.conversationId} returned mismatched range evidence`)
        }
        return new PinnedPersistentConversation(
            lease,
            options.characterId,
            options.conversationId,
            probe.value.totalMessages,
            options.signal,
        )
    } catch (error) {
        await releasePersistentRevisionLease(lease)
        throw error
    }
}

export async function commitStrictConversationReplaceRange(
    options: StrictConversationReplaceRangeOptions,
): Promise<{ revision: DataRevision }> {
    validateIndex(options.start, 'Conversation replace-range start')
    validateIndex(options.deleteCount, 'Conversation replace-range deleteCount')
    const reader = await openPinnedPersistentConversation({
        store: options.store,
        characterId: options.characterId,
        conversationId: options.conversationId,
        revision: options.expectedRevision,
        signal: options.signal,
    })
    try {
        if (options.start > reader.totalMessages) {
            throw new RangeError('Conversation replace-range start exceeds the current message count')
        }
        if (options.deleteCount > reader.totalMessages - options.start) {
            throw new RangeError('Conversation replace-range deleteCount exceeds the current message count')
        }
    } finally {
        await reader.close()
    }
    assertNotCancelled(options.signal)
    return options.store.commit({
        expectedRevision: options.expectedRevision,
        conversations: [{
            type: 'replace-range',
            characterId: options.characterId,
            conversationId: options.conversationId,
            start: options.start,
            deleteCount: options.deleteCount,
            messages: safeStructuredClone([...options.messages]),
            ...(options.conversation === undefined
                ? {}
                : { conversation: safeStructuredClone(options.conversation) }),
        }],
    })
}
