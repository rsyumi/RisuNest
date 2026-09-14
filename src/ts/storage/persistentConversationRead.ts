import { safeStructuredClone } from '../polyfill'
import {
    CONVERSATION_RANGE_MAX_LIMIT,
    RevisionConflictError,
    type CharacterDetail,
    type ConversationWindow,
    type DataRevision,
    type PersistentDataStore,
    type PersistentRevisionLease,
} from './persistentDataStore'
import {
    assertPinnedRevision,
    releasePersistentRevisionLease,
    withPersistentRevisionLease,
} from './persistentRecordIterator'

const REVISION_ACQUIRE_ATTEMPTS = 3

export class PersistentConversationReadCancelledError extends Error {
    constructor() {
        super('Persistent conversation read was cancelled')
        this.name = 'PersistentConversationReadCancelledError'
    }
}

export class PersistentConversationReadStaleError extends Error {
    constructor() {
        super('Persistent conversation read outlived its navigation session')
        this.name = 'PersistentConversationReadStaleError'
    }
}

export interface PersistentSelectedConversationWindow {
    revision: DataRevision
    character: CharacterDetail
    conversation: ConversationWindow | null
}

export interface PersistentConversationReadDependencies {
    store: PersistentDataStore
    flushPendingData(reason: string): Promise<void>
    getNavigationGeneration(): number
}

export interface PersistentSelectedConversationWindowQuery {
    characterId: string
    count: number
    offset: number
    reason: string
    signal?: AbortSignal
}

function validateQuery(input: PersistentSelectedConversationWindowQuery): void {
    if (typeof input.characterId !== 'string' || input.characterId.length === 0) {
        throw new RangeError('Character ID must be a nonempty string')
    }
    if (!Number.isSafeInteger(input.count) || input.count <= 0) {
        throw new RangeError('Conversation count must be a positive safe integer')
    }
    if (input.count > CONVERSATION_RANGE_MAX_LIMIT) {
        throw new RangeError(
            `Conversation count cannot exceed ${CONVERSATION_RANGE_MAX_LIMIT}`,
        )
    }
    if (!Number.isSafeInteger(input.offset) || input.offset < 0) {
        throw new RangeError('Conversation offset must be a nonnegative safe integer')
    }
}

function assertCurrent(
    dependencies: PersistentConversationReadDependencies,
    generation: number,
    signal?: AbortSignal,
): void {
    if (signal?.aborted) throw new PersistentConversationReadCancelledError()
    if (dependencies.getNavigationGeneration() !== generation) {
        throw new PersistentConversationReadStaleError()
    }
}

export async function acquireCurrentPersistentRevision(
    dependencies: PersistentConversationReadDependencies,
    generation: number,
    signal?: AbortSignal,
): Promise<PersistentRevisionLease> {
    for (let attempt = 0; attempt < REVISION_ACQUIRE_ATTEMPTS; attempt++) {
        assertCurrent(dependencies, generation, signal)
        const root = await dependencies.store.readRoot()
        assertCurrent(dependencies, generation, signal)
        try {
            const lease = await dependencies.store.acquireRevision(root.revision)
            try {
                assertCurrent(dependencies, generation, signal)
            } catch (error) {
                try {
                    await releasePersistentRevisionLease(lease)
                } catch (releaseError) {
                    console.error(
                        'Persistent conversation revision release failed after current-read validation failed',
                        releaseError,
                    )
                }
                throw error
            }
            return lease
        } catch (error) {
            if (
                !(error instanceof RevisionConflictError) ||
                attempt === REVISION_ACQUIRE_ATTEMPTS - 1
            ) {
                throw error
            }
        }
    }
    throw new Error('Unable to acquire persistent revision')
}

export async function readPinnedSelectedConversationWindow(
    dependencies: PersistentConversationReadDependencies,
    input: PersistentSelectedConversationWindowQuery,
): Promise<PersistentSelectedConversationWindow | null> {
    validateQuery(input)
    const generation = dependencies.getNavigationGeneration()
    assertCurrent(dependencies, generation, input.signal)
    await dependencies.flushPendingData(input.reason)
    assertCurrent(dependencies, generation, input.signal)
    await dependencies.store.open()
    assertCurrent(dependencies, generation, input.signal)
    const lease = await acquireCurrentPersistentRevision(dependencies, generation, input.signal)

    return withPersistentRevisionLease(lease, async (reader) => {
        assertCurrent(dependencies, generation, input.signal)
        const character = await reader.readCharacter(input.characterId)
        assertCurrent(dependencies, generation, input.signal)
        if (!character) return null
        assertPinnedRevision(reader.revision, character.revision, `Character ${input.characterId}`)
        if (character.value.chaId !== input.characterId) {
            throw new Error(`Character ${input.characterId} returned mismatched detail`)
        }

        const selectedIndex = character.value.chatPage ?? 0
        if (!Number.isSafeInteger(selectedIndex) || selectedIndex < 0) {
            throw new RangeError('Selected conversation position must be a nonnegative safe integer')
        }
        const conversations = await reader.queryConversations({
            characterId: input.characterId,
            order: 'configured',
            limit: 1,
            cursor: selectedIndex === 0 ? undefined : String(selectedIndex),
        })
        assertCurrent(dependencies, generation, input.signal)
        assertPinnedRevision(reader.revision, conversations.revision, 'Selected conversation')
        const summary = conversations.items[0]
        if (!summary) {
            return {
                revision: reader.revision,
                character: safeStructuredClone(character.value),
                conversation: null,
            }
        }
        if (summary.characterId !== input.characterId) {
            throw new Error(`Conversation ${summary.id} belongs to another character`)
        }

        const endIndex = Math.max(0, summary.messageCount - input.offset)
        const startIndex = Math.max(0, endIndex - input.count)
        const limit = endIndex - startIndex
        if (limit === 0) {
            return {
                revision: reader.revision,
                character: safeStructuredClone(character.value),
                conversation: {
                    characterId: input.characterId,
                    conversationId: summary.id,
                    messages: [],
                    startIndex,
                    endIndex,
                    totalMessages: summary.messageCount,
                    hasMoreBefore: startIndex > 0,
                    hasMoreAfter: endIndex < summary.messageCount,
                },
            }
        }

        const window = await reader.readConversationWindow({
            characterId: input.characterId,
            conversationId: summary.id,
            startIndex,
            limit,
        })
        assertCurrent(dependencies, generation, input.signal)
        if (!window) throw new Error(`Conversation ${summary.id} was not found`)
        assertPinnedRevision(reader.revision, window.revision, `Conversation ${summary.id}`)
        if (
            window.value.characterId !== input.characterId ||
            window.value.conversationId !== summary.id ||
            window.value.startIndex !== startIndex ||
            window.value.endIndex !== endIndex ||
            window.value.totalMessages !== summary.messageCount
        ) {
            throw new Error(`Conversation ${summary.id} returned a mismatched window`)
        }
        return {
            revision: reader.revision,
            character: safeStructuredClone(character.value),
            conversation: safeStructuredClone(window.value),
        }
    })
}
