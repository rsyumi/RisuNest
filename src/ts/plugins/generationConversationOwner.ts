import type { GenerationOutputPhase } from '../process/generationOutputPhase'
import { valuesEqual } from '../process/conversationOperationContext'
import { safeStructuredClone } from '../safeStructuredClone'
import { cloneConversationMetadata, replaceConversationMetadata, type ActiveConversationOperationRange } from '../storage/activeConversationSession'
import type { SelectedConversationTarget, WindowedConversationRowEdit } from '../storage/activeWorkingSet.svelte'
import type { Chat, Message } from '../storage/database.svelte'
import type { PersistentDataStore } from '../storage/persistentDataStore'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'

/** Freeze the shell and the sparse row projection together at admission, then load its base in bounded pages. */
export async function readGenerationConversation(target: SelectedConversationTarget, shell: Chat, store: PersistentDataStore, runtime: PersistentDataRuntime): Promise<Chat> {
    const session = runtime.getActiveConversationSession()
    if (session?.matchesConversation(target.characterId, shell)) return safeStructuredClone(shell)
    const source = runtime.getActiveConversationViewportSource()
    if (!source?.captureProjectedReader) throw new Error('Generating conversation has no projected reader')
    const snapshot = source.snapshot()
    const lease = store.acquireRevision(snapshot.storeRevision)
    const projected = source.captureProjectedReader({
        readConversationWindow: async query => (await lease).readConversationWindow(query),
    })
    const messages: Message[] = []
    const chat = { ...cloneConversationMetadata(shell), message: messages } as Chat
    try {
        for (let start = 0; start < snapshot.totalMessages; start += 128) {
            const limit = Math.min(128, snapshot.totalMessages - start)
            const page = await projected.readConversationWindow({ characterId: target.characterId, conversationId: target.conversationId, startIndex: start, limit })
            if (!page || page.revision !== snapshot.storeRevision || page.value.messages.length !== limit)
                throw new Error('Incomplete generating conversation projection')
            messages.push(...page.value.messages)
        }
        return chat
    } finally {
        await (await lease).release()
    }
}

export function applyGenerationConversationRanges(
    target: SelectedConversationTarget, shell: Chat, phase: GenerationOutputPhase,
    ranges: readonly WindowedConversationRowEdit[], metadata: Record<string, unknown>, runtime: PersistentDataRuntime,
): boolean {
    const expectedMetadata = cloneConversationMetadata(shell)
    if (!ranges.length && valuesEqual(metadata, expectedMetadata)) return true
    const session = runtime.getActiveConversationSession()
    if (session?.matchesConversation(target.characterId, shell)) {
        const previousVersion = session.version
        const replyIndex = phase.messageIndex
        // The setter and patch planners emit non-overlapping ranges in application order.
        // Session positions address the original array, so undo the preceding count deltas.
        const baseRanges: ActiveConversationOperationRange[] = []
        let offset = 0, previousEnd = 0
        for (const range of ranges) {
            const start = range.start - offset
            if (start < previousEnd || !valuesEqual(range.before, shell.message.slice(start, start + range.before.length))) return false
            baseRanges.push({ position: session.positionAt(start), deleteCount: range.before.length, messages: range.messages })
            previousEnd = start + range.before.length
            offset += range.messages.length - range.before.length
        }
        if (baseRanges.length > 1 && baseRanges.some(range => range.deleteCount !== range.messages.length)) {
            const first = baseRanges[0].position.absoluteIndex
            let cursor = first
            const replacement: Message[] = []
            for (const range of baseRanges) {
                const start = range.position.absoluteIndex
                for (; cursor < start; cursor++) replacement.push(shell.message[cursor])
                for (const message of range.messages) replacement.push(message)
                cursor = start + range.deleteCount
            }
            baseRanges.splice(0, baseRanges.length, { position: session.positionAt(first), deleteCount: cursor - first, messages: replacement })
        }
        session.applyOperation({ expectedVersion: previousVersion, expectedMetadata, metadata, ranges: baseRanges })
        phase.acceptCommit({ follows: (owner, version, chat) => owner === session && chat === shell &&
            version !== null && version <= previousVersion && replyIndex >= 0 && session.isActive,
            mapSessionIndex(index) {
                if (index !== replyIndex) return null
                for (const range of ranges) {
                    if (index < range.start) continue
                    if (index >= range.start + range.before.length) {
                        index += range.messages.length - range.before.length
                        continue
                    }
                    const offset = index - range.start
                    if (range.messages.length === range.before.length &&
                        range.before[offset].chatId === range.messages[offset].chatId) continue
                    const id = range.before[offset].chatId
                    const next = id && range.messages.findIndex(message => message.chatId === id)
                    if (typeof next !== 'number' || next < 0 || range.messages.findLastIndex(message => message.chatId === id) !== next) return null
                    index = range.start + next
                }
                return index
            },
        })
        return true
    }
    const authority = runtime.captureSelectedConversationAuthority()
    if (!authority) return false
    const chat = { ...expectedMetadata, message: [] } as Chat
    const controller = runtime.captureWindowedConversationMutationController(target, chat, authority.totalMessages)
    if (!controller) return false
    try {
        replaceConversationMetadata(chat, metadata)
        return controller.applyAbsoluteRanges?.(ranges) ?? false
    } finally {
        controller.release()
    }
}
