import { get } from 'svelte/store'
import { describe, expect, it, vi } from 'vitest'

import {
    cancelActiveNativeFileOperation,
    nativeFileOperation,
    runSharedNativeFileOperation,
} from './nativeFileJobManager'

describe('renderer-lifetime native file job manager', () => {
    it('coalesces remounted callers onto one operation and exposes shared progress', async () => {
        let finish!: (value: string) => void
        const operation = vi.fn(async ({ onStatus }: {
            signal: AbortSignal
            onStatus(status: never): void
            setBlocking(value: boolean): void
        }) => {
            onStatus({
                jobId: 'restore-1',
                kind: 'restore-block-risu-save',
                state: 'running',
                phase: 'reading-source',
                progress: { completedBytes: 64, totalBytes: 128, completedItems: 0 },
            } as never)
            return await new Promise<string>((resolve) => finish = resolve)
        })

        const first = runSharedNativeFileOperation('import', operation)
        const second = runSharedNativeFileOperation('import', operation)

        expect(first).toBe(second)
        expect(operation).toHaveBeenCalledOnce()
        expect(get(nativeFileOperation)?.status?.progress.completedBytes).toBe(64)

        finish('done')
        await expect(first).resolves.toBe('done')
        expect(get(nativeFileOperation)).toBeNull()
    })

    it('cancels only through the explicit manager action', async () => {
        let observedSignal!: AbortSignal
        const promise = runSharedNativeFileOperation('export', async ({ signal }) => {
            observedSignal = signal
            return await new Promise<never>((_resolve, reject) => {
                signal.addEventListener('abort', () => reject(signal.reason))
            })
        })

        expect(observedSignal.aborted).toBe(false)
        cancelActiveNativeFileOperation()
        await expect(promise).rejects.toBeDefined()
        expect(observedSignal.aborted).toBe(true)
    })
})
