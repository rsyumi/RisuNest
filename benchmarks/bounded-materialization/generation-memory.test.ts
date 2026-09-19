import { describe, expect, it } from 'vitest'
import type {
    PersistentDataStore,
    PersistentRevisionLease,
} from '../../src/ts/storage/persistentDataStore'
import { prepareSummaryAwareGeneration } from '../../src/ts/process/summaryAwareGenerationPreparation'

describe('summary-aware bounded generation memory evidence', () => {
    it('reports metadata growth separately from transferred bodies and retained heap', async () => {
        const totalMessages = 50_000
        const suffixMessages = 32
        const boundary = totalMessages - suffixMessages
        const bodyReads: number[] = []
        const lease = {
            revision: 1,
            async readConversationMessageMetadataWindow(input: any) {
                await new Promise<void>((resolve) => setImmediate(resolve))
                const endIndex = Math.min(totalMessages, input.startIndex + input.limit)
                return {
                    revision: 1,
                    value: {
                        characterId: input.characterId,
                        conversationId: input.conversationId,
                        startIndex: input.startIndex,
                        endIndex,
                        totalMessages,
                        hasMoreBefore: input.startIndex > 0,
                        hasMoreAfter: endIndex < totalMessages,
                        messages: Array.from(
                            { length: endIndex - input.startIndex },
                            (_, offset) => ({
                                chatId: `message-${input.startIndex + offset}`,
                                role: (input.startIndex + offset) % 2 ? 'char' : 'user',
                                parserInert: true,
                            }),
                        ),
                    },
                }
            },
            async readConversationWindow(input: any) {
                await new Promise<void>((resolve) => setImmediate(resolve))
                bodyReads.push(input.startIndex)
                return {
                    revision: 1,
                    value: {
                        characterId: input.characterId,
                        conversationId: input.conversationId,
                        startIndex: input.startIndex,
                        endIndex: input.startIndex + 1,
                        totalMessages,
                        hasMoreBefore: true,
                        hasMoreAfter: input.startIndex + 1 < totalMessages,
                        messages: [{
                            chatId: `message-${input.startIndex}`,
                            role: input.startIndex % 2 ? 'char' : 'user',
                            data: 'x'.repeat(2 * 1024),
                        }],
                    },
                }
            },
            async release() {},
        } as unknown as PersistentRevisionLease
        const store = {
            readConversationMessageMetadataWindow() {},
            async acquireRevision() { return lease },
        } as unknown as PersistentDataStore
        const conversation = {
            id: 'large-conversation',
            name: 'Large conversation',
            hypaV3Data: {
                summaries: [{
                    chatMemos: Array.from({ length: boundary }, (_, index) =>
                        `message-${index}`),
                }],
            },
        } as any
        const baselineHeapUsedBytes = process.memoryUsage().heapUsed
        let peakHeapUsedBytes = baselineHeapUsedBytes
        const sampler = setInterval(() => {
            peakHeapUsedBytes = Math.max(peakHeapUsedBytes, process.memoryUsage().heapUsed)
        }, 1)
        const result = await prepareSummaryAwareGeneration({
            store,
            authority: {
                kind: 'windowed',
                characterId: 'large-character',
                conversationId: 'large-conversation',
                sessionToken: 'memory-evidence' as any,
                storeRevision: 1,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages,
            },
            conversation,
            preserveOrphanedMemory: false,
            isCurrent: () => true,
        })
        clearInterval(sampler)
        expect(result.route).toBe('summary-aware')
        if (result.route !== 'summary-aware') return
        const retainedHeapUsedBytes = process.memoryUsage().heapUsed
        peakHeapUsedBytes = Math.max(peakHeapUsedBytes, retainedHeapUsedBytes)
        expect(bodyReads).toHaveLength(suffixMessages)
        expect(bodyReads[0]).toBe(boundary)
        process.stdout.write('BOUNDED_GENERATION_MEMORY ' + JSON.stringify({
            totalMessages,
            summarizedMessages: boundary,
            suffixMessages,
            baselineHeapUsedBytes,
            peakHeapUsedBytes,
            retainedHeapUsedBytes,
            ...result.preparation.metrics,
            classificationRows: totalMessages,
            scope: 'Node preparation helper with synthetic store; CBS, regex and tokenizer are not executed',
            coveredBodyTransfers: 0,
        }) + '\n')
        await result.preparation.release()
    })
})
