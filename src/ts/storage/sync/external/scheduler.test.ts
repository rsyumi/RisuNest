import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createExternalStorageScheduler } from './scheduler'
import type { ExternalStorageController } from './controller'

describe('external storage scheduler', () => {
    beforeEach(() => vi.useFakeTimers())

    function harness() {
        const request = vi.fn(async input => ({
            kind: 'complete' as const,
            revision: input.targetRevision,
            job: {} as never,
        }))
        const cancel = vi.fn(async () => {})
        const controller = { request, cancel } as unknown as ExternalStorageController
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => true,
            destinations: () => [{ connectionId: 'destination-1', kind: 'backup' }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
        })
        return { scheduler, request, cancel }
    }

    it('runs idle maintenance once across several backups and observes its cooldown', async () => {
        const request = vi.fn(async input => ({ kind: 'complete' as const, revision: input.targetRevision, job: {} as never }))
        const scheduler = createExternalStorageScheduler({ request, cancel: vi.fn() } as unknown as ExternalStorageController, {
            available: () => true,
            destinations: () => [{ connectionId: 'backup-1', kind: 'backup' }],
            maintenance: () => [{ connectionId: 'backup-1' }],
            session: () => ({ kind: 'foreground', id: 'session' }),
        })
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request.mock.calls.map(([input]) => input.kind)).toEqual(['backup'])
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request.mock.calls.map(([input]) => input.kind)).toEqual(['backup', 'cleanup'])
        for (let value = 2; value <= 4; value++) {
            scheduler.durableRevision(String(value) as `${number}`)
            await vi.advanceTimersByTimeAsync(60_000)
        }
        expect(request.mock.calls.filter(([input]) => input.kind === 'cleanup')).toHaveLength(1)
        scheduler.stop()
    })

    it('defers automatic maintenance while offline or a write is in flight', async () => {
        let available = true
        let finish!: (value: unknown) => void
        const request = vi.fn(input => input.kind === 'backup'
            ? new Promise(resolve => { finish = resolve })
            : Promise.resolve({ kind: 'complete', revision: '0', job: {} }))
        const scheduler = createExternalStorageScheduler({ request, cancel: vi.fn() } as unknown as ExternalStorageController, {
            available: () => available,
            destinations: () => [{ connectionId: 'backup-1', kind: 'backup' }],
            maintenance: () => [{ connectionId: 'backup-1' }],
            session: () => ({ kind: 'foreground', id: 'session' }),
        })
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(180_000)
        expect(request).toHaveBeenCalledTimes(1)
        finish({ kind: 'complete', revision: '1', job: {} })
        available = false
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).toHaveBeenCalledTimes(1)
        available = true
        scheduler.resume()
        await vi.advanceTimersByTimeAsync(0)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ kind: 'cleanup' }))
        scheduler.stop()
    })

    it('coalesces edits to the latest revision after 60 seconds of quiet', async () => {
        const { scheduler, request } = harness()
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(59_000)
        scheduler.durableRevision('2')
        await vi.advanceTimersByTimeAsync(59_000)
        scheduler.durableRevision('30')
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(59_999)
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledOnce()
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ targetRevision: '30' }))
    })

    it('does not let continuous edits extend the five minute maximum', async () => {
        const { scheduler, request } = harness()
        scheduler.durableRevision('1')
        for (let index = 2; index <= 6; index += 1) {
            await vi.advanceTimersByTimeAsync(50_000)
            scheduler.durableRevision(String(index) as `${number}`)
        }
        await vi.advanceTimersByTimeAsync(49_999)
        expect(request).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ targetRevision: '6' }))
    })

    it('uses the 60 second quiet and five minute maximum backup policy', async () => {
        const { scheduler, request } = harness()
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
            destinations: () => [{ connectionId: 'backup-1', kind: 'backup' }],
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
        await scheduler.requestNow('destination-1', 'backup', '10')
        expect(request).toHaveBeenNthCalledWith(1, expect.objectContaining({
            reason: 'manual', targetRevision: '10',
        }))
        await vi.advanceTimersByTimeAsync(60_000)
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
                    action: 'wait' as const, retryAtMs: '80000' as const,
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
            destinations: () => [{ connectionId: 'backup-1', kind: 'backup' }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
            now: () => now,
            wallNow: () => now,
        })
        scheduler.durableRevision('7')
        now = 60_000
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).toHaveBeenCalledOnce()
        now = 79_999
        await vi.advanceTimersByTimeAsync(19_999)
        expect(request).toHaveBeenCalledOnce()
        now = 80_000
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledTimes(2)
    })

    it.each(['retry', 'wait'])('does not reschedule a non-retryable %s response', async (action) => {
        const { scheduler, request } = harness()
        request.mockResolvedValue({
            kind: 'blocked', reason: 'permanent-rejection',
            error: { code: 'corrupt', message: 'Rejected', retryable: false, action, retryAtMs: '20000' },
        } as never)
        scheduler.durableRevision('7')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.resume()
        await vi.advanceTimersByTimeAsync(300_000)
        expect(request).toHaveBeenCalledOnce()
        await scheduler.requestNow('destination-1', 'backup', '7')
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
            destinations: () => [{ connectionId: 'backup-1', kind: 'backup' }],
            session: () => ({ kind: 'foreground', id: 'foreground-1' }),
        })

        scheduler.durableRevision('7')
        await vi.advanceTimersByTimeAsync(60_000)
        await vi.advanceTimersByTimeAsync(300_000)

        expect(request).toHaveBeenCalledOnce()
    })
    it.each(['endpointRejected', 'repositoryMismatch'])('does not automatically retry a native %s start rejection', async kind => {
        const request = vi.fn(async () => ({ kind: 'blocked' as const, reason: kind, cause: { kind } }))
        const scheduler = createExternalStorageScheduler({ request, cancel: vi.fn() } as unknown as ExternalStorageController, {
            available: () => true, destinations: () => [{ connectionId: 'backup', kind: 'backup' }],
            session: () => ({ kind: 'foreground', id: 'session' }),
        })
        scheduler.durableRevision('7')
        await vi.advanceTimersByTimeAsync(300_000)
        expect(request).toHaveBeenCalledOnce()
        scheduler.stop()
    })

    it('does not dispatch durable revisions while suspended', async () => {
        const { scheduler, request } = harness()
        await scheduler.suspend()
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).not.toHaveBeenCalled()
        scheduler.resume()
        await vi.advanceTimersByTimeAsync(0)
        expect(request).toHaveBeenCalledOnce()
        scheduler.stop()
    })

    it('keeps one automatic request and one latest target across 30 saves while a job is held', async () => {
        const { scheduler, request } = harness()
        let finish!: (value: { kind: 'complete'; revision: string; job: never }) => void
        request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(61_000)
        for (let revision = 2; revision <= 30; revision++) {
            scheduler.durableRevision(String(revision) as `${number}`)
            await vi.advanceTimersByTimeAsync(61_000)
        }
        expect(request).toHaveBeenCalledTimes(1)
        finish({ kind: 'complete', revision: '1', job: {} as never })
        await vi.advanceTimersByTimeAsync(0)
        expect(request.mock.calls.map(([input]) => input.targetRevision)).toEqual(['1', '30'])
        scheduler.stop()
    })

    it('discards a pending revision already achieved by the held job', async () => {
        const { scheduler, request } = harness()
        let finish!: (value: { kind: 'complete'; revision: string; job: never }) => void
        request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(60_000)
        finish({ kind: 'complete', revision: '9', job: {} as never })
        await vi.advanceTimersByTimeAsync(300_000)
        expect(request).toHaveBeenCalledTimes(1)
        scheduler.stop()
    })

    it('preserves a service retry deadline and newer target after another save', async () => {
        vi.setSystemTime(0)
        const { scheduler, request } = harness()
        request.mockResolvedValueOnce({ kind: 'blocked', reason: 'rateLimited', error: {
            code: 'rateLimited', message: 'Wait', retryable: true, action: 'retry', retryAtMs: '240000',
        } } as never)
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(179_999)
        expect(request).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        expect(request).toHaveBeenCalledTimes(2)
        scheduler.stop()
    })

    it('an older blocked result cannot replace the latest pending revision', async () => {
        vi.setSystemTime(0)
        const { scheduler, request } = harness()
        let finish!: (value: never) => void
        request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.durableRevision('30')
        await vi.advanceTimersByTimeAsync(61_000)
        finish({ kind: 'blocked', reason: 'rateLimited', error: {
            code: 'rateLimited', message: 'Wait', retryable: true, action: 'retry', retryAtMs: '180000',
        } } as never)
        await vi.advanceTimersByTimeAsync(58_999)
        expect(request).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '30' }))
        scheduler.stop()
    })

    it('keeps manual ownership while automatic saves coalesce and stop prevents a follow-up', async () => {
        const { scheduler, request, cancel } = harness()
        let finish!: (value: { kind: 'complete'; revision: string; job: never }) => void
        request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        const manual = scheduler.requestNow('destination-1', 'backup', '1')
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(300_000)
        expect(request).toHaveBeenCalledTimes(1)
        expect(cancel).not.toHaveBeenCalled()
        scheduler.stop()
        finish({ kind: 'complete', revision: '1', job: {} as never })
        expect((await manual).kind).toBe('complete')
        await vi.advanceTimersByTimeAsync(300_000)
        expect(request).toHaveBeenCalledTimes(1)
    })

    it.each([
        ['its own connection', { reason: 'reauthRequired' }, 'destination-1'],
        // A backup reads an asset through the storage connection that holds it.
        ['the connection its read was refused by', { reason: 'reauthRequired', error: {
            code: 'reauthRequired', message: '', retryable: false, action: 'reauthenticate', connectionId: 'drive-2',
        } }, 'drive-2'],
    ] as const)('resumes a remediation-blocked latest target only after verified recovery of %s', async (_, blocked, recovers) => {
        const { scheduler, request } = harness()
        request.mockResolvedValueOnce({ kind: 'blocked', ...blocked } as never)
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.durableRevision('9')
        scheduler.resume()
        scheduler.recovered('unrelated')
        await vi.advanceTimersByTimeAsync(300_000)
        expect(request).toHaveBeenCalledTimes(1)
        scheduler.recovered(recovers)
        await vi.advanceTimersByTimeAsync(0)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        scheduler.stop()
    })

    it.each([
        { kind: 'blocked', reason: 'clockSkew' },
        { kind: 'blocked', reason: 'clockSkew', error: { code: 'clockSkew', retryable: false, action: 'retry' } },
        { kind: 'blocked', reason: 'preconditionFailed', error: { code: 'preconditionFailed', retryable: false, action: 'retry' } },
        { kind: 'blocked', reason: 'repositoryBusy', error: { code: 'repositoryBusy', retryable: true, action: 'wait' } },
    ])('lets a new save retry $reason without repeating an unchanged attempt', async blocked => {
        const { scheduler, request } = harness()
        request.mockResolvedValueOnce(blocked as never)
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(600_000)
        expect(request).toHaveBeenCalledTimes(1)
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(59_999)
        expect(request).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledTimes(2)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        scheduler.stop()
    })

    it('limits precondition retries until a new save arrives', async () => {
        const { scheduler, request } = harness()
        request.mockResolvedValue({ kind: 'blocked', reason: 'preconditionFailed' } as never)
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(600_000)
        expect(request).toHaveBeenCalledTimes(3)
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).toHaveBeenCalledTimes(4)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        scheduler.stop()
    })

    it('retains a newer save when an older request must wait without a deadline', async () => {
        const { scheduler, request } = harness()
        let finish!: (value: never) => void
        request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(60_000)
        finish({ kind: 'blocked', reason: 'repositoryBusy', error: {
            code: 'repositoryBusy', retryable: true, action: 'wait',
        } } as never)
        await vi.advanceTimersByTimeAsync(0)
        expect(request).toHaveBeenCalledTimes(2)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        scheduler.stop()
    })

    it.each([
        { reason: 'storageFull' },
        { reason: 'localPermissionDenied' },
        { reason: 'corrupt' },
        { reason: 'repositoryMismatch' },
        { reason: 'publication-unknown' },
        { reason: 'publication-unknown', error: { code: 'preconditionFailed', retryable: false, action: 'retry' } },
    ])('does not retry $reason merely because another save arrives', async blocked => {
        const { scheduler, request } = harness()
        request.mockResolvedValueOnce({ kind: 'blocked', ...blocked } as never)
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        scheduler.durableRevision('9')
        await vi.advanceTimersByTimeAsync(600_000)
        expect(request).toHaveBeenCalledTimes(1)
        scheduler.stop()
    })

    it('retains the observed retry wait across wall-clock jumps and recovery', async () => {
        vi.setSystemTime(0)
        const { scheduler, request } = harness()
        request.mockResolvedValueOnce({ kind: 'blocked', reason: 'rateLimited', error: {
            code: 'rateLimited', message: 'Wait', retryable: true, action: 'retry', retryAtMs: '240000',
        } } as never)
        scheduler.durableRevision('1')
        await vi.advanceTimersByTimeAsync(60_000)
        vi.setSystemTime(10_000_000)
        scheduler.durableRevision('9')
        scheduler.recovered('destination-1')
        await vi.advanceTimersByTimeAsync(179_999)
        expect(request).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(1)
        expect(request).toHaveBeenCalledTimes(2)
        expect(request).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        scheduler.stop()
    })

    it('compares persisted cleanup timestamps against wall time after monotonic scheduling starts', async () => {
        vi.setSystemTime(1_791_600_000_000)
        const request = vi.fn(async input => ({ kind: 'complete', revision: input.targetRevision, job: {} }))
        const scheduler = createExternalStorageScheduler({ request, cancel: vi.fn() } as unknown as ExternalStorageController, {
            available: () => true, destinations: () => [],
            maintenance: () => [{ connectionId: 'backup-1', lastAttemptAt: Date.now() - 7 * 60 * 60 * 1000 }],
            session: () => ({ kind: 'foreground', id: 'session' }),
        })
        scheduler.resume()
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).toHaveBeenCalledTimes(1)
        expect(request).toHaveBeenCalledWith(expect.objectContaining({ kind: 'cleanup' }))
        await vi.advanceTimersByTimeAsync(60_000)
        expect(request).toHaveBeenCalledTimes(1)
        scheduler.stop()
    })

})
