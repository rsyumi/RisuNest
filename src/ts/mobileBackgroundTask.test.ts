import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
const mocks = vi.hoisted(() => ({ os: 'android' as 'android' | 'ios', toast: vi.fn() }))
vi.mock('./platform', async () => (await import('./platform.testSupport')).platformModule(() => ({ runtime: 'tauri', os: mocks.os })))
vi.mock('./androidNativeControl', () => ({}))
vi.mock('./alert', () => ({ alertToast: mocks.toast }))
import { language } from '../lang'
import { BackgroundExecutionExpiredError, beginIOSGeneration } from './iosNative'
import { beginDeferredMobileBackgroundTask, beginMobileBackgroundTask, hasMobileBackgroundTasks, measuredTaskPercent, mobileDeferredHoldsSettled, runWithMobileBackgroundTask } from './mobileBackgroundTask'
import { runSharedNativeFileOperation, cancelActiveNativeFileOperation } from './storage/nativeFileJobManager'
import { syntheticNativeFileJobStatus, type NativeFileJobStatus } from './storage/nativeFileJobs'
import { createExternalStorageController } from './storage/sync/external/controller'
import type { ExternalJobSummary, ExternalStorageState } from './storage/sync/external/types'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>(done => { resolve = done })
    return { promise, resolve }
}
const bridge = { begin: vi.fn(), progress: vi.fn(), end: vi.fn() }
/** The id TypeScript chose for the nth begin, which every later call for that hold uses. */
const id = (index = 0): string => bridge.begin.mock.calls[index][0]
beforeEach(() => {
    mocks.os = 'android'
    mocks.toast.mockReset()
    bridge.begin.mockReset().mockImplementation(async (_id: string, _kind: string, deferred: boolean) => deferred ? 'deferred' : 'active')
    bridge.progress.mockReset().mockResolvedValue(undefined)
    bridge.end.mockReset().mockResolvedValue(undefined)
    window.RisuBackgroundTasks = bridge
})
afterEach(() => {
    delete window.RisuBackgroundTasks
    expect(hasMobileBackgroundTasks()).toBe(false)
})

