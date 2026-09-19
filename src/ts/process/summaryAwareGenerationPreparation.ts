import { safeStructuredClone } from '../polyfill'
import type { Chat } from '../storage/database.svelte'
import type {
    ConversationMessageMetadata,
    PersistentDataStore,
    PersistentRevisionLease,
} from '../storage/persistentDataStore'
import type { WindowedConversationPersistenceAuthority } from '../storage/saveCoordinator'
import {
    planSummaryAwarePromptMetadata,
    type SummaryAwarePromptHistoryPlan,
} from './summaryAwarePromptHistory'

const METADATA_PAGE_SIZE = 128

export interface SummaryAwareGenerationPreparationMetrics {
    metadataRows: number
    metadataPages: number
    bodyRows: number
    bodyPages: number
    bodyBytes: number
    elapsedMs: number
}

export interface SummaryAwareGenerationPreparation {
    chat: Chat
    plan: SummaryAwarePromptHistoryPlan
    authority: WindowedConversationPersistenceAuthority
    lease: PersistentRevisionLease
    metrics: SummaryAwareGenerationPreparationMetrics
    release(): Promise<void>
}

export type SummaryAwareGenerationPreparationDecision =
    | { route: 'summary-aware'; preparation: SummaryAwareGenerationPreparation }
    | { route: 'complete'; reason: string }

interface PreparationInput {
    store: PersistentDataStore
    authority: WindowedConversationPersistenceAuthority
    conversation: Omit<Chat, 'message'>
    preserveOrphanedMemory: boolean
    minimumTailMessages?: number
    signal?: AbortSignal
    isCurrent(): boolean
    now?(): number
}

function assertCurrent(input: PreparationInput): void {
    input.signal?.throwIfAborted()
    if (!input.isCurrent()) throw new Error('Summary-aware generation target became stale')
}

export async function prepareSummaryAwareGeneration(
    input: PreparationInput,
): Promise<SummaryAwareGenerationPreparationDecision> {
    const readMetadata = input.store.readConversationMessageMetadataWindow
    if (!readMetadata) return { route: 'complete', reason: 'metadata-reader-unavailable' }
    if (input.authority.sessionVersion !== input.authority.persistedSessionVersion) {
        return { route: 'complete', reason: 'pending-selected-session-edits' }
    }
    const startedAt = (input.now ?? performance.now.bind(performance))()
    assertCurrent(input)
    const lease = await input.store.acquireRevision(input.authority.storeRevision)
    let released = false
    const release = async () => {
        if (released) return
        released = true
        await lease.release()
    }
    try {
        const metadata: ConversationMessageMetadata[] = []
        let metadataPages = 0
        for (let startIndex = 0; startIndex < input.authority.totalMessages;) {
            assertCurrent(input)
            const page = await lease.readConversationMessageMetadataWindow?.({
                characterId: input.authority.characterId,
                conversationId: input.authority.conversationId,
                startIndex,
                limit: Math.min(METADATA_PAGE_SIZE, input.authority.totalMessages - startIndex),
            })
            assertCurrent(input)
            if (!page || page.revision !== input.authority.storeRevision) {
                throw new Error('Summary-aware generation metadata revision changed')
            }
            if (
                page.value.startIndex !== startIndex
                || page.value.totalMessages !== input.authority.totalMessages
                || page.value.messages.length === 0
            ) throw new Error('Summary-aware generation metadata page is incomplete')
            metadata.push(...page.value.messages)
            metadataPages += 1
            startIndex = page.value.endIndex
        }
        const decision = planSummaryAwarePromptMetadata(
            input.conversation,
            metadata,
            input.preserveOrphanedMemory,
        )
        if (decision.route === 'complete') {
            await release()
            return decision
        }
        const tailCount = metadata.slice(decision.plan.bodyStartIndex)
            .filter((message) => message.disabled !== true).length
        if (tailCount < (input.minimumTailMessages ?? 0)) {
            await release()
            return { route: 'complete', reason: 'memory-query-needs-covered-history' }
        }

        const messages = []
        let bodyBytes = 0
        let bodyPages = 0
        for (
            let startIndex = decision.plan.bodyStartIndex;
            startIndex < input.authority.totalMessages;
            startIndex += 1
        ) {
            assertCurrent(input)
            const page = await lease.readConversationWindow({
                characterId: input.authority.characterId,
                conversationId: input.authority.conversationId,
                startIndex,
                limit: 1,
            })
            assertCurrent(input)
            if (
                !page
                || page.revision !== input.authority.storeRevision
                || page.value.startIndex !== startIndex
                || page.value.totalMessages !== input.authority.totalMessages
                || page.value.messages.length !== 1
            ) throw new Error('Summary-aware generation body page is incomplete')
            const message = page.value.messages[0]
            messages.push(message)
            bodyBytes += new TextEncoder().encode(JSON.stringify(message)).byteLength
            bodyPages += 1
        }
        const chat = {
            ...safeStructuredClone(input.conversation),
            message: messages,
        } as Chat
        const metrics: SummaryAwareGenerationPreparationMetrics = {
            metadataRows: metadata.length,
            metadataPages,
            bodyRows: messages.length,
            bodyPages,
            bodyBytes,
            elapsedMs: (input.now ?? performance.now.bind(performance))() - startedAt,
        }
        return {
            route: 'summary-aware',
            preparation: {
                chat,
                plan: decision.plan,
                authority: { ...input.authority },
                lease,
                metrics,
                release,
            },
        }
    } catch (error) {
        await release()
        throw error
    }
}
