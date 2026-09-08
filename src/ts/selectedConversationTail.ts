import type { ConversationViewportSource } from './conversationViewportSource'
import { safeStructuredClone } from './polyfill'
import {
    isSameSelectedConversationTarget,
    type SelectedConversationTarget,
} from './storage/activeWorkingSet.svelte'
import type { Message } from './storage/database.svelte'

const MAX_SELECTED_CONVERSATION_TAIL = 10

export interface SelectedConversationTailRuntime {
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    getActiveConversationViewportSource(): ConversationViewportSource | null
}

export class SelectedConversationTailStaleError extends Error {
    constructor() {
        super('Selected conversation changed while reading its tail')
        this.name = 'SelectedConversationTailStaleError'
    }
}

export async function readSelectedConversationLatestTail(
    runtime: SelectedConversationTailRuntime,
    limit: number,
    signal?: AbortSignal,
): Promise<Message[]> {
    if (!Number.isSafeInteger(limit) || limit <= 0) {
        throw new RangeError(
            'Selected conversation tail limit must be a positive safe integer',
        )
    }
    if (limit > MAX_SELECTED_CONVERSATION_TAIL) {
        throw new RangeError(
            `Selected conversation tail limit cannot exceed ${MAX_SELECTED_CONVERSATION_TAIL}`,
        )
    }
    const target = runtime.captureSelectedConversationTarget()
    try {
        return await readExactSelectedConversationTail(runtime, limit, signal)
    } catch (error) {
        assertNotAborted(signal)
        const currentTarget = runtime.captureSelectedConversationTarget()
        if (
            error instanceof SelectedConversationTailStaleError &&
            target &&
            currentTarget &&
            currentTarget.storeRevision > target.storeRevision &&
            currentTarget.characterId === target.characterId &&
            currentTarget.conversationId === target.conversationId &&
            currentTarget.navigationGeneration === target.navigationGeneration
        ) {
            // Saving can replace the windowed source and its state token. Start a
            // fresh read once for the same selection, retaining exact snapshot checks.
            return readExactSelectedConversationTail(runtime, limit, signal)
        }
        throw error
    }
}

async function readExactSelectedConversationTail(
    runtime: SelectedConversationTailRuntime,
    limit: number,
    signal?: AbortSignal,
): Promise<Message[]> {
    assertNotAborted(signal)
    const target = runtime.captureSelectedConversationTarget()
    const source = runtime.getActiveConversationViewportSource()
    if (!target || !source) throw new SelectedConversationTailStaleError()
    const snapshot = source.snapshot()
    if (snapshot.storeRevision !== target.storeRevision) {
        throw new SelectedConversationTailStaleError()
    }
    const count = Math.min(limit, snapshot.totalMessages)
    if (count === 0) return []
    const startIndex = snapshot.totalMessages - count

    await source.ensureRange({
        startIndex,
        limit: count,
        reason: 'viewport',
        signal,
    })
    assertNotAborted(signal)
    const currentTarget = runtime.captureSelectedConversationTarget()
    const currentSource = runtime.getActiveConversationViewportSource()
    const currentSnapshot = currentSource?.snapshot()
    if (
        !currentTarget ||
        currentSource !== source ||
        !currentSnapshot ||
        !isSameSelectedConversationTarget(target, currentTarget) ||
        currentSnapshot.sourceToken !== snapshot.sourceToken ||
        currentSnapshot.version !== snapshot.version ||
        currentSnapshot.storeRevision !== target.storeRevision ||
        currentSnapshot.totalMessages !== snapshot.totalMessages
    )
        throw new SelectedConversationTailStaleError()

    const messages: Message[] = []
    for (
        let absoluteIndex = startIndex;
        absoluteIndex < snapshot.totalMessages;
        absoluteIndex++
    ) {
        const row = currentSnapshot.rowAt(absoluteIndex)
        if (!row || row.absoluteIndex !== absoluteIndex) {
            throw new SelectedConversationTailStaleError()
        }
        messages.push(safeStructuredClone(row.message))
    }
    return messages
}

function assertNotAborted(signal?: AbortSignal): void {
    if (signal?.aborted) {
        throw new DOMException('Selected conversation tail read was cancelled', 'AbortError')
    }
}
