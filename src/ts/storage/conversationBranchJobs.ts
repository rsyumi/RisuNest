import type { Chat, Message } from './database.svelte'
import {
    CONVERSATION_RANGE_MAX_LIMIT,
    type DataRevision,
    type PersistentDataStore,
} from './persistentDataStore'
import {
    assertPinnedRevision,
    withPersistentRevisionLease,
} from './persistentRecordIterator'

const DEFAULT_BRANCH_PAGE_SIZE = 128

export interface CopyPinnedConversationBranchInput {
    characterId: string
    sourceConversationId: string
    sourceRevision: DataRevision
    inclusiveEndIndex: number
    branch: Omit<Chat, 'message'>
    branchMarker: Message
    pageSize?: number
    signal?: AbortSignal
}

export interface PinnedConversationBranchResult {
    sourceCharacterId: string
    sourceConversationId: string
    sourceRevision: DataRevision
    sourceStartIndex: 0
    sourceEndIndex: number
    sourceTotalMessages: number
    branchConversationId: string
    branchRevision: DataRevision
}

function validateCopyInput(input: CopyPinnedConversationBranchInput): number {
    if (!Number.isSafeInteger(input.inclusiveEndIndex) || input.inclusiveEndIndex < 0) {
        throw new RangeError('Branch source index must be a nonnegative safe integer')
    }
    const pageSize = input.pageSize ?? DEFAULT_BRANCH_PAGE_SIZE
    if (
        !Number.isSafeInteger(pageSize)
        || pageSize <= 0
        || pageSize > CONVERSATION_RANGE_MAX_LIMIT
    ) {
        throw new RangeError(
            `Branch page size must be between 1 and ${CONVERSATION_RANGE_MAX_LIMIT}`,
        )
    }
    if (!input.branch.id) throw new Error('Branch conversation requires a nonempty ID')
    return pageSize
}

export async function copyPinnedConversationBranch(
    store: PersistentDataStore,
    input: CopyPinnedConversationBranchInput,
): Promise<PinnedConversationBranchResult> {
    const pageSize = validateCopyInput(input)
    input.signal?.throwIfAborted()
    const lease = await store.acquireRevision(input.sourceRevision)
    return withPersistentRevisionLease(lease, async (reader) => {
        const messages: Message[] = []
        const sourceEndIndex = input.inclusiveEndIndex + 1
        let sourceTotalMessages = -1
        for (let startIndex = 0; startIndex < sourceEndIndex;) {
            input.signal?.throwIfAborted()
            const limit = Math.min(pageSize, sourceEndIndex - startIndex)
            const result = await reader.readConversationWindow({
                characterId: input.characterId,
                conversationId: input.sourceConversationId,
                startIndex,
                limit,
            })
            input.signal?.throwIfAborted()
            if (!result) {
                throw new Error(`Conversation ${input.sourceConversationId} does not exist`)
            }
            assertPinnedRevision(input.sourceRevision, result.revision, 'Branch source page')
            const page = result.value
            if (
                page.characterId !== input.characterId
                || page.conversationId !== input.sourceConversationId
                || page.startIndex !== startIndex
                || page.endIndex !== startIndex + page.messages.length
            ) {
                throw new Error('Branch source page returned mismatched absolute range evidence')
            }
            if (sourceTotalMessages < 0) sourceTotalMessages = page.totalMessages
            if (page.totalMessages !== sourceTotalMessages || sourceEndIndex > page.totalMessages) {
                throw new RangeError('Branch source index is outside the pinned conversation')
            }
            if (page.messages.length !== limit) {
                throw new Error('Branch source page ended before the requested prefix')
            }
            messages.push(...structuredClone(page.messages))
            startIndex = page.endIndex
        }

        input.signal?.throwIfAborted()
        const committed = await store.commit({
            expectedRevision: input.sourceRevision,
            conversations: [{
                type: 'replace-range',
                characterId: input.characterId,
                conversationId: input.branch.id,
                start: 0,
                deleteCount: 0,
                messages: [...messages, structuredClone(input.branchMarker)],
                conversation: structuredClone(input.branch),
                configuredIndex: 0,
            }],
        })
        return {
            sourceCharacterId: input.characterId,
            sourceConversationId: input.sourceConversationId,
            sourceRevision: input.sourceRevision,
            sourceStartIndex: 0,
            sourceEndIndex,
            sourceTotalMessages,
            branchConversationId: input.branch.id,
            branchRevision: committed.revision,
        }
    })
}
