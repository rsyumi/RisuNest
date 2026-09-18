import { describe, expect, it, vi } from 'vitest'
import { createExternalStorageController, type ExternalStorageJobBridge } from './controller'
import type { ExternalJobSummary, ExternalStorageState } from './types'

const state: ExternalStorageState = {
    supported: true,
    selection: {
        kind: 'external',
        connectionId: 'sync-1',
        selectionEpoch: 'opaque-selection',
        paused: false,
        decisionRequired: false,
    },
    connections: [],
    jobs: [],
}

function job(
    id: string,
    status: ExternalJobSummary['state'],
    publishedRevision?: string,
): ExternalJobSummary {
    return {
        id,
        connectionId: 'sync-1',
        kind: 'sync',
        state: status,
        phase: status,
        completedBytes: '0',
        completedItems: '0',
        startedAtMs: '1',
        updatedAtMs: '1',
        result: publishedRevision ? { publishedRevision: publishedRevision as `${number}` } : undefined,
    }
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>(settle => {
        resolve = settle
    })
    return { promise, resolve }
}

describe('external storage controller', () => {
    it('keeps the latest dirty revision while resolving an earlier manual goal', async () => {
        const firstPoll = deferred<ExternalJobSummary>()
        let starts = 0
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async request => job(`job-${++starts}`, 'running', request.targetRevision)),
            getJob: vi.fn(async id => {
                if (id === 'job-1') return firstPoll.promise
                return job(id, 'succeeded', '30')
            }),
            cancelJob: vi.fn(async id => job(id, 'cancelled')),
        }
        const controller = createExternalStorageController(bridge, state, {
            wait: async () => {},
        })
        const manual = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '10',
            reason: 'manual', session: { kind: 'foreground', id: 'session-1' },
        })
        await vi.waitFor(() => expect(bridge.getJob).toHaveBeenCalledWith('job-1'))
        const newer = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '30',
            reason: 'automatic', session: { kind: 'foreground', id: 'session-1' },
        })
        firstPoll.resolve(job('job-1', 'succeeded', '10'))
        await expect(manual).resolves.toMatchObject({ kind: 'complete', revision: '10' })
        await expect(newer).resolves.toMatchObject({ kind: 'complete', revision: '30' })
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
        expect(bridge.startJob).toHaveBeenLastCalledWith(expect.objectContaining({
            targetRevision: '30',
        }))
    })

    it('runs same-revision sync and backup goals as distinct native jobs', async () => {
        const firstPoll = deferred<ExternalJobSummary>()
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async request => request.kind === 'sync'
                ? job('sync-job', 'running')
                : { ...job('backup-job', 'succeeded', '8'), kind: 'backup' as const }),
            getJob: vi.fn(async () => firstPoll.promise),
            cancelJob: vi.fn(),
        }
        const controller = createExternalStorageController(bridge, state, { wait: async () => {} })
        const sync = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '8',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        await vi.waitFor(() => expect(bridge.getJob).toHaveBeenCalledWith('sync-job'))
        const backup = controller.request({
            connectionId: 'sync-1', kind: 'backup', targetRevision: '8',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        firstPoll.resolve(job('sync-job', 'succeeded', '8'))

        await expect(sync).resolves.toMatchObject({ kind: 'complete', job: { id: 'sync-job' } })
        await expect(backup).resolves.toMatchObject({ kind: 'complete', job: { id: 'backup-job' } })
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
        expect(bridge.startJob).toHaveBeenNthCalledWith(1, expect.objectContaining({ kind: 'sync' }))
        expect(bridge.startJob).toHaveBeenNthCalledWith(2, expect.objectContaining({ kind: 'backup' }))
    })

    it('runs another kind after a blocked attempt without sharing its result', async () => {
        const blocked = {
            ...job('sync-job', 'waiting'),
            phase: 'paused',
            error: { code: 'offline', message: 'Offline', retryable: true, action: 'retry' as const },
        }
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async request => request.kind === 'sync'
                ? blocked
                : { ...job('backup-job', 'succeeded', '8'), kind: 'backup' as const }),
            getJob: vi.fn(),
            cancelJob: vi.fn(),
        }
        const controller = createExternalStorageController(bridge, state)
        const sync = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '8',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        const backup = controller.request({
            connectionId: 'sync-1', kind: 'backup', targetRevision: '8',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })

        await expect(sync).resolves.toMatchObject({ kind: 'blocked', job: { id: 'sync-job' } })
        await expect(backup).resolves.toMatchObject({ kind: 'complete', job: { id: 'backup-job' } })
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
    })

    it('uses the exit drain session for a queued newer goal', async () => {
        const firstPoll = deferred<ExternalJobSummary>()
        let starts = 0
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => job(`job-${++starts}`, 'running')),
            getJob: vi.fn(async id => id === 'job-1'
                ? firstPoll.promise
                : job(id, 'succeeded', '12')),
            cancelJob: vi.fn(async id => job(id, 'cancelled')),
        }
        const controller = createExternalStorageController(bridge, state, { wait: async () => {} })
        const automatic = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '10',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
        })
        await vi.waitFor(() => expect(bridge.getJob).toHaveBeenCalled())
        const abort = new AbortController()
        const drain = controller.drainToRevision(
            'sync-1',
            {
                revision: 12,
                libraryEpoch: 'library-1',
                selectionEpoch: 'selection-1',
                selectionId: 'selected',
            },
            'exit-1',
            abort.signal,
        )
        const backup = controller.request({
            connectionId: 'sync-1', kind: 'backup', targetRevision: '12',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        firstPoll.resolve(job('job-1', 'succeeded', '10'))
        await expect(automatic).resolves.toMatchObject({ kind: 'complete' })
        await expect(drain).resolves.toEqual({ kind: 'complete' })
        await expect(backup).resolves.toMatchObject({ kind: 'complete' })
        expect(bridge.startJob).toHaveBeenNthCalledWith(2, expect.objectContaining({
            targetRevision: '12', reason: 'exitDrain', session: 'exitDrain', sessionId: 'exit-1',
        }))
        expect(bridge.startJob).toHaveBeenNthCalledWith(3, expect.objectContaining({
            targetRevision: '12', kind: 'backup', session: 'foreground', sessionId: 'foreground-1',
        }))
    })

    it('aborts an active exit drain without discarding a coalesced foreground goal', async () => {
        let starts = 0
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => ++starts === 1
                ? job('exit-job', 'running')
                : job('foreground-job', 'succeeded', '20')),
            getJob: vi.fn(),
            cancelJob: vi.fn(async id => job(id, 'cancelled')),
        }
        const controller = createExternalStorageController(bridge, state, {
            wait: async (_delay, signal) => new Promise<void>((resolve, reject) => {
                if (signal?.aborted) reject(signal.reason)
                else signal?.addEventListener('abort', () => reject(signal.reason), { once: true })
            }),
        })
        const abort = new AbortController()
        const removeListener = vi.spyOn(abort.signal, 'removeEventListener')
        const drain = controller.drainToRevision(
            'sync-1',
            {
                revision: 12,
                libraryEpoch: 'library-1',
                selectionEpoch: 'selection-1',
                selectionId: 'selected',
            },
            'exit-1',
            abort.signal,
        )
        await vi.waitFor(() => expect(bridge.startJob).toHaveBeenCalledTimes(1))
        const foreground = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '20',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
        })

        abort.abort(new Error('exit cancelled'))

        await expect(drain).resolves.toEqual({ kind: 'blocked', reason: 'cancelled' })
        await vi.waitFor(() => expect(bridge.cancelJob).toHaveBeenCalledWith('exit-job'))
        await expect(foreground).resolves.toMatchObject({ kind: 'complete', revision: '20' })
        expect(removeListener).toHaveBeenCalledWith('abort', expect.any(Function))
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
        expect(bridge.startJob).toHaveBeenLastCalledWith(expect.objectContaining({
            reason: 'automatic', session: 'foreground', sessionId: 'foreground-1',
        }))
    })

    it('cancels a native exit job returned after its caller already aborted', async () => {
        const started = deferred<ExternalJobSummary>()
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => started.promise),
            getJob: vi.fn(),
            cancelJob: vi.fn(async id => job(id, 'cancelled')),
        }
        const controller = createExternalStorageController(bridge, state)
        const abort = new AbortController()
        const drain = controller.drainToRevision(
            'sync-1',
            {
                revision: 12,
                libraryEpoch: 'library-1',
                selectionEpoch: 'selection-1',
                selectionId: 'selected',
            },
            'exit-1',
            abort.signal,
        )
        abort.abort(new Error('exit cancelled'))
        started.resolve(job('late-exit-job', 'running'))

        await expect(drain).resolves.toEqual({ kind: 'blocked', reason: 'cancelled' })
        await vi.waitFor(() => expect(bridge.cancelJob).toHaveBeenCalledWith('late-exit-job'))
        expect(bridge.getJob).not.toHaveBeenCalled()
    })

    it.each(['sync', 'backup'] as const)('cancels a late-started %s job after the connection was cancelled', async kind => {
        const started = deferred<ExternalJobSummary>()
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => started.promise),
            getJob: vi.fn(),
            cancelJob: vi.fn(async id => ({ ...job(id, 'cancelled'), kind })),
        }
        const controller = createExternalStorageController(bridge, state)
        const pending = controller.request({
            connectionId: 'sync-1', kind, targetRevision: '12',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        expect(bridge.startJob).toHaveBeenCalledTimes(1)
        const cancelled = controller.cancel('sync-1')
        started.resolve({ ...job('late-job', 'running'), kind })

        await cancelled
        await expect(pending).resolves.toEqual({ kind: 'cancelled' })
        expect(bridge.cancelJob).toHaveBeenCalledExactlyOnceWith('late-job')
        expect(bridge.getJob).not.toHaveBeenCalled()
    })

    it('keeps a replacement request separate until the cancelled native start is settled', async () => {
        const started = deferred<ExternalJobSummary>()
        const cancelled = deferred<ExternalJobSummary>()
        let starts = 0
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => ++starts === 1
                ? started.promise
                : job('replacement-job', 'succeeded', '20')),
            getJob: vi.fn(async id => job(id, 'succeeded', '12')),
            cancelJob: vi.fn(async () => cancelled.promise),
        }
        const controller = createExternalStorageController(bridge, state, { wait: async () => {} })
        const first = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '12',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        const stopping = controller.cancel('sync-1')
        const replacement = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '20',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
        })
        started.resolve(job('late-job', 'running'))

        await vi.waitFor(() => expect(bridge.cancelJob).toHaveBeenCalledExactlyOnceWith('late-job'))
        expect(bridge.startJob).toHaveBeenCalledTimes(1)
        expect(bridge.getJob).not.toHaveBeenCalled()
        cancelled.resolve(job('late-job', 'cancelled'))
        await stopping
        await expect(first).resolves.toEqual({ kind: 'cancelled' })
        await expect(replacement).resolves.toMatchObject({ kind: 'complete', revision: '20' })
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
    })

    it.each(['connection', 'caller'] as const)('does not start a queued replacement cancelled by its %s', async cancellation => {
        const started = deferred<ExternalJobSummary>()
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => started.promise),
            getJob: vi.fn(),
            cancelJob: vi.fn(async id => job(id, 'cancelled')),
        }
        const controller = createExternalStorageController(bridge, state)
        const first = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '12',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })
        const stopping = controller.cancel('sync-1')
        const abort = new AbortController()
        const replacement = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '20',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
            signal: abort.signal,
        })
        const stopReplacement = cancellation === 'connection'
            ? controller.cancel('sync-1')
            : Promise.resolve(abort.abort())
        started.resolve(job('late-job', 'running'))

        await Promise.all([stopping, stopReplacement])
        await expect(first).resolves.toEqual({ kind: 'cancelled' })
        await expect(replacement).resolves.toEqual({ kind: 'cancelled' })
        expect(bridge.startJob).toHaveBeenCalledTimes(1)
        expect(bridge.cancelJob).toHaveBeenCalledExactlyOnceWith('late-job')
        expect(bridge.getJob).not.toHaveBeenCalled()
    })

    it('does not apply a remote receive returned after abort and waits for native cancellation', async () => {
        const polled = deferred<ExternalJobSummary>()
        const cancelled = deferred<ExternalJobSummary>()
        const applyReceived = vi.fn(async () => {})
        let starts = 0
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => ++starts === 1
                ? job('exit-job', 'running')
                : job('foreground-job', 'succeeded', '20')),
            getJob: vi.fn(async () => polled.promise),
            cancelJob: vi.fn(async () => cancelled.promise),
        }
        const controller = createExternalStorageController(bridge, state, {
            applyReceived,
            wait: async () => {},
        })
        const abort = new AbortController()
        const drain = controller.drainToRevision(
            'sync-1',
            {
                revision: 12,
                libraryEpoch: 'library-1',
                selectionEpoch: 'selection-1',
                selectionId: 'selected',
            },
            'exit-1',
            abort.signal,
        )
        await vi.waitFor(() => expect(bridge.getJob).toHaveBeenCalledWith('exit-job'))
        const foreground = controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '20',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
        })

        abort.abort(new Error('exit cancelled'))
        polled.resolve({
            ...job('exit-job', 'waiting'),
            phase: 'remote-apply',
            result: { receiveReady: true, expectedRevision: '12' },
        })

        await expect(drain).resolves.toEqual({ kind: 'blocked', reason: 'cancelled' })
        await vi.waitFor(() => expect(bridge.cancelJob).toHaveBeenCalledWith('exit-job'))
        expect(applyReceived).not.toHaveBeenCalled()
        expect(bridge.startJob).toHaveBeenCalledTimes(1)
        cancelled.resolve(job('exit-job', 'cancelled'))
        await expect(foreground).resolves.toMatchObject({ kind: 'complete', revision: '20' })
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
    })

    it('does not accept success without native publication evidence', async () => {
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => job('job-1', 'succeeded')),
            getJob: vi.fn(),
            cancelJob: vi.fn(),
        }
        const controller = createExternalStorageController(bridge, state)
        await expect(controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '10',
            reason: 'manual', session: { kind: 'foreground', id: 'foreground-1' },
        })).resolves.toEqual(expect.objectContaining({
            kind: 'blocked', reason: 'external-storage-published-revision-missing',
        }))
    })

    it('backs off native polling while a persisted job waits', async () => {
        const waits: number[] = []
        const capturing = { ...job('job-1', 'waiting'), phase: 'device-capture' }
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => capturing),
            getJob: vi.fn(async () => job('job-1', 'succeeded', '4')),
            cancelJob: vi.fn(),
        }
        const controller = createExternalStorageController(bridge, state, {
            wait: async delay => { waits.push(delay) },
        })
        await controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '4',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
        })
        expect(waits).toEqual([5_000])
    })

    it('applies a staged remote receive before accepting native completion', async () => {
        const applyReceived = vi.fn(async () => {})
        let starts = 0
        const ready = {
            ...job('job-1', 'waiting'),
            phase: 'remote-apply',
            result: { receiveReady: true, expectedRevision: '4' as const },
        }
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => ++starts === 1 ? ready : job('job-2', 'succeeded', '5')),
            getJob: vi.fn(async () => ({
                ...job('job-1', 'succeeded'),
                result: { snapshotId: 'remote', receivedRevision: '5' as const },
            })),
            cancelJob: vi.fn(),
        }
        const controller = createExternalStorageController(bridge, state, { applyReceived })
        await expect(controller.request({
            connectionId: 'sync-1', kind: 'sync', targetRevision: '4',
            reason: 'automatic', session: { kind: 'foreground', id: 'foreground-1' },
        })).resolves.toMatchObject({ kind: 'complete', revision: '5' })
        expect(applyReceived).toHaveBeenCalledOnce()
        expect(applyReceived).toHaveBeenCalledWith(ready)
        expect(bridge.getJob).toHaveBeenCalledWith('job-1')
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
    })

    it('returns a paused native wait and lets an explicit retry resume the retained job', async () => {
        let attempts = 0
        const paused = {
            ...job('job-1', 'waiting'),
            phase: 'paused',
            error: {
                code: 'offline', message: 'Offline', retryable: true, action: 'retry' as const,
            },
        }
        const bridge: ExternalStorageJobBridge = {
            startJob: vi.fn(async () => ++attempts === 1
                ? paused
                : job('job-1', 'succeeded', '4')),
            getJob: vi.fn(),
            cancelJob: vi.fn(),
        }
        const controller = createExternalStorageController(bridge, state)
        const request = {
            connectionId: 'sync-1', kind: 'sync' as const, targetRevision: '4' as const,
            reason: 'manual' as const, session: { kind: 'foreground' as const, id: 'foreground-1' },
        }
        await expect(controller.request(request)).resolves.toMatchObject({
            kind: 'blocked', reason: 'offline', job: paused,
        })
        await expect(controller.request(request)).resolves.toMatchObject({
            kind: 'complete', revision: '4',
        })
        expect(bridge.startJob).toHaveBeenCalledTimes(2)
        expect(bridge.getJob).not.toHaveBeenCalled()
    })
})
