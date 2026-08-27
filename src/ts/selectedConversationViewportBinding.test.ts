import { describe, expect, test, vi } from 'vitest'
import type {
    ConversationViewportKey,
    ConversationViewportRow,
    ConversationViewportSnapshot,
    ConversationViewportSource,
} from './conversationViewportSource'
import type { Message } from './storage/database.svelte'
import {
    SelectedConversationViewportBinding,
    type SelectedConversationViewportRuntime,
} from './selectedConversationViewportBinding'

class TestViewportSource implements ConversationViewportSource {
    private readonly rows = new Map<number, ConversationViewportRow>()
    private readonly listeners = new Set<() => void>()
    readonly dispose = vi.fn()
    readonly ensureRange = vi.fn(async ({ startIndex, limit, signal }) => {
        await Promise.resolve()
        if (signal?.aborted) return
        for (let index = startIndex; index < Math.min(startIndex + limit, this.messages.length); index++) {
            this.rows.set(index, {
                key: this.keyAt(index)!,
                absoluteIndex: index,
                message: this.messages[index],
                sourceVersion: this.version,
            })
        }
        for (const listener of [...this.listeners]) listener()
    })

    constructor(
        readonly sourceToken: string,
        private readonly messages: readonly Message[],
        private version = 1,
    ) {}

    snapshot(): ConversationViewportSnapshot {
        return {
            sourceToken: this.sourceToken,
            version: this.version,
            storeRevision: 1,
            totalMessages: this.messages.length,
            keyAt: (index) => this.keyAt(index),
            indexOfKey: (key) => Number(String(key).split(':').at(-1)),
            rowAt: (index) => this.rows.get(index),
        }
    }

    acquireRangePin() {
        return { release() {} }
    }

    subscribe(listener: () => void): () => void {
        this.listeners.add(listener)
        return () => this.listeners.delete(listener)
    }

    captureMessageTarget() {
        return null
    }

    evictRows(): void {
        this.rows.clear()
        for (const listener of [...this.listeners]) listener()
    }

    private keyAt(index: number): ConversationViewportKey | undefined {
        if (index < 0 || index >= this.messages.length) return undefined
        return `${this.sourceToken}:${index}` as ConversationViewportKey
    }
}

class TestRuntime implements SelectedConversationViewportRuntime {
    private readonly listeners = new Set<(source: ConversationViewportSource | null) => void>()

    constructor(private source: ConversationViewportSource | null) {}

    getActiveConversationViewportSource(): ConversationViewportSource | null {
        return this.source
    }

    subscribeActiveConversationViewportSource(
        listener: (source: ConversationViewportSource | null) => void,
    ): () => void {
        this.listeners.add(listener)
        return () => this.listeners.delete(listener)
    }

    publish(source: ConversationViewportSource | null): void {
        this.source = source
        for (const listener of [...this.listeners]) listener(source)
    }
}

function message(data: string, role: Message['role'] = 'char'): Message {
    return { data, role }
}

describe('SelectedConversationViewportBinding', () => {
    test('loads only the first and tail rows from the runtime-owned source', async () => {
        const source = new TestViewportSource('source-a', [
            message('first', 'user'),
            message('middle'),
            message('tail'),
        ])
        const binding = new SelectedConversationViewportBinding(new TestRuntime(source), () => {})

        await vi.waitFor(() => {
            expect(binding.firstMessage?.data).toBe('first')
            expect(binding.tailMessage?.data).toBe('tail')
        })
        expect(binding.totalMessages).toBe(3)
        expect(source.ensureRange).toHaveBeenCalledTimes(2)
        expect(source.ensureRange).toHaveBeenCalledWith(expect.objectContaining({
            startIndex: 0,
            limit: 1,
        }))
        expect(source.ensureRange).toHaveBeenCalledWith(expect.objectContaining({
            startIndex: 2,
            limit: 1,
        }))

        source.evictRows()
        expect(binding.firstMessage?.data).toBe('first')
        expect(binding.tailMessage?.data).toBe('tail')
        expect(source.ensureRange).toHaveBeenCalledTimes(2)

        binding.dispose()
        expect(source.dispose).not.toHaveBeenCalled()
    })

    test('publishes a runtime source replacement to the UI and safely unsubscribes', async () => {
        const first = new TestViewportSource('source-a', [message('old')])
        const replacement = new TestViewportSource('source-b', [message('new'), message('tail')])
        const runtime = new TestRuntime(first)
        const changed = vi.fn()
        const binding = new SelectedConversationViewportBinding(runtime, changed)
        await vi.waitFor(() => expect(binding.firstMessage?.data).toBe('old'))
        changed.mockClear()

        runtime.publish(replacement)

        expect(changed).toHaveBeenCalledTimes(1)
        expect(binding.source).toBe(replacement)
        expect(binding.totalMessages).toBe(2)
        await vi.waitFor(() => expect(binding.tailMessage?.data).toBe('tail'))

        binding.dispose()
        changed.mockClear()
        runtime.publish(first)
        expect(changed).not.toHaveBeenCalled()
        expect(first.dispose).not.toHaveBeenCalled()
        expect(replacement.dispose).not.toHaveBeenCalled()
    })
})