describe('mobile background task lifetime', () => {
    it('retains pending admission and overlapping owners until each finishes, ending each by its own id', async () => {
        const admission = deferred<string>()
        bridge.begin.mockReturnValueOnce(admission.promise)
        const first = beginMobileBackgroundTask('backup')
        expect(hasMobileBackgroundTasks()).toBe(true)
        const second = await beginMobileBackgroundTask('sync')
        admission.resolve('active')
        const task = await first
        await second.dispose(true)
        expect(hasMobileBackgroundTasks()).toBe(true)
        await task.dispose(true)
        await task.dispose(true)
        expect(bridge.begin.mock.calls.map(call => call.slice(1))).toEqual([['backup', false, false], ['sync', false, false]])
        expect(id(0)).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
        expect(bridge.end.mock.calls).toEqual([[id(1)], [id(0)]])
    })

    it('asks native again to end a hold while its control channel is full, a bounded number of times', async () => {
        vi.useFakeTimers()
        try {
            const busy = new Error('android-control-busy')
            bridge.end.mockRejectedValueOnce(busy).mockRejectedValueOnce(busy)
            const ended = await beginMobileBackgroundTask('backup')
            const ending = ended.dispose(true)
            await vi.runAllTimersAsync()
            await ending
            expect(bridge.end.mock.calls).toEqual([[id(0)], [id(0)], [id(0)]])
            bridge.end.mockReset().mockRejectedValue(busy)
            const refused = await beginMobileBackgroundTask('backup')
            const giving = refused.dispose(true)
            await vi.runAllTimersAsync()
            await giving
            expect(bridge.end).toHaveBeenCalledTimes(5)
            expect(hasMobileBackgroundTasks()).toBe(false)
        } finally { vi.useRealTimers() }
    })

    it.each([
        ['background', { state: 'refused', reason: 'background' }],
        ['failed', { state: 'refused', reason: 'failed' }],
        [new Error('android-control-timeout'), { state: 'refused', reason: 'timeout' }],
        [new Error('unavailable'), { state: 'refused', reason: 'failed' }],
    ] as const)('runs the work and ends a hold whose admission was refused: %s', async (outcome, protection) => {
        if (outcome instanceof Error) bridge.begin.mockRejectedValueOnce(outcome)
        else bridge.begin.mockResolvedValueOnce(outcome)
        await expect(runWithMobileBackgroundTask('restore', async task => task.protection)).resolves.toEqual(protection)
        // Native may still admit a late begin, so the hold always ends.
        expect(bridge.end).toHaveBeenCalledExactlyOnceWith(id())
    })

    it('marks user-started work for native and tells the user only about its refused protection', async () => {
        bridge.begin.mockResolvedValue('background')
        await (await beginMobileBackgroundTask('sync')).dispose()
        await (await beginDeferredMobileBackgroundTask('sync')).dispose()
        await vi.waitFor(() => expect(bridge.end).toHaveBeenCalledTimes(2))
        expect(mocks.toast).not.toHaveBeenCalled()
        await (await beginMobileBackgroundTask('sync', undefined, true)).dispose()
        await vi.waitFor(() => expect(mocks.toast).toHaveBeenCalledExactlyOnceWith(language.risuNest.backgroundWorkMayStop))
        expect(bridge.begin.mock.calls.map(call => call.slice(1))).toEqual([['sync', false, false], ['sync', true, false], ['sync', false, true]])
    })

    it('registers a deferred hold that native protects only once the app leaves the screen', async () => {
        const task = await beginDeferredMobileBackgroundTask('sync')
        expect(bridge.begin).toHaveBeenCalledExactlyOnceWith(expect.any(String), 'sync', true, false)
        expect(task.protection).toEqual({ state: 'deferred' })
        await task.dispose()
        expect(bridge.end).toHaveBeenCalledExactlyOnceWith(id())
    })

    it('releases on errors and cancellation during acquisition', async () => {
        await expect(runWithMobileBackgroundTask('export', async () => { throw new Error('failed') })).rejects.toThrow('failed')
        const pending = deferred<string>()
        bridge.begin.mockReturnValueOnce(pending.promise)
        const abort = new AbortController()
        const operation = vi.fn(async () => {})
        const result = runWithMobileBackgroundTask('backup', operation, abort.signal)
        abort.abort()
        pending.resolve('active')
        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(operation).not.toHaveBeenCalled()
        expect(bridge.end.mock.calls).toEqual([[id(0)], [id(1)]])
    })

    it('serializes progress, clears unknown totals, and ignores reports after disposal', async () => {
        const task = await beginMobileBackgroundTask('backup')
        task.progress(40)
        task.progress(40)
        task.progress(null)
        task.progress(20)
        await task.dispose(true)
        task.progress(100)
        expect(bridge.progress.mock.calls).toEqual([[id(), 40], [id(), -1], [id(), 20]])
        expect(bridge.progress.mock.invocationCallOrder.at(-1)!).toBeLessThan(bridge.end.mock.invocationCallOrder[0])
        expect(measuredTaskPercent(2, 8)).toBe(25)
        expect(measuredTaskPercent(2, 0)).toBeNull()
        expect(measuredTaskPercent(2)).toBeNull()
        expect(measuredTaskPercent(Infinity, 8)).toBeNull()
    })

    it('does not abort on Home and handles only its native expiration event', async () => {
        const task = await beginMobileBackgroundTask('sync')
        document.dispatchEvent(new Event('visibilitychange'))
        window.dispatchEvent(new CustomEvent('risunest-background-expired', { detail: 'other' }))
        expect(task.signal?.aborted).toBe(false)
        window.dispatchEvent(new CustomEvent('risunest-background-expired', { detail: id() }))
        expect(task.signal?.aborted).toBe(true)
        expect(hasMobileBackgroundTasks()).toBe(false)
        await task.dispose()
    })

    it('protects the entire file operation and reports the same measured bytes and published items', async () => {
        const finish = deferred<void>()
        let signal: AbortSignal | undefined
        let report: (status: NativeFileJobStatus) => void = () => {}
        const operation = runSharedNativeFileOperation('export', 'synthetic-backup', async context => {
            signal = context.signal
            report = context.onStatus
            context.onStatus({
                jobId: 'synthetic', kind: 'export-portable-backup', state: 'running', phase: 'writing-export',
                progress: { completedBytes: 25, totalBytes: 100, completedItems: 0 },
            })
            await finish.promise
        }, { format: 'library-backup' })
        await vi.waitFor(() => expect(bridge.progress).toHaveBeenCalledWith(id(), 25))
        expect(bridge.begin).toHaveBeenCalledWith(id(), 'backup', false, true)
        report(syntheticNativeFileJobStatus({ kind: 'official-publication-upload' }, 'publishing-destination', {
            stageCompleted: 3, stageTotal: 4, stageUnit: 'items',
        }))
        await vi.waitFor(() => expect(bridge.progress).toHaveBeenLastCalledWith(id(), 75))
        expect(bridge.end).not.toHaveBeenCalled()
        cancelActiveNativeFileOperation()
        expect(signal?.aborted).toBe(true)
        finish.resolve()
        await operation
        expect(bridge.end).toHaveBeenCalledExactlyOnceWith(id())
    })

    it('begins a picker-first file operation hold only once its picker returns', async () => {
        await expect(runSharedNativeFileOperation('import', 'synthetic-picker-cancel', async () => null, {
            format: 'content', protectAfterPicker: true,
        })).resolves.toBeNull()
        expect(bridge.begin).not.toHaveBeenCalled()

        const picker = deferred<void>()
        const finish = deferred<void>()
        let signal: AbortSignal | undefined
        const writing = (completedBytes: number): NativeFileJobStatus => ({
            jobId: 'synthetic', kind: 'export-dataset', state: 'running', phase: 'writing-export',
            progress: { completedBytes, totalBytes: 100, completedItems: 0 },
        })
        const operation = runSharedNativeFileOperation('export', 'synthetic-dataset', async context => {
            signal = context.signal
            await picker.promise
            context.protect()
            context.protect()
            context.onStatus(writing(25))
            await finish.promise
            if (context.signal.aborted) throw new DOMException('Native file job was cancelled', 'AbortError')
        }, { format: 'dataset', protectAfterPicker: true })
        await vi.waitFor(() => expect(signal).toBeDefined())
        expect(bridge.begin).not.toHaveBeenCalled()
        expect(hasMobileBackgroundTasks()).toBe(false)
        picker.resolve()
        await vi.waitFor(() => expect(bridge.progress).toHaveBeenCalledWith(id(), 25))
        expect(bridge.begin).toHaveBeenCalledExactlyOnceWith(id(), 'export', false, true)
        window.dispatchEvent(new CustomEvent('risunest-background-expired', { detail: id() }))
        expect(signal?.aborted).toBe(true)
        finish.resolve()
        await expect(operation).rejects.toBeInstanceOf(BackgroundExecutionExpiredError)
        expect(bridge.end).toHaveBeenCalledExactlyOnceWith(id())
    })

    it('reports the current restore phase and clears completed source bytes while work remains', async () => {
        const copying = (stageCompleted: number, stageTotal?: number) => syntheticNativeFileJobStatus(
            { jobId: 'source', kind: 'restore-legacy-local-backup' }, 'copying-source',
            { stageUnit: 'bytes', stageCompleted, ...(stageTotal === undefined ? {} : { stageTotal }) },
        )
        await runSharedNativeFileOperation('import', 'synthetic-restore', async ({ onStatus }) => {
            onStatus(copying(2 * 1024 ** 3, 8 * 1024 ** 3))
            onStatus(copying(4 * 1024 ** 3))
            onStatus(syntheticNativeFileJobStatus({ kind: 'restore-legacy-local-backup' }, 'reading-database', {
                stageUnit: 'bytes', stageCompleted: 1024, stageTotal: 2048,
            }))
            onStatus({
                jobId: 'restore', kind: 'restore-legacy-local-backup', state: 'running', phase: 'reading-source',
                progress: { completedBytes: 75, totalBytes: 100, completedItems: 0 },
            })
            onStatus({
                jobId: 'restore', kind: 'restore-legacy-local-backup', state: 'running', phase: 'staging-database',
                progress: { completedBytes: 100, totalBytes: 100, completedItems: 0 },
            })
        }, { format: 'library-backup' })
        expect(bridge.begin).toHaveBeenCalledWith(id(), 'restore', false, true)
        expect(bridge.progress.mock.calls).toEqual([[id(), 25], [id(), -1], [id(), 50], [id(), 75], [id(), -1]])
    })

    it('protects external native polling and releases a blocked job', async () => {
        const terminal = deferred<ExternalJobSummary>()
        const job: ExternalJobSummary = { id: 'job', connectionId: 'connection', kind: 'backup', state: 'running', phase: 'upload', completedBytes: '5', totalBytes: '10', completedItems: '0', startedAtMs: '1', updatedAtMs: '1' }
        const native = {
            startJob: vi.fn(async () => job), getJob: vi.fn(() => terminal.promise), cancelJob: vi.fn(async () => ({ ...job, state: 'cancelled' as const })),
        }
        const controller = createExternalStorageController(native, { supported: true, selection: { kind: 'none', selectionEpoch: '0', paused: false }, connections: [], jobs: [] } as ExternalStorageState, { wait: async () => {} })
        const result = runWithMobileBackgroundTask('backup', backgroundTask => controller.request({ connectionId: 'connection', kind: 'backup', targetRevision: '1', reason: 'manual', session: { kind: 'foreground', id: 'session' }, backgroundTask }))
        await vi.waitFor(() => expect(bridge.progress).toHaveBeenCalledWith(id(), 50))
        expect(bridge.begin).toHaveBeenCalledOnce()
        document.dispatchEvent(new Event('visibilitychange'))
        expect(native.cancelJob).not.toHaveBeenCalled()
        terminal.resolve({ ...job, state: 'failed' })
        await expect(result).resolves.toMatchObject({ kind: 'blocked' })
        await vi.waitFor(() => expect(bridge.end).toHaveBeenCalledWith(id()))
    })
})

