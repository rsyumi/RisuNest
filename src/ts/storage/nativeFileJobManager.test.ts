import { get } from 'svelte/store'
import { describe, expect, it, vi } from 'vitest'

import {
    cancelActiveNativeFileOperation,
    nativeFileOperation,
    runExternalAndroidNativeFileOperation,
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

    it('runs external Android open events FIFO instead of joining an unrelated active operation', async () => {
        let releaseActive!: () => void
        let releaseFirstEvent!: () => void
        const calls: string[] = []
        const active = runSharedNativeFileOperation('import', async () => {
            calls.push('active')
            await new Promise<void>((resolve) => releaseActive = resolve)
            return 'active'
        })
        const firstEvent = runExternalAndroidNativeFileOperation('import', async () => {
            calls.push('first-event')
            await new Promise<void>((resolve) => releaseFirstEvent = resolve)
            return 'first-event'
        })
        const secondEvent = runExternalAndroidNativeFileOperation('import', async () => {
            calls.push('second-event')
            return 'second-event'
        })

        expect(firstEvent).not.toBe(active)
        expect(secondEvent).not.toBe(active)
        expect(calls).toEqual(['active'])

        releaseActive()
        await expect(active).resolves.toBe('active')
        await Promise.resolve()
        expect(calls).toEqual(['active', 'first-event'])

        releaseFirstEvent()
        await expect(firstEvent).resolves.toBe('first-event')
        await expect(secondEvent).resolves.toBe('second-event')
        expect(calls).toEqual(['active', 'first-event', 'second-event'])
    })
})
