import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createExternalStorageScheduler } from './scheduler'
import type { ExternalStorageController } from './controller'

describe('external storage scheduler', () => {
    beforeEach(() => vi.useFakeTimers())

    function harness(kind: 'sync' | 'backup' = 'sync') {
        const request = vi.fn(async input => ({
            kind: 'complete' as const,
            revision: input.targetRevision,
            job: {} as never,
        }))
        const cancel = vi.fn(async () => {})
        const controller = { request, cancel } as unknown as ExternalStorageController
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => true,
            destinations: () => [{ connectionId: 'destination-1', kind }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
        })
        return { scheduler, request, cancel }
    }

    it('coalesces edits to the latest revision after 15 seconds of quiet', async () => {
        const { scheduler, request } = harness()
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(14_000)
        scheduler.durableRevision('2')
        await vi.advanceTimersByTimeAsync(14_000)
        scheduler.durableRevision('30')
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(14_999)
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledOnce()
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ targetRevision: '30' }))
    })

    it('does not let continuous edits extend the 60 second maximum', async () => {
        const { scheduler, request } = harness()
        scheduler.durableRevision('1')
        for (let index = 2; index <= 5; index += 1) {
            await vi.advanceTimersByTimeAsync(12_000)
            scheduler.durableRevision(String(index) as `${number}`)
        }
        await vi.advanceTimersByTimeAsync(11_999)
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ targetRevision: '5' }))
    })

    it('uses five seconds for generation completion and creates no token-level jobs', async () => {
        const { scheduler, request } = harness()
        scheduler.durableRevision('7', 'generation-complete')
        scheduler.durableRevision('7', 'generation-complete')
        await vi.advanceTimersByTimeAsync(4_999)
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledOnce()
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ targetRevision: '7' }))
    })

    it('uses the 60 second quiet and five minute maximum backup policy', async () => {
        const { scheduler, request } = harness('backup')
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(59_000)
        scheduler.durableRevision('2')
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).toHaveBeenCalledWith(expect.objectContaining({
            kind: 'backup', targetRevision: '2',
        }))
    })

    it('preserves pending work offline and dispatches it on resume', async () => {
        let online = false
        const request = vi.fn(async () => ({ kind: 'cancelled' as const }))
        const controller = {
            request,
            cancel: vi.fn(async () => {}),
        } as unknown as ExternalStorageController
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => online,
            destinations: () => [{ connectionId: 'sync-1', kind: 'sync' }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
        })
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).not.toHaveBeenCalled()
        online = true
        scheduler.resume()
        await vi.advanceTimersByTimeAsync(0)
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ targetRevision: '9' }))
    })

    it('manual requests bypass debounce and preserve a later automatic revision', async () => {
        const { scheduler, request } = harness()
        scheduler.durableRevision('20')
        await scheduler.requestNow('destination-1', 'sync', '10')
        expect(request).toHaveBeenNthCalledWith(1, expect.objectContaining({
            reason: 'manual', targetRevision: '10',
        }))
        await vi.advanceTimersByTimeAsync(15_000)
        expect(request).toHaveBeenNthCalledWith(2, expect.objectContaining({
            reason: 'automatic', targetRevision: '20',
        }))
    })

    it('retries a durable quota wait at its native deadline without polling', async () => {
        const request = vi.fn()
            .mockResolvedValueOnce({
                kind: 'blocked' as const,
                reason: 'daily-quota-exhausted',
                error: {
                    code: 'daily-quota-exhausted', message: 'Wait', retryable: true,
                    action: 'wait' as const, retryAtMs: '20000' as const,
                },
            })
            .mockResolvedValue({ kind: 'complete' as const, revision: '7', job: {} as never })
        const controller = {
            request,
            cancel: vi.fn(async () => {}),
        } as unknown as ExternalStorageController
        let now = 0
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => true,
            destinations: () => [{ connectionId: 'sync-1', kind: 'sync' }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
            now: () => now,
        })
        scheduler.durableRevision('7')
        now = 15_000
        await vi.advanceTimersByTimeAsync(15_000)
        expect(request).toHaveBeenCalledOnce()
        now = 19_999
        await vi.advanceTimersByTimeAsync(4_999)
        expect(request).toHaveBeenCalledOnce()
        now = 20_000
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledTimes(2)
    })

    it('leaves an unknown publication stopped until an explicit request', async () => {
        const request = vi.fn().mockResolvedValue({
            kind: 'blocked' as const,
            reason: 'publication-unknown',
            error: {
                code: 'transient',
                message: 'Unknown publication result',
                retryable: true,
                action: 'retry' as const,
                reason: 'publication-unknown' as const,
            },
        })
        const controller = {
            request,
            cancel: vi.fn(async () => {}),
        } as unknown as ExternalStorageController
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => true,
            destinations: () => [{ connectionId: 'sync-1', kind: 'sync' }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
        })

        scheduler.durableRevision('7')
        await vi.advanceTimersByTimeAsync(15_000)
        await vi.advanceTimersByTimeAsync(300_000)

        expect(request).toHaveBeenCalledOnce()
        expect(scheduler.pendingRevision('sync-1', 'sync')).toBeUndefined()
    })
})
