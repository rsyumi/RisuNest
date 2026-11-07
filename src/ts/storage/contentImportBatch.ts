import type { NativeFileOperationSource, SharedNativeFileOperationContext } from './nativeFileJobManager'
import { NativeFileJobActivationCommittedError, NativeFileJobActivationUnresolvedError } from './nativeFileJobs'

export interface ContentImportBatchFailure {
    name: string
    error: unknown
}

export interface ContentImportBatchOptions<T> {
    /** Files the picker already refused; they count toward the batch. */
    failures?: readonly ContentImportBatchFailure[]
    /** Releases a file that was never imported because the batch stopped first. */
    releaseUnstarted?(item: T): Promise<unknown>
}

function stopsBatch(error: unknown, signal: AbortSignal): boolean {
    return signal.aborted
        || (error instanceof DOMException && error.name === 'AbortError')
        // Committed data needs a refresh before any other change.
        || error instanceof NativeFileJobActivationCommittedError
        || error instanceof NativeFileJobActivationUnresolvedError
}

/**
 * Imports picked files one after another inside the caller's operation and returns the last
 * imported id. One file keeps the single-file outcome; several files continue past a failed file
 * and report every file in one outcome.
 */
export async function importContentBatch<T>(
    context: SharedNativeFileOperationContext,
    items: readonly T[],
    describe: (item: T) => NativeFileOperationSource,
    importItem: (item: T) => Promise<string | null>,
    options: ContentImportBatchOptions<T> = {},
): Promise<string | null> {
    const failures = options.failures ?? []
    let next = 0
    try {
        if (items.length === 1 && !failures.length) {
            next = 1
            context.setSource(describe(items[0]))
            return await importItem(items[0])
        }
        context.beginBatch(items.length + failures.length)
        for (const failure of failures) context.recordBatchItem(failure.name, { error: failure.error })
        let last: string | null = null
        while (next < items.length) {
            context.signal.throwIfAborted()
            const item = items[next++]
            const source = describe(item)
            context.beginBatchItem(failures.length + next, source)
            try {
                const value = await importItem(item)
                context.recordBatchItem(source.name, { imported: value !== null })
                last = value ?? last
            } catch (error) {
                if (stopsBatch(error, context.signal)) throw error
                context.recordBatchItem(source.name, { error })
            }
        }
        return last
    } finally {
        for (const item of items.slice(next)) await options.releaseUnstarted?.(item).catch(() => {})
    }
}
