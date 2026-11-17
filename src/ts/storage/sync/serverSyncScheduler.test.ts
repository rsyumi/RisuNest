import { afterEach, describe, expect, it, vi } from 'vitest'
import { createServerSyncScheduler, isServerSyncUnavailable } from './serverSyncScheduler'
import { createPluginStartupGate } from '../../plugins/pluginStartupGate'

afterEach(() => vi.useRealTimers())
function fixture() {
    vi.useFakeTimers()
    const dependencies = { push: vi.fn(async () => {}), pull: vi.fn(async () => {}), connect: vi.fn(async (_operationId: string) => {}), disconnect: vi.fn(async () => {}), stopNotifications: vi.fn(async () => {}), retryClock: vi.fn(async () => {}), checkFormat: vi.fn(async () => {}), failed: vi.fn(), recovered: vi.fn() }
    return { ...dependencies, scheduler: createServerSyncScheduler(dependencies) }
}
describe('server LWW foreground scheduler', () => {
    it('classifies tunnel outages and timeouts without masking authentication or local failures', () => {
        for (const status of [502, 503, 504, 530]) expect(isServerSyncUnavailable({ code: 'server-response-error', status })).toBe(true)
        for (const code of ['server-timeout', 'server-updating', 'incomplete-response', 'sync-retry-budget-exhausted']) expect(isServerSyncUnavailable({ code })).toBe(true)
        // An activation's server check marks its failures with startupLocalReady.
        for (const status of [429, 500, 503]) expect(isServerSyncUnavailable({ code: 'worker-unavailable', status, retryable: true, startupLocalReady: false })).toBe(true)
        for (const error of [{ code: 'server-response-error', status: 401 }, { code: 'local-storage', status: 503 }, { code: 'server-unreachable', status: 409, retryable: false }, new TypeError('Load failed'),
            { code: 'device-busy', status: 429, retryable: true }, { code: 'worker-unavailable', status: 503, retryable: true }, { code: 'server-response-error', status: 409, retryable: false, startupLocalReady: true }]) {
            expect(isServerSyncUnavailable(error)).toBe(false)
        }
    })
    it.each(['timer', 'edit', 'foreground'])('retries a cancelled publication that failed with a retryable server reply on %s', async trigger => {
        const f = fixture()
        const failure = { code: 'worker-unavailable', status: 503, retryable: true }
        f.push.mockRejectedValueOnce(failure)
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.failed).toHaveBeenCalledWith(failure, false)
        expect(f.scheduler.isBlocked()).toBe(false)
        if (trigger === 'edit') f.scheduler.localChange(true)
        else if (trigger === 'foreground') { await f.scheduler.foreground(false); await f.scheduler.foreground(true) }
        await vi.advanceTimersByTimeAsync(trigger === 'timer' ? 1000 : 0)
        expect(f.push).toHaveBeenCalledTimes(2)
        expect(f.recovered).toHaveBeenCalledOnce()
        expect(f.scheduler.blockedCode()).toBe('')
        f.scheduler.dispose()
    })
    it.each(['server-unreachable', 'sync-retry-budget-exhausted'])('backs off %s across all automatic triggers and resumes queued work after recovery', async code => {
        const f = fixture()
        const offline = { code, retryable: true }
        f.push.mockRejectedValue(offline); f.pull.mockRejectedValue(offline); f.connect.mockRejectedValue(offline)
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        for (const [index, delay] of [5000, 10000, 20000, 40000, 80000, 160000, 300000, 300000].entries()) {
            f.scheduler.localChange(true); f.scheduler.remoteHint(); await f.scheduler.conversationOpened()
            await f.scheduler.foreground(false, true); await f.scheduler.foreground(true)
            await f.scheduler.resumeAutomaticSync(); f.scheduler.socket(false)
            await vi.advanceTimersByTimeAsync(delay - 1)
            expect([f.push.mock.calls.length, f.pull.mock.calls.length, f.connect.mock.calls.length]).toEqual([index + 1, index + 1, index + 1])
            await vi.advanceTimersByTimeAsync(1)
        }
        f.push.mockResolvedValue(); f.pull.mockResolvedValue(); f.connect.mockResolvedValue()
        await vi.advanceTimersByTimeAsync(300000)
        expect(f.scheduler.isWaiting()).toBe(false)
        expect(f.recovered).toHaveBeenCalledOnce()
        f.push.mockClear(); f.scheduler.localChange(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.push).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it('lets an explicit retry probe during a cooldown and preserves explicit receive failures', async () => {
        const f = fixture()
        const offline = { code: 'server-unreachable', retryable: true }
        f.pull.mockRejectedValue(offline)
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        await expect(f.scheduler.receiveAvailableChanges()).rejects.toEqual(offline)
        f.pull.mockResolvedValue()
        await f.scheduler.retry(); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isWaiting()).toBe(false)
        expect(f.recovered).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it('keeps an outage delay when a receive started before the failure finishes later', async () => {
        const f = fixture()
        let finish!: () => void
        f.pull.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        f.scheduler.reportFailure({ code: 'server-unreachable', retryable: true })
        finish(); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isWaiting()).toBe(true)
        expect(f.recovered).not.toHaveBeenCalled()
        f.scheduler.localChange(true); await vi.advanceTimersByTimeAsync(4999)
        expect(f.pull).toHaveBeenCalledOnce()
        await vi.advanceTimersByTimeAsync(1)
        expect(f.scheduler.isWaiting()).toBe(false)
        expect(f.recovered).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it('ends an outage delay on time after the device clock is set back', async () => {
        const f = fixture()
        f.pull.mockRejectedValueOnce({ code: 'server-unreachable', retryable: true })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isWaiting()).toBe(true)
        // A clock that ran an hour fast is corrected.
        vi.setSystemTime(Date.now() - 3_600_000)
        await vi.advanceTimersByTimeAsync(5000)
        expect(f.pull).toHaveBeenCalledTimes(2)
        expect(f.scheduler.isWaiting()).toBe(false)
        f.scheduler.dispose()
    })
    it('correlates notification retries until the socket connects or a fresh session starts', async () => {
        const f = fixture()
        f.connect.mockRejectedValueOnce({ code: 'server-unreachable' })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        const first = f.connect.mock.calls[0][0]
        expect(first).toMatch(/^[0-9a-f-]{36}$/)
        await vi.advanceTimersByTimeAsync(5000)
        expect(f.connect.mock.calls.at(-1)![0]).toBe(first)
        // Starting the native worker succeeded, but the socket has not connected yet.
        f.scheduler.socket(false); await vi.advanceTimersByTimeAsync(2000)
        expect(f.connect.mock.calls.at(-1)![0]).toBe(first)
        f.scheduler.socket(true); f.scheduler.socket(false); await vi.advanceTimersByTimeAsync(1000)
        const afterSuccess = f.connect.mock.calls.at(-1)![0]
        expect(afterSuccess).not.toBe(first)
        await f.scheduler.foreground(false); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.connect.mock.calls.at(-1)![0]).not.toBe(afterSuccess)
        f.scheduler.dispose()
    })
    it('defers every automatic trigger while unavailable, retains explicit receive and resumes on release', async () => {
        const f = fixture()
        let available = false
        const publishHidden = vi.fn(async () => {})
        const scheduler = createServerSyncScheduler({ ...f, publishHidden, automaticSyncAvailable: () => available })
        await scheduler.foreground(true)
        scheduler.localChange(true); scheduler.remoteHint(); scheduler.socket(false)
        await scheduler.conversationOpened(); await scheduler.retry()
        await vi.advanceTimersByTimeAsync(65_000)
        expect(f.push).not.toHaveBeenCalled(); expect(f.pull).not.toHaveBeenCalled(); expect(f.connect).not.toHaveBeenCalled()
        await scheduler.receiveAvailableChanges()
        expect(f.pull).toHaveBeenCalledExactlyOnceWith(true)
        f.pull.mockClear()
        await scheduler.foreground(false, true)
        expect(publishHidden).not.toHaveBeenCalled()
        await scheduler.foreground(true)
        available = true
        await scheduler.resumeAutomaticSync()
        await vi.advanceTimersByTimeAsync(0)
        expect(f.push).toHaveBeenCalledOnce(); expect(f.pull).toHaveBeenCalledOnce(); expect(f.connect).toHaveBeenCalledOnce()
        scheduler.dispose()
    })

    it('does not start a queued clock repair after file work becomes unavailable', async () => {
        const f = fixture()
        let available = true
        let finishDisconnect!: () => void
        f.disconnect.mockImplementationOnce(() => new Promise<void>(resolve => { finishDisconnect = resolve }))
        const scheduler = createServerSyncScheduler({ ...f, automaticSyncAvailable: () => available })
        await scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        scheduler.reportFailure({ code: 'clock-skew' })
        const resumed = scheduler.resumeAutomaticSync()
        available = false
        finishDisconnect()
        await resumed; await scheduler.waitForIdle()
        expect(f.retryClock).not.toHaveBeenCalled()
        expect(scheduler.blockedCode()).toBe('clock-skew')
        available = true
        await scheduler.resumeAutomaticSync()
        expect(f.retryClock).toHaveBeenCalledOnce()
        scheduler.dispose()
    })

    it('debounces from the last durable edit and flushes generation completion immediately', async () => {
        const f = fixture(); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0); f.push.mockClear()
        f.scheduler.localChange(); await vi.advanceTimersByTimeAsync(1500); f.scheduler.localChange()
        await vi.advanceTimersByTimeAsync(1999); expect(f.push).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1); expect(f.push).toHaveBeenCalledTimes(1)
        f.scheduler.localChange(true); await vi.advanceTimersByTimeAsync(0); expect(f.push).toHaveBeenCalledTimes(2)
        f.scheduler.dispose()
    })
    it('publishes on hide before disconnecting, and a return to the foreground waits for that publish', async () => {
        const f = fixture()
        let finish!: () => void
        const publishHidden = vi.fn(() => new Promise<void>(resolve => { finish = resolve }))
        const scheduler = createServerSyncScheduler({ ...f, publishHidden })
        await scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0); f.push.mockClear()
        scheduler.localChange()
        const hidden = scheduler.foreground(false, true); await vi.advanceTimersByTimeAsync(0)
        expect(publishHidden).toHaveBeenCalledOnce(); expect(f.stopNotifications).not.toHaveBeenCalled()
        await scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.push).not.toHaveBeenCalled()
        finish(); await hidden; await vi.advanceTimersByTimeAsync(0)
        expect(f.stopNotifications).not.toHaveBeenCalled(); expect(f.push).toHaveBeenCalledOnce()
        await scheduler.foreground(false); expect(publishHidden).toHaveBeenCalledOnce(); expect(f.stopNotifications).toHaveBeenCalledOnce()
        expect(f.disconnect).not.toHaveBeenCalled()
        scheduler.dispose()
    })
    it('keeps a stalled receive independent from publication', async () => {
        const f = fixture(); let release!: () => void; f.pull.mockImplementation(() => new Promise<void>(resolve => { release = resolve }))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0); f.push.mockClear()
        f.scheduler.localChange(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.push).toHaveBeenCalledTimes(1); release(); f.scheduler.dispose()
    })
    it('uses foreground fallback and connected safety intervals and stops checks and notifications in background without cancelling', async () => {
        const f = fixture(); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0); f.pull.mockClear()
        await vi.advanceTimersByTimeAsync(5000); expect(f.pull).toHaveBeenCalledTimes(1)
        f.scheduler.socket(true); f.pull.mockClear(); await vi.advanceTimersByTimeAsync(59_999); expect(f.pull).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1); expect(f.pull).toHaveBeenCalledTimes(1)
        await f.scheduler.foreground(false); f.pull.mockClear(); f.push.mockClear(); f.connect.mockClear()
        await vi.advanceTimersByTimeAsync(120_000); f.scheduler.localChange(true); f.scheduler.remoteHint()
        expect(f.pull).not.toHaveBeenCalled(); expect(f.push).not.toHaveBeenCalled(); expect(f.connect).not.toHaveBeenCalled()
        expect(f.stopNotifications).toHaveBeenCalledOnce(); expect(f.disconnect).not.toHaveBeenCalled()
    })
    it('still cancels when a fence arrives while hiding stops only the notification channel', async () => {
        const f = fixture(); let stopped!: () => void
        f.stopNotifications.mockImplementationOnce(() => new Promise<void>(resolve => { stopped = resolve }))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        const hidden = f.scheduler.foreground(false); await vi.advanceTimersByTimeAsync(0)
        const fenced = f.scheduler.fence(); await vi.advanceTimersByTimeAsync(0)
        expect(f.disconnect).not.toHaveBeenCalled()
        stopped(); await Promise.all([hidden, fenced])
        expect(f.disconnect).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it.each([
        { pendingStart: false, stopFails: false },
        { pendingStart: true, stopFails: false },
        { pendingStart: false, stopFails: true },
        { pendingStart: true, stopFails: true },
    ])('reconnects after returning while the hidden notification stop is pending (old start pending: $pendingStart, stop fails: $stopFails)', async ({ pendingStart, stopFails }) => {
        const f = fixture()
        const stopFailure = new Error('notification stop failed')
        let stopped!: () => void
        let started: (() => void) | undefined
        let notificationsRunning = false
        f.connect.mockImplementation(async () => { notificationsRunning = true })
        if (pendingStart) f.connect.mockImplementationOnce(() => new Promise<void>(resolve => {
            started = () => { notificationsRunning = true; resolve() }
        }))
        f.stopNotifications.mockImplementation(async () => { notificationsRunning = false })
        f.stopNotifications.mockImplementationOnce(() => new Promise<void>((resolve, reject) => {
            stopped = () => { notificationsRunning = false; if (stopFails) reject(stopFailure); else resolve() }
        }))
        await f.scheduler.foreground(true)
        await vi.advanceTimersByTimeAsync(0)
        const hidden = f.scheduler.foreground(false).catch(error => error)
        await vi.advanceTimersByTimeAsync(0)
        const returned = f.scheduler.foreground(true)
        await vi.advanceTimersByTimeAsync(0)
        stopped()
        started?.()
        expect(await hidden).toBe(stopFails ? stopFailure : undefined)
        await returned
        await vi.advanceTimersByTimeAsync(0)
        expect(notificationsRunning).toBe(true)
        f.scheduler.dispose()
    })
    it('immediately pulls on notification, conversation open, foreground return and socket loss', async () => {
        const f = fixture(); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0); f.pull.mockClear()
        f.scheduler.remoteHint(); await vi.advanceTimersByTimeAsync(0); f.scheduler.conversationOpened(); await vi.advanceTimersByTimeAsync(0); f.scheduler.socket(false); await vi.advanceTimersByTimeAsync(0)
        expect(f.pull).toHaveBeenCalledTimes(3)
        await f.scheduler.foreground(false); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0); expect(f.pull).toHaveBeenCalledTimes(4)
        f.scheduler.dispose()
    })
    it('retries transient publication with bounded backoff and stops permanent failures', async () => {
        const f = fixture()
        f.push.mockRejectedValueOnce({ code: 'server-unreachable', retryable: true })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.push).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(4999); expect(f.push).toHaveBeenCalledTimes(1)
        await vi.advanceTimersByTimeAsync(1); expect(f.push).toHaveBeenCalledTimes(2)
        f.push.mockRejectedValueOnce({ code: 'equal-stamp-integrity', retryable: false })
        f.scheduler.localChange(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(true)
        f.push.mockClear(); f.pull.mockClear(); f.connect.mockClear()
        await vi.advanceTimersByTimeAsync(120_000)
        expect(f.push).not.toHaveBeenCalled(); expect(f.pull).not.toHaveBeenCalled(); expect(f.connect).not.toHaveBeenCalled()
        f.scheduler.dispose()
    })
    it('serializes explicit receive-to-head in background and fences every active lane', async () => {
        const f = fixture(); const release: Array<() => void> = []
        f.pull.mockImplementation(() => new Promise<void>(resolve => release.push(resolve)))
        const available = f.scheduler.receiveAvailableChanges()
        await vi.advanceTimersByTimeAsync(0)
        expect(f.pull).toHaveBeenCalledWith(true)
        let fenced = false; const fence = f.scheduler.fence().then(() => { fenced = true })
        await vi.advanceTimersByTimeAsync(0); expect(fenced).toBe(false)
        release[0](); await available; await fence; expect(fenced).toBe(true)
        await vi.advanceTimersByTimeAsync(120_000); expect(f.pull).toHaveBeenCalledTimes(1)
        f.scheduler.dispose()
    })
    it.each(['background', 'file reservation', 'foreground'] as const)('reserves a complete receive for plugin startup with queued hints in %s', async stoppedBy => {
        const f = fixture()
        let available = true
        const pulls: Array<{ complete: boolean; resolve(): void }> = []
        const scheduler = createServerSyncScheduler({ ...f,
            automaticSyncAvailable: () => available,
            pull: complete => new Promise<void>(resolve => { pulls.push({ complete: !!complete, resolve }) }),
        })
        let starts = 0
        const gate = createPluginStartupGate({ enabled: () => true, delay: () => true,
            context: async () => ({ identity: 'synthetic-binding', bound: true,
                receive: () => scheduler.receiveAvailableChanges(), assertCurrent: async () => {},
            }),
            start: async () => { if (await gate.ensure()) starts++ },
        })
        try {
            await scheduler.foreground(true)
            scheduler.remoteHint()
            expect(await gate.ensure()).toBe(false)
            let secondComplete = false
            const second = scheduler.receiveAvailableChanges().then(() => { secondComplete = true })
            pulls[0].resolve()
            await vi.advanceTimersByTimeAsync(0)
            expect(pulls.map(pull => pull.complete)).toEqual([false, true])
            scheduler.remoteHint(); scheduler.remoteHint()
            expect(starts).toBe(0)
            expect(secondComplete).toBe(false)
            expect(pulls).toHaveLength(2)
            if (stoppedBy === 'background') await scheduler.foreground(false)
            else if (stoppedBy === 'file reservation') available = false
            pulls[1].resolve()
            await second
            await vi.advanceTimersByTimeAsync(0)
            expect(starts).toBe(1)
            expect(secondComplete).toBe(true)
            expect(pulls.map(pull => pull.complete)).toEqual(stoppedBy === 'foreground' ? [false, true, false] : [false, true])
            pulls[2]?.resolve()
            await scheduler.waitForIdle()
        } finally { scheduler.dispose(); gate.invalidate() }
    })
    it.each(['server-unreachable', 'cancelled', 'equal-stamp-integrity'])('keeps an explicit %s receive failure retryable without releasing plugin startup', async code => {
        const f = fixture()
        const failure = { code, retryable: code !== 'equal-stamp-integrity' }
        f.pull.mockRejectedValueOnce(failure)
        let starts = 0
        const gate = createPluginStartupGate({ enabled: () => true, delay: () => true,
            context: async () => ({ identity: 'synthetic-binding', bound: true,
                receive: () => f.scheduler.receiveAvailableChanges(), assertCurrent: async () => {},
            }),
            start: async () => { if (await gate.ensure()) starts++ },
        })
        try {
            expect(await gate.ensure()).toBe(false)
            await vi.advanceTimersByTimeAsync(0)
            expect(starts).toBe(0)
            expect(gate.isWaiting()).toBe(true)
            expect(f.scheduler.isRunning()).toBe(false)
            if (code === 'equal-stamp-integrity') {
                await expect(f.scheduler.receiveAvailableChanges()).rejects.toThrow('Sync is stopped')
                f.scheduler.reset()
            }
            expect(f.failed).toHaveBeenCalledTimes(code === 'cancelled' ? 0 : 1)
            gate.retry()
            await vi.advanceTimersByTimeAsync(0)
            expect(starts).toBe(1)
        } finally { f.scheduler.dispose(); gate.invalidate() }
    })
    it('stops native integrity errors restored as Error messages', async () => {
        const f=fixture(); f.pull.mockRejectedValueOnce(new Error('equal-stamp-integrity'))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(true); f.pull.mockClear(); f.push.mockClear()
        expect(f.scheduler.blockedCode()).toBe('equal-stamp-integrity')
        await vi.advanceTimersByTimeAsync(120_000); expect(f.pull).not.toHaveBeenCalled(); expect(f.push).not.toHaveBeenCalled()
        f.scheduler.dispose()
    })
    it('names the failure that stopped sync until a retry or a new binding clears it', async () => {
        const f = fixture()
        expect(f.scheduler.blockedCode()).toBe('')
        f.push.mockRejectedValueOnce({ code: 'writer-collision', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.blockedCode()).toBe('writer-collision')
        // A later failure does not replace the one that stopped sync.
        f.scheduler.reportFailure({ code: 'registration-used', retryable: false })
        expect(f.scheduler.blockedCode()).toBe('writer-collision')
        f.scheduler.reset()
        expect(f.scheduler.blockedCode()).toBe('')
        f.push.mockRejectedValueOnce({ code: 'writer-collision', retryable: false })
        f.scheduler.localChange(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.blockedCode()).toBe('writer-collision')
        await f.scheduler.retry()
        expect(f.scheduler.blockedCode()).toBe('')
        f.scheduler.dispose()
    })
    it('rechecks only a clock block on foreground and resumes publication after native admission succeeds', async () => {
        const f = fixture(); f.push.mockRejectedValueOnce({ code: 'clock-skew', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(true)
        await vi.advanceTimersByTimeAsync(120_000); expect(f.retryClock).not.toHaveBeenCalled()
        await f.scheduler.foreground(false); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.retryClock).toHaveBeenCalledOnce(); expect(f.scheduler.isBlocked()).toBe(false)
        expect(f.push).toHaveBeenCalledTimes(2)
        f.scheduler.dispose()
    })
    it('keeps an uncorrected clock blocked and coalesces an open-event native recheck', async () => {
        const f = fixture(); f.push.mockRejectedValueOnce(new Error('clock-skew'))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        f.retryClock.mockRejectedValueOnce({ code: 'clock-skew', retryable: false })
        await f.scheduler.conversationOpened(); expect(f.scheduler.isBlocked()).toBe(true)
        let finish!: () => void
        f.retryClock.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        const first = f.scheduler.conversationOpened(); const second = f.scheduler.conversationOpened()
        await vi.advanceTimersByTimeAsync(0); expect(f.retryClock).toHaveBeenCalledTimes(2)
        expect(f.scheduler.isBlocked()).toBe(true)
        finish(); await Promise.all([first, second]); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(false); expect(f.push).toHaveBeenCalledTimes(2)
        f.scheduler.dispose()
    })
    it('reports a clock recheck that began after an outage delay as a retry', async () => {
        const f = fixture(); f.push.mockRejectedValueOnce(new Error('clock-skew'))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        const outage = { code: 'server-unreachable', retryable: true }
        f.retryClock.mockRejectedValue(outage)
        await f.scheduler.conversationOpened(); expect(f.failed).toHaveBeenLastCalledWith(outage, false)
        await f.scheduler.conversationOpened(); expect(f.failed).toHaveBeenLastCalledWith(outage, true)
        f.scheduler.dispose()
    })
    it('waits for the blocked connection cancellation before admitting clock repair', async () => {
        const f = fixture(); let stopped!: () => void
        f.disconnect.mockImplementationOnce(() => new Promise<void>(resolve => { stopped = resolve }))
        f.push.mockRejectedValueOnce({ code: 'clock-skew', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        const opened = f.scheduler.conversationOpened(); await vi.advanceTimersByTimeAsync(0)
        expect(f.retryClock).not.toHaveBeenCalled()
        stopped(); await opened
        expect(f.retryClock).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it('rechecks an incoming clock block on foreground and blocks again while the stamp is still ahead', async () => {
        const f = fixture(); f.pull.mockRejectedValueOnce({ code: 'incoming-clock-skew', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(true)
        await vi.advanceTimersByTimeAsync(120_000); expect(f.retryClock).not.toHaveBeenCalled()
        f.pull.mockClear(); f.pull.mockRejectedValueOnce({ code: 'incoming-clock-skew', retryable: false })
        await f.scheduler.foreground(false); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.retryClock).toHaveBeenCalledOnce(); expect(f.pull).toHaveBeenCalledOnce(); expect(f.scheduler.isBlocked()).toBe(true)
        f.pull.mockClear()
        await f.scheduler.conversationOpened(); await vi.advanceTimersByTimeAsync(0)
        expect(f.retryClock).toHaveBeenCalledTimes(2); expect(f.pull).toHaveBeenCalled(); expect(f.scheduler.isBlocked()).toBe(false)
        f.scheduler.dispose()
    })
    it('rechecks a library format block on each trigger, follows the code it reports, and resumes once the level matches', async () => {
        const f = fixture(); f.pull.mockRejectedValueOnce({ code: 'library-format-switch-required', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.blockedCode()).toBe('library-format-switch-required')
        await vi.advanceTimersByTimeAsync(120_000); expect(f.checkFormat).not.toHaveBeenCalled()
        f.checkFormat.mockRejectedValueOnce({ code: 'sync-server-update-required', retryable: false })
        await f.scheduler.conversationOpened(); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.blockedCode()).toBe('sync-server-update-required'); expect(f.retryClock).not.toHaveBeenCalled()
        // A check that cannot reach the server keeps the stop it already reports.
        f.failed.mockClear(); f.checkFormat.mockRejectedValueOnce({ code: 'server-unreachable', retryable: true })
        await f.scheduler.conversationOpened(); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.blockedCode()).toBe('sync-server-update-required'); expect(f.failed).not.toHaveBeenCalled()
        // Edits check again once they settle.
        f.checkFormat.mockRejectedValueOnce({ code: 'sync-server-update-required', retryable: false })
        f.scheduler.localChange(); await vi.advanceTimersByTimeAsync(1_000); f.scheduler.localChange(); await vi.advanceTimersByTimeAsync(2_000)
        expect(f.checkFormat).toHaveBeenCalledTimes(3); expect(f.scheduler.isBlocked()).toBe(true)
        f.push.mockClear(); f.pull.mockClear()
        await f.scheduler.foreground(false); await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.checkFormat).toHaveBeenCalledTimes(4); expect(f.scheduler.isBlocked()).toBe(false)
        expect(f.pull).toHaveBeenCalled(); expect(f.push).toHaveBeenCalled()
        f.scheduler.dispose()
    })
    it.each(['accepted-clock-correction-required', 'writer-collision', 'equal-stamp-integrity'])('never automatically clears %s', async code => {
        const f = fixture(); f.push.mockRejectedValueOnce({ code, retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        await f.scheduler.foreground(false); await f.scheduler.foreground(true); await f.scheduler.conversationOpened()
        await vi.advanceTimersByTimeAsync(120_000)
        expect(f.retryClock).not.toHaveBeenCalled(); expect(f.scheduler.isBlocked()).toBe(true)
        expect(f.push).toHaveBeenCalledTimes(1)
        f.scheduler.dispose()
    })
    it('does not report the fence complete until an in-flight notification start has settled and been stopped', async () => {
        const f = fixture(); const order: string[] = []; let started!: () => void
        f.connect.mockImplementationOnce(() => new Promise<void>(resolve => { started = () => { order.push('started'); resolve() } }))
        f.disconnect.mockImplementation(async () => { order.push('stopped') })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.connect).toHaveBeenCalledOnce()
        let fenced = false; const fence = f.scheduler.fence().then(() => { fenced = true })
        await vi.advanceTimersByTimeAsync(0); expect(fenced).toBe(false)
        started(); await fence
        expect(order.indexOf('started')).toBeLessThan(order.lastIndexOf('stopped'))
        f.connect.mockClear(); await vi.advanceTimersByTimeAsync(120_000); expect(f.connect).not.toHaveBeenCalled()
        f.scheduler.dispose()
    })
    it('never reports a cancelled lane or connection start', async () => {
        const f = fixture()
        f.push.mockRejectedValueOnce({ code: 'cancelled', retryable: true })
        f.pull.mockRejectedValueOnce({ code: 'cancelled', retryable: true })
        f.connect.mockRejectedValueOnce({ code: 'cancelled', retryable: true })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.failed).not.toHaveBeenCalled(); expect(f.scheduler.isBlocked()).toBe(false)
        f.scheduler.dispose()
    })
    it('reports recovery only after every failed lane succeeds again', async () => {
        const f = fixture()
        f.push.mockRejectedValueOnce({ code: 'server-unreachable', retryable: true })
        f.pull.mockRejectedValueOnce({ code: 'server-unreachable', retryable: true })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.failed).toHaveBeenCalledTimes(2); expect(f.recovered).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(1000); expect(f.push).toHaveBeenCalledTimes(1); expect(f.recovered).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(4000); expect(f.pull).toHaveBeenCalledTimes(2); expect(f.recovered).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it('never reports recovery while an integrity block holds', async () => {
        const f = fixture(); let finishPull!: () => void
        f.pull.mockImplementationOnce(() => new Promise<void>(resolve => { finishPull = resolve }))
        f.push.mockRejectedValueOnce({ code: 'writer-collision', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(true)
        finishPull(); await vi.advanceTimersByTimeAsync(0)
        expect(f.recovered).not.toHaveBeenCalled()
        f.scheduler.dispose()
    })
    it('resets a block for a newly installed binding', async () => {
        const f = fixture(); f.push.mockRejectedValueOnce({ code: 'writer-collision', retryable: false })
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(true)
        await f.scheduler.fence(); f.scheduler.reset(); f.push.mockClear()
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        expect(f.scheduler.isBlocked()).toBe(false); expect(f.push).toHaveBeenCalledOnce()
        f.scheduler.dispose()
    })
    it('retains an incoming integrity block when an already running push later fails clock admission', async () => {
        const f = fixture(); let rejectPush!: (error: unknown) => void; let rejectPull!: (error: unknown) => void
        f.push.mockImplementationOnce(() => new Promise<void>((_resolve, reject) => { rejectPush = reject }))
        f.pull.mockImplementationOnce(() => new Promise<void>((_resolve, reject) => { rejectPull = reject }))
        await f.scheduler.foreground(true); await vi.advanceTimersByTimeAsync(0)
        rejectPull({ code: 'equal-stamp-integrity', retryable: false }); await vi.advanceTimersByTimeAsync(0)
        rejectPush({ code: 'clock-skew', retryable: false }); await vi.advanceTimersByTimeAsync(0)
        await f.scheduler.conversationOpened()
        expect(f.retryClock).not.toHaveBeenCalled(); expect(f.scheduler.isBlocked()).toBe(true)
        expect(f.failed).toHaveBeenLastCalledWith({ code: 'equal-stamp-integrity', retryable: false }, false)
        f.scheduler.dispose()
    })
})
