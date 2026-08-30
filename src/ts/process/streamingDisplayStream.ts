import type { StreamingDisplayOptimizationMode } from '../storage/database.svelte'
import {
    createStreamingDisplayController,
    type ScheduledSnapshot,
    type StreamingDisplayProcessContext,
} from './streamingDisplayScheduler'

export interface StreamingDisplayReader<T> {
    read(): Promise<ReadableStreamReadResult<T>>
    cancel(): Promise<void>
}

interface ConsumeStreamingDisplayStreamOptions<T> {
    mode: StreamingDisplayOptimizationMode
    reader: StreamingDisplayReader<T>
    abortSignal: AbortSignal
    getSnapshot(value: T): string
    isOwned(): boolean
    processSemantic(
        snapshot: ScheduledSnapshot<string>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    processPreview(
        snapshot: ScheduledSnapshot<string>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    onValue?(value: T, snapshot: string): void
}

interface ConsumeStreamingDisplayStreamResult<T> {
    completed: boolean
    latestSnapshot: string
    lastValue: T | undefined
}

export async function consumeStreamingDisplayStream<T>(
    options: ConsumeStreamingDisplayStreamOptions<T>,
): Promise<ConsumeStreamingDisplayStreamResult<T>> {
    let aborted = options.abortSignal.aborted
    let normalEof = false
    let latestSnapshot = ''
    let lastValue: T | undefined
    const withOwnership = (
        context: StreamingDisplayProcessContext,
    ): StreamingDisplayProcessContext => ({
        signal: context.signal,
        canCommit: () => context.canCommit() && options.isOwned(),
    })
    const controller = createStreamingDisplayController<string>({
        mode: options.mode,
        processSemantic: (snapshot, context) =>
            options.processSemantic(snapshot, withOwnership(context)),
        processPreview: (snapshot, context) =>
            options.processPreview(snapshot, withOwnership(context)),
        onError: () => {
            void options.reader.cancel().catch(() => {})
        },
    })
    let abortPromise: Promise<void> | null = null
    const abortDisplay = () => abortPromise ??= controller.abort()
    const abort = () => {
        aborted = true
        void abortDisplay()
        void options.reader.cancel().catch(() => {})
    }

    options.abortSignal.addEventListener('abort', abort, { once: true })
    if (options.abortSignal.aborted) abort()
    try {
        while (!aborted) {
            let read: ReadableStreamReadResult<T>
            try {
                read = await options.reader.read()
            }
            catch (error) {
                if (options.abortSignal.aborted || aborted) break
                await abortDisplay()
                throw error
            }
            if (read.value !== undefined) {
                lastValue = read.value
                latestSnapshot = options.getSnapshot(read.value)
                options.onValue?.(read.value, latestSnapshot)
                await controller.submit(latestSnapshot)
            }
            if (read.done) {
                normalEof = true
                break
            }
        }
    }
    finally {
        try {
            if (normalEof && !aborted && !options.abortSignal.aborted) {
                await controller.finish()
            }
            else {
                await abortDisplay()
            }
        }
        finally {
            options.abortSignal.removeEventListener('abort', abort)
            void options.reader.cancel().catch(() => {})
        }
    }

    return {
        completed: normalEof && !aborted && options.isOwned(),
        latestSnapshot,
        lastValue,
    }
}