describe('iOS deferred holds', () => {
    it('keeps the background-entry wait until each hold settles and aborts them when it expires', async () => {
        mocks.os = 'ios'
        const lifecycle = (detail: { event: string; id: string }) => window.dispatchEvent(new CustomEvent('risunest-ios-lifecycle', { detail }))
        const show = (value: 'hidden' | 'visible') => Object.defineProperty(document, 'visibilityState', { configurable: true, value })
        const first = await beginDeferredMobileBackgroundTask('sync')
        const second = await beginDeferredMobileBackgroundTask('generation')
        expect(first.protection).toEqual({ state: 'deferred' })
        const settled = vi.fn()
        void mobileDeferredHoldsSettled().then(settled)
        await first.dispose()
        await Promise.resolve(); await Promise.resolve()
        expect(settled).not.toHaveBeenCalled()
        const own = await beginIOSGeneration(undefined, {
            enabled: () => true, begin: async () => ({ id: 'own-task' }), end: async () => {}, events: window,
            state: async () => ({ activeTasks: ['own-task'], expiredTasks: [], foreground: false, notifications: false, notificationStatus: 0, backgroundMode: 'limited', continuedRegistered: false, continuedErrorCode: null, backgroundRefreshStatus: 2 }),
        })
        try {
            // A task the page began ends only its own work, and an expiry heard back on screen arrived late.
            show('hidden'); lifecycle({ event: 'expired', id: 'own-task' })
            show('visible'); lifecycle({ event: 'expired', id: 'background-entry' })
            expect(own.signal?.aborted).toBe(true)
            expect(second.signal?.aborted).toBe(false)
            // The app left before the page heard the departure, so only the expiry of its assertion arrives.
            show('hidden'); lifecycle({ event: 'expired', id: 'background-entry' })
            expect(second.signal?.reason).toBeInstanceOf(BackgroundExecutionExpiredError)
            await second.dispose()
            // Back on screen and away again: work that runs now ends with the new departure's assertion, not the old one's.
            show('visible')
            const third = await beginDeferredMobileBackgroundTask('sync')
            show('hidden'); lifecycle({ event: 'background', id: 'next-entry' })
            lifecycle({ event: 'expired', id: 'earlier-entry' })
            expect(third.signal?.aborted).toBe(false)
            lifecycle({ event: 'expired', id: 'next-entry' })
            expect(third.signal?.reason).toBeInstanceOf(BackgroundExecutionExpiredError)
            await third.dispose()
        } finally { show('visible'); await own.dispose() }
        await vi.waitFor(() => expect(settled).toHaveBeenCalledOnce())
        expect(bridge.begin).not.toHaveBeenCalled()
    })
})

it.each([false, true])('distinguishes OS expiry after inner cancellation, user cancel wins: %s', async userCancel => {
    const caller = new AbortController()
    const operation = runWithMobileBackgroundTask('import', async task => {
        await new Promise<void>(resolve => task.signal!.addEventListener('abort', () => resolve(), { once: true }))
        if (userCancel) caller.abort()
        throw new DOMException('inner cancellation', 'AbortError')
    }, caller.signal)
    await vi.waitFor(() => expect(hasMobileBackgroundTasks()).toBe(true))
    await vi.waitFor(() => expect(bridge.begin).toHaveBeenCalledOnce())
    await Promise.resolve()
    window.dispatchEvent(new CustomEvent('risunest-background-expired', { detail: id() }))
    const { isBackgroundExpiryReason } = await import('./iosNative')
    await expect(operation.catch(error => isBackgroundExpiryReason(error))).resolves.toBe(!userCancel)
})
