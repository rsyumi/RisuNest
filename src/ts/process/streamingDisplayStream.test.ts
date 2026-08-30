import { describe, expect, it, vi } from 'vitest'
import {
    consumeStreamingDisplayStream,
    type StreamingDisplayReader,
} from './streamingDisplayStream'

function readerFrom<T>(
    reads: Array<
        ReadableStreamReadResult<T>
        | (() => ReadableStreamReadResult<T> | Promise<ReadableStreamReadResult<T>>)
    >,
) {
    const cancel = vi.fn(async () => undefined)
    const reader: StreamingDisplayReader<T> = {
        cancel,
        async read() {
            const next = reads.shift()
            if (!next) return { done: true, value: undefined }
            return typeof next === 'function' ? next() : next
        },
    }
    return { reader, cancel }
}

describe('sendChat streaming response boundary', () => {
    it('processes a final provider value that arrives together with done', async () => {
        const semanticValues: string[] = []
        const previewValues: string[] = []
        const { reader } = readerFrom([
            { done: true, value: { text: 'final' } },
        ])

        const result = await consumeStreamingDisplayStream({
            mode: 'strong',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: () => true,
            processPreview: async ({ value }, context) => {
                if (context.canCommit()) previewValues.push(value)
            },
            processSemantic: async ({ value }, context) => {
                if (context.canCommit()) semanticValues.push(value)
            },
        })

        expect(previewValues).toEqual(['final'])
        expect(semanticValues).toEqual(['final'])
        expect(result).toMatchObject({ completed: true, latestSnapshot: 'final' })
    })

    it('propagates reader failure without converting it into normal EOF', async () => {
        const message = { data: 'previous' }
        const failure = new Error('reader failed')
        const cancel = vi.fn(async () => undefined)
        const reader: StreamingDisplayReader<{ text: string }> = {
            cancel,
            async read() {
                throw failure
            },
        }

        await expect(consumeStreamingDisplayStream({
            mode: 'balanced',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: () => true,
            processSemantic: async ({ value }, context) => {
                if (context.canCommit()) message.data = `semantic:${value}`
            },
            processPreview: async ({ value }, context) => {
                if (context.canCommit()) message.data = `preview:${value}`
            },
        })).rejects.toBe(failure)

        expect(message.data).toBe('previous')
        expect(cancel).toHaveBeenCalledTimes(1)
    })

    it('drops a late commit when ownership is lost during semantic processing', async () => {
        const message = { data: 'previous' }
        let owned = true
        let release!: () => void
        const active = new Promise<void>((resolve) => { release = resolve })
        const { reader } = readerFrom([
            { done: false, value: { text: 'late' } },
            () => {
                owned = false
                release()
                return { done: true, value: undefined }
            },
        ])

        const result = await consumeStreamingDisplayStream({
            mode: 'balanced',
            reader,
            abortSignal: new AbortController().signal,
            getSnapshot: (value) => value.text,
            isOwned: () => owned,
            processSemantic: async ({ value }, context) => {
                await active
                if (context.canCommit()) message.data = value
            },
            processPreview: async () => {},
        })

        expect(message.data).toBe('previous')
        expect(result.completed).toBe(false)
    })

    it('waits for active balanced processing to settle after the outer signal aborts', async () => {
        const abortController = new AbortController()
        let releaseSemantic!: () => void
        const semanticReleased = new Promise<void>((resolve) => { releaseSemantic = resolve })
        let markSemanticStarted!: () => void
        const semanticStarted = new Promise<void>((resolve) => { markSemanticStarted = resolve })
        let releasePendingRead!: (read: ReadableStreamReadResult<{ text: string }>) => void
        let markPendingReadStarted!: () => void
        const pendingReadStarted = new Promise<void>((resolve) => { markPendingReadStarted = resolve })
        let readCount = 0
        const cancel = vi.fn(async () => {
            releasePendingRead?.({ done: true, value: undefined })
        })
        const reader: StreamingDisplayReader<{ text: string }> = {
            cancel,
            async read() {
                if (readCount++ === 0) return { done: false, value: { text: 'late' } }
                markPendingReadStarted()
                return new Promise((resolve) => { releasePendingRead = resolve })
            },
        }
        let committed = 'previous'

        const consuming = consumeStreamingDisplayStream({
            mode: 'balanced',
            reader,
            abortSignal: abortController.signal,
            getSnapshot: (value) => value.text,
            isOwned: () => true,
            processSemantic: async ({ value }, context) => {
                markSemanticStarted()
                await semanticReleased
                if (context.canCommit()) committed = value
            },
            processPreview: async () => {},
        })
        await Promise.all([semanticStarted, pendingReadStarted])

        abortController.abort()
        let settled = false
        void consuming.then(() => { settled = true })
        await new Promise((resolve) => setTimeout(resolve, 0))

        expect(cancel).toHaveBeenCalled()
        expect(settled).toBe(false)

        releaseSemantic()
        await expect(consuming).resolves.toMatchObject({ completed: false })
        expect(committed).toBe('previous')
    })
})
