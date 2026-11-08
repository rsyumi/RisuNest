import { webcrypto } from 'node:crypto'
import { beforeAll, describe, expect, it, vi } from 'vitest'
import { AppLockController, type AppLockStorage } from './controller'
import { normalizeAppLockBackground } from './native'
import type { AppLockCapabilities, AppLockLifecycle, AppLockNativeAdapter, OsAuthOutcome } from './native'
import { APP_LOCK_KEY, createPinRecord, defaultAppLockRecord, lockRequiredAfterDeparture, matchesPin, readAppLockRecord, type AppLockRecord, type AppLockTiming, type PinRecord } from './record'
import { createNativeDeviceMarkers, DEVICE_MARKER_KEYS } from '../storage/deviceMarkers'
import type { NativeDeviceSettings } from '../storage/nativeDeviceSettings'

let pin: PinRecord
beforeAll(async () => {
    vi.stubGlobal('crypto', webcrypto)
    pin = await createPinRecord('123456')
})

function fixture(options: { record?: unknown; timing?: AppLockTiming; nativeUnlock?: boolean; available?: boolean; gateRequired?: boolean } = {}) {
    let stored: unknown = options.record === undefined ? JSON.stringify({
        ...defaultAppLockRecord(), enabled: true, pin,
        timing: options.timing ?? 'launch', nativeUnlock: options.nativeUnlock ?? false,
    }) : options.record
    let lifecycle: AppLockLifecycle = { active: true, epoch: 0, atMs: 0, departedAtMs: null }
    const capabilities: AppLockCapabilities = {
        auth: options.available ? { status: 'available', method: 'device' } : { status: 'unsupported' }, previewSupported: false,
    }
    let listener: (snapshot: AppLockLifecycle) => void
    let nativeGateRequired = options.gateRequired ?? false
    let presentedLocked: boolean | null = null
    const adapter: AppLockNativeAdapter = {
        check: vi.fn(async () => capabilities),
        authenticate: vi.fn(async (): Promise<OsAuthOutcome> => 'success'),
        setPolicy: vi.fn(async (policy) => { nativeGateRequired = policy.gateRequired; return lifecycle }),
        ready: vi.fn(async (epoch, locked) => {
            if (epoch !== lifecycle.epoch || (nativeGateRequired && !locked)) return false
            nativeGateRequired = locked
            presentedLocked = locked
            return true
        }),
        listen: vi.fn(async (callback) => { listener = callback; return () => {} }),
        report: vi.fn(),
    }
    const storage: AppLockStorage = {
        read: vi.fn(async () => stored),
        commit: vi.fn(async (serialized) => { stored = serialized }),
    }
    const controller = new AppLockController(storage, adapter)
    /** Changes the native lifecycle without its event reaching the page. */
    const miss = (active: boolean, atMs: number, departedAtMs: number | null = lifecycle.departedAtMs) => {
        lifecycle = { active, atMs, epoch: lifecycle.epoch + 1, departedAtMs: active ? departedAtMs : atMs }
    }
    return {
        controller, storage, adapter, capabilities, miss,
        stored: () => stored,
        presented: () => presentedLocked,
        refresh() { listener(lifecycle) },
        emit(active: boolean, atMs: number, departedAtMs: number | null = lifecycle.departedAtMs) {
            miss(active, atMs, departedAtMs)
            listener(lifecycle)
        },
    }
}
async function enter(controller: AppLockController, value = '123456') {
    for (const digit of value) await controller.digit(digit)
}

function deferred<T>() {
    let resolve: (value: T) => void
    const promise = new Promise<T>((done) => { resolve = done })
    return { promise, resolve: resolve! }
}

describe('app lock record and timing', () => {
    it('hands native covers opaque hex colors for the current theme', () => {
        expect(normalizeAppLockBackground('#AbC')).toBe('#aabbcc')
        expect(normalizeAppLockBackground('#21252B')).toBe('#21252b')
        expect(normalizeAppLockBackground('rgb(33, 37, 43)')).toBe('#21252b')
        expect(normalizeAppLockBackground('rgba(33 37 43 / 0.5)')).toBe('#21252b')
        expect(normalizeAppLockBackground('rgb(100% 0% 0%)')).toBe('#ff0000')
    })

    it('round-trips a salted six-digit PIN while preserving extra record and PIN fields', async () => {
        const encoded = JSON.stringify({ ...defaultAppLockRecord(), enabled: true, pin: { ...pin, futurePinField: 7 }, futureField: { retain: true } })
        const record = readAppLockRecord(encoded)
        expect(record.futureField).toEqual({ retain: true })
        expect(record.pin.futurePinField).toBe(7)
        expect(record.pin.iterations).toBe(100_000)
        expect(atob(record.pin.salt)).toHaveLength(16)
        expect(record.pin.hash).not.toContain('123456')
        expect(await matchesPin('123456', record.pin)).toBe(true)
        expect(await matchesPin('654321', record.pin)).toBe(false)
        const another = await createPinRecord('123456')
        expect(another.salt).not.toBe(pin.salt)
        expect(another.hash).not.toBe(pin.hash)
        await expect(createPinRecord('12345')).rejects.toThrow('six digits')
    })

    it('defaults only an absent record and rejects incomplete or damaged present records', () => {
        expect(readAppLockRecord(null)).toEqual(defaultAppLockRecord())
        for (const value of ['', '{}', 'null', '{}x', undefined, {}, JSON.stringify({ ...defaultAppLockRecord(), enabled: true }),
            JSON.stringify({ ...defaultAppLockRecord(), pin: { ...pin, hash: 'broken' } })]) {
            expect(() => readAppLockRecord(value)).toThrow()
        }
    })

    it('uses inclusive deadlines and treats a backward clock as expired', () => {
        expect(lockRequiredAfterDeparture('after-1m', 100, 60_099)).toBe(false)
        expect(lockRequiredAfterDeparture('after-1m', 100, 60_100)).toBe(true)
        expect(lockRequiredAfterDeparture('after-30m', 100, 1_800_100)).toBe(true)
        expect(lockRequiredAfterDeparture('after-5m', 100, 99)).toBe(true)
        expect(lockRequiredAfterDeparture('launch', 100, 1_800_100)).toBe(false)
    })
})

describe('app lock controller', () => {
    it.each([[false, false], [true, false], [false, true], [true, true]])('keeps an unlocked return interactive (enabled=%s, preview=%s)', async (enabled, hideRecentsPreview) => {
        const f = fixture(enabled ? {} : { record: null })
        f.capabilities.previewSupported = hideRecentsPreview
        await f.controller.initialize()
        if (enabled) await enter(f.controller)
        if (hideRecentsPreview) await f.controller.setHideRecentsPreview(true)
        f.emit(false, 100)
        f.emit(true, 100_000)
        expect(f.controller.gated).toBe(false)
        expect(f.controller.state.returnPending).toBe(false)
        expect(f.controller.state.locked).toBe(false)
        await f.controller.rendered('unlock')
        expect(f.presented()).toBe(false)
    })

    it.each(['listen', 'setPolicy'] as const)('does not gate a disabled record after a native %s failure', async (method) => {
        const f = fixture({ record: null })
        vi.mocked(f.adapter[method]).mockRejectedValueOnce(new Error('synthetic bridge failure'))
        await f.controller.initialize()
        expect(f.controller.state.readFailed).toBe(false)
        expect(f.controller.gated).toBe(false)
    })

    it('repairs a stale native gate after the disabled policy initially fails to arrive', async () => {
        const f = fixture({ record: null, gateRequired: true })
        vi.mocked(f.adapter.setPolicy).mockRejectedValueOnce(new Error('policy unavailable'))
        await f.controller.initialize()
        expect(f.controller.gated).toBe(false)
        await f.controller.rendered('unlock')
        expect(f.presented()).toBe(false)
    })

    it('reconnects a failed listener before enabling and then locks on departure', async () => {
        const f = fixture({ record: null })
        vi.mocked(f.adapter.listen).mockRejectedValueOnce(new Error('transient listener failure'))
        await f.controller.initialize()
        f.controller.beginSetup()
        await enter(f.controller, '123456123456')
        expect(f.controller.state.record.enabled).toBe(true)
        expect(f.controller.gated).toBe(false)
        await f.controller.setTiming('immediate')
        f.emit(false, 100)
        expect(f.controller.state.locked).toBe(true)
    })

    it('retains the disabled record and offers setup retry while the listener remains unavailable', async () => {
        const f = fixture({ record: null })
        vi.mocked(f.adapter.listen).mockRejectedValue(new Error('listener unavailable'))
        await f.controller.initialize()
        f.controller.beginSetup()
        await enter(f.controller, '123456123456')
        expect(f.controller.state.record.enabled).toBe(false)
        expect(f.stored()).toBeNull()
        expect(f.controller.state.saveFailed).toBe(true)
        f.controller.cancelFlow()
        expect(f.controller.gated).toBe(false)
    })

    it('releases disabled startup loading without leaving the native input gate required', async () => {
        const f = fixture({ record: null })
        await f.controller.initialize()
        await f.controller.renderLoading()
        expect(f.presented()).toBe(false)
        await f.controller.rendered('unlock')
        expect(f.presented()).toBe(false)
        expect(f.controller.gated).toBe(false)
    })

    it('presents the loading surface without unlocking or starting native authentication', async () => {
        const f = fixture({ nativeUnlock: true, available: true })
        await f.controller.initialize()
        await f.controller.renderLoading()
        expect(f.presented()).toBe(true)
        expect(f.controller.state.locked).toBe(true)
        expect(f.adapter.authenticate).not.toHaveBeenCalled()
        await f.controller.rendered('unlock')
        expect(f.controller.state.locked).toBe(false)
    })

    it('starts every enabled mode locked, accepts the correct PIN and retains wrong-PIN entry until feedback ends', async () => {
        const f = fixture()
        await f.controller.initialize()
        expect(f.controller.state.locked).toBe(true)
        const wrong = enter(f.controller, '999999')
        await vi.waitFor(() => expect(f.controller.state.issue).toBe('wrongPin'))
        expect(f.controller.state.digits).toBe('999999')
        expect(f.controller.state.locked).toBe(true)
        await wrong
        expect(f.controller.state.digits).toBe('')
        await enter(f.controller)
        expect(f.controller.gated).toBe(false)
        f.emit(false, 100)
        f.emit(true, 100_000)
        await f.controller.rendered('unlock')
        expect(f.controller.state.locked).toBe(false)
        const restarted = fixture({ record: f.stored() })
        await restarted.controller.initialize()
        expect(restarted.controller.state.locked).toBe(true)
    })

    it('requires an immediate lock on departure and discards a delayed native success after leaving', async () => {
        const f = fixture({ timing: 'immediate', nativeUnlock: true, available: true })
        await f.controller.initialize()
        await enter(f.controller)
        f.emit(false, 100)
        expect(f.controller.state.locked).toBe(true)
        f.emit(true, 110)
        const outcome = deferred<OsAuthOutcome>()
        vi.mocked(f.adapter.authenticate).mockReturnValueOnce(outcome.promise)
        const request = f.controller.unlockWithDevice('unlock')
        await vi.waitFor(() => expect(f.controller.state.nativeBusy).toBe(true))
        await vi.waitFor(() => expect(f.adapter.authenticate).toHaveBeenCalled())
        f.emit(false, 120)
        f.emit(true, 130)
        outcome.resolve('success')
        await request
        expect(f.controller.state.locked).toBe(true)
        expect(f.controller.state.nativeBusy).toBe(false)
        const beforeFailure = deferred<OsAuthOutcome>()
        vi.mocked(f.adapter.authenticate).mockReturnValueOnce(beforeFailure.promise)
        const failureRequest = f.controller.unlockWithDevice('unlock')
        await vi.waitFor(() => expect(f.controller.state.nativeBusy).toBe(true))
        await f.controller.renderBootFailure()
        beforeFailure.resolve('success')
        await failureRequest
        expect(f.controller.state.locked).toBe(true)
        await f.controller.continueNormally(f.storage)
        await enter(f.controller)
        expect(f.controller.state.locked).toBe(false)
    })

    it('counts one continuous absence and keeps a return covered until its current frame is ready', async () => {
        const f = fixture({ timing: 'after-1m' })
        await f.controller.initialize()
        await enter(f.controller)
        f.emit(false, 100)
        f.emit(true, 60_099)
        expect(f.controller.state.locked).toBe(false)
        expect(f.controller.gated).toBe(true)
        await f.controller.rendered('unlock')
        expect(f.controller.gated).toBe(false)
        f.emit(false, 60_100)
        f.emit(true, 120_099)
        await f.controller.rendered('unlock')
        expect(f.controller.state.locked).toBe(false)
        f.emit(false, 120_100)
        f.emit(true, 180_100)
        expect(f.controller.state.locked).toBe(true)
        const before = fixture({ timing: 'after-5m' })
        await before.controller.initialize()
        await enter(before.controller)
        before.emit(false, 100)
        before.emit(true, 50)
        expect(before.controller.state.locked).toBe(true)
    })

    it('keeps content gated when the native policy or lifecycle subscription fails and permits retry', async () => {
        const policy = fixture()
        await policy.controller.initialize()
        await enter(policy.controller)
        vi.mocked(policy.adapter.setPolicy).mockRejectedValueOnce(new Error('native policy unavailable'))
        policy.controller.setBackgroundColor('#ffffff')
        await vi.waitFor(() => expect(policy.controller.state.failure).toBe('bridge'))
        expect(policy.controller.state.readFailed).toBe(true)
        expect(policy.controller.state.failure).toBe('bridge')
        expect(policy.controller.gated).toBe(true)
        await policy.controller.rendered('unlock')
        expect(policy.controller.gated).toBe(true)
        await policy.controller.retryRead()
        await policy.controller.rendered('unlock')
        expect(policy.presented()).toBe(false)
        expect(policy.controller.gated).toBe(false)
        const events = fixture()
        vi.mocked(events.adapter.listen).mockRejectedValueOnce(new Error('native events unavailable'))
        await events.controller.initialize()
        expect(events.controller.gated).toBe(true)
        await events.controller.retryRead()
        expect(events.controller.state.locked).toBe(true)
        await enter(events.controller)
        expect(events.controller.gated).toBe(false)
        const unlocked = fixture({ available: true })
        await unlocked.controller.initialize()
        await enter(unlocked.controller)
        vi.mocked(unlocked.adapter.setPolicy).mockRejectedValueOnce(new Error('native policy unavailable'))
        unlocked.controller.setBackgroundColor('#ffffff')
        await vi.waitFor(() => expect(unlocked.controller.state.failure).toBe('bridge'))
        await unlocked.controller.resetPin('reset')
        expect(unlocked.controller.state.flow).toBeNull()
        await unlocked.controller.retryRead()
        await unlocked.controller.rendered('unlock')
        expect(unlocked.presented()).toBe(false)
        expect(unlocked.controller.state.locked).toBe(false)
        expect(unlocked.controller.gated).toBe(false)
    })

    it('refreshes presentation after an owned native prompt without making a new departure or auth request', async () => {
        const f = fixture({ nativeUnlock: true, available: true })
        await f.controller.initialize()
        const outcome = deferred<OsAuthOutcome>()
        vi.mocked(f.adapter.authenticate).mockReturnValueOnce(outcome.promise)
        const unlock = f.controller.unlockWithDevice('unlock')
        await vi.waitFor(() => expect(f.adapter.authenticate).toHaveBeenCalled())
        const epoch = f.controller.state.epoch
        const revision = f.controller.state.presentationRevision
        f.refresh()
        expect(f.controller.state.epoch).toBe(epoch)
        expect(f.controller.state.presentationRevision).toBe(revision + 1)
        outcome.resolve('success')
        await unlock
        expect(f.controller.state.locked).toBe(false)
        f.refresh()
        await f.controller.rendered('unlock')
        expect(f.controller.gated).toBe(false)
    })

    it('requests another rendered frame after a current transient readiness failure', async () => {
        const f = fixture()
        await f.controller.initialize()
        f.emit(false, 1)
        f.emit(true, 2)
        vi.mocked(f.adapter.ready).mockResolvedValueOnce(false)
        const revision = f.controller.state.presentationRevision
        await f.controller.rendered('unlock')
        expect(f.controller.state.returnPending).toBe(true)
        await vi.waitFor(() => expect(f.controller.state.presentationRevision).toBe(revision + 1))
        await f.controller.rendered('unlock')
        expect(f.controller.state.returnPending).toBe(false)
        vi.mocked(f.adapter.ready).mockResolvedValueOnce(false)
        const failureRevision = f.controller.state.presentationRevision
        await f.controller.renderBootFailure()
        await vi.waitFor(() => expect(f.controller.state.presentationRevision).toBe(failureRevision + 1))
        await f.controller.renderBootFailure()
        expect(f.presented()).toBe(false)
        expect(f.controller.state.locked).toBe(true)
        await f.controller.continueNormally(f.storage)
        await f.controller.rendered('unlock')
        vi.mocked(f.adapter.setPolicy).mockRejectedValueOnce(new Error('native policy unavailable'))
        await f.controller.renderBootFailure()
        expect(await vi.mocked(f.adapter.ready).mock.results.at(-1)?.value).toBe(true)
        expect(f.presented()).toBe(true)
        expect(f.controller.state.locked).toBe(true)
        f.controller.dispose()
    })

    it('asks for a refused return frame for a bounded time and again after a later page signal', async () => {
        vi.useFakeTimers()
        try {
            const f = fixture()
            await f.controller.initialize()
            const ready = vi.mocked(f.adapter.ready).getMockImplementation()!
            f.emit(false, 1)
            f.emit(true, 2)
            vi.mocked(f.adapter.ready).mockResolvedValue(false)
            const revision = f.controller.state.presentationRevision
            for (let retry = 1; retry <= 6; retry++) {
                await f.controller.rendered('unlock')
                await vi.advanceTimersByTimeAsync(4000)
                expect(f.controller.state.presentationRevision).toBe(revision + retry)
            }
            await f.controller.rendered('unlock')
            await vi.advanceTimersByTimeAsync(60_000)
            expect(f.controller.state.presentationRevision).toBe(revision + 6)
            expect(f.controller.state.returnPending).toBe(true)
            f.miss(false, 3)
            f.miss(true, 4)
            vi.mocked(f.adapter.ready).mockImplementation(ready)
            f.controller.retryPendingPresentation()
            await vi.waitFor(() => expect(f.controller.state.presentationRevision).toBe(revision + 7))
            expect(f.controller.state.epoch).toBe(4)
            expect(f.controller.state.returnPending).toBe(true)
            await f.controller.rendered('unlock')
            expect(f.controller.state.returnPending).toBe(false)
            expect(f.presented()).toBe(true)
            const policies = vi.mocked(f.adapter.setPolicy).mock.calls.length
            f.controller.retryPendingPresentation()
            await vi.advanceTimersByTimeAsync(1000)
            expect(f.controller.state.presentationRevision).toBe(revision + 7)
            expect(f.adapter.setPolicy).toHaveBeenCalledTimes(policies)
            expect(f.adapter.report).not.toHaveBeenCalled()
            f.controller.dispose()
        } finally { vi.useRealTimers() }
    })

    it('logs a return frame, policy update or frame confirmation that stays pending once per epoch', async () => {
        vi.useFakeTimers()
        try {
            const f = fixture()
            await f.controller.initialize()
            f.emit(false, 1)
            await vi.advanceTimersByTimeAsync(0)
            const policy = deferred<AppLockLifecycle>()
            vi.mocked(f.adapter.setPolicy).mockReturnValueOnce(policy.promise)
            f.emit(true, 2)
            const rendering = f.controller.rendered('unlock')
            const again = f.controller.rendered('unlock')
            await vi.advanceTimersByTimeAsync(3000)
            expect(f.adapter.report).toHaveBeenCalledOnce()
            expect(f.adapter.report).toHaveBeenLastCalledWith('app-lock policy update still pending after 3000 ms: epoch 2')
            const answer = deferred<boolean>()
            vi.mocked(f.adapter.ready).mockReturnValueOnce(answer.promise).mockReturnValueOnce(new Promise<boolean>(() => {}))
            policy.resolve({ active: true, epoch: 2, atMs: 2, departedAtMs: 1 })
            await vi.advanceTimersByTimeAsync(3000)
            expect(f.adapter.report).toHaveBeenCalledTimes(2)
            expect(f.adapter.report).toHaveBeenLastCalledWith('app-lock frame confirmation still pending after 3000 ms: epoch 2')
            const revision = f.controller.state.presentationRevision
            const policies = vi.mocked(f.adapter.setPolicy).mock.calls.length
            f.controller.retryPendingPresentation()
            await vi.advanceTimersByTimeAsync(1000)
            expect(f.controller.state.presentationRevision).toBe(revision)
            expect(f.adapter.setPolicy).toHaveBeenCalledTimes(policies)
            answer.resolve(true)
            await rendering
            expect(f.controller.state.returnPending).toBe(false)
            expect(f.adapter.report).toHaveBeenCalledTimes(2)
            void again
            f.emit(false, 3)
            f.emit(true, 4)
            await vi.advanceTimersByTimeAsync(3000)
            expect(f.adapter.report).toHaveBeenCalledTimes(3)
            expect(f.adapter.report).toHaveBeenLastCalledWith('app-lock frame presentation still pending after 3000 ms: epoch 4')
            f.emit(false, 5)
            f.emit(true, 6)
            await f.controller.rendered('unlock')
            await vi.advanceTimersByTimeAsync(3000)
            expect(f.adapter.report).toHaveBeenCalledTimes(3)
            f.controller.dispose()
        } finally { vi.useRealTimers() }
    })

    it('keeps a pending return separate from the native PIN gate so a pre-deadline return can present', async () => {
        const f = fixture({ timing: 'after-1m' })
        let requiredByNative = false
        const setPolicy = f.adapter.setPolicy
        vi.mocked(f.adapter.setPolicy).mockImplementation(async (policy) => {
            requiredByNative = policy.gateRequired
            return { active: f.controller.state.active, epoch: f.controller.state.epoch, atMs: 0, departedAtMs: null }
        })
        // The native shell rejects an unlocked frame when its policy still requires the gate.
        vi.mocked(f.adapter.ready).mockImplementation(async (_epoch, locked) => !requiredByNative || locked)
        await f.controller.initialize()
        await enter(f.controller)
        f.emit(false, 100)
        f.emit(true, 60_099)
        expect(f.controller.state.returnPending).toBe(true)
        expect(f.controller.gated).toBe(true)
        await f.controller.rendered('unlock')
        expect(f.controller.state.returnPending).toBe(false)
        expect(f.controller.gated).toBe(false)
        expect(setPolicy).toHaveBeenLastCalledWith(expect.objectContaining({ enabled: true, gateRequired: false }))
    })

    it('does not acknowledge a frame from an old lifecycle epoch', async () => {
        const f = fixture()
        await f.controller.initialize()
        const outcome = deferred<boolean>()
        vi.mocked(f.adapter.ready).mockReturnValueOnce(outcome.promise)
        f.emit(false, 1)
        f.emit(true, 2)
        const rendering = f.controller.rendered('unlock')
        await vi.waitFor(() => expect(f.adapter.ready).toHaveBeenCalled())
        f.emit(false, 3)
        f.emit(true, 4)
        outcome.resolve(true)
        await rendering
        expect(f.controller.state.returnPending).toBe(true)
    })

    it('automatically attempts native unlock once per episode and permits an explicit retry', async () => {
        const f = fixture({ nativeUnlock: true, available: true })
        vi.mocked(f.adapter.authenticate).mockResolvedValueOnce('cancelled')
        await f.controller.initialize()
        await f.controller.rendered('unlock')
        expect(f.controller.state.issue).toBeNull()
        expect(f.controller.state.locked).toBe(true)
        await f.controller.rendered('unlock')
        f.emit(false, 1)
        f.emit(true, 2)
        await f.controller.rendered('unlock')
        expect(f.controller.state.locked).toBe(true)
        await f.controller.unlockWithDevice('unlock')
        expect(f.controller.state.locked).toBe(false)
    })

    it('preserves PIN access and preferences when native availability changes or an OS request fails', async () => {
        const f = fixture({ nativeUnlock: true, available: true })
        await f.controller.initialize()
        f.capabilities.auth = { status: 'notEnrolled' }
        await f.controller.unlockWithDevice('unlock')
        expect(f.controller.state.issue).toBe('nativeUnavailable')
        expect(f.controller.state.record.nativeUnlock).toBe(true)
        await enter(f.controller)
        expect(f.controller.state.locked).toBe(false)
    })

    it('keeps failed record reads covered and allows an OS reset without enabling native unlock', async () => {
        const f = fixture({ record: 'malformed', available: true })
        await f.controller.initialize()
        expect(f.controller.state.readFailed).toBe(true)
        expect(f.controller.gated).toBe(true)
        await f.controller.resetPin('reset')
        expect(f.controller.state.flow).toBe('reset')
        f.controller.cancelFlow()
        expect(f.controller.state.readFailed).toBe(true)
        await f.controller.resetPin('reset')
        await enter(f.controller, '654321')
        await enter(f.controller, '654321')
        expect(f.controller.gated).toBe(false)
        expect(f.controller.state.record.nativeUnlock).toBe(false)
        expect(await matchesPin('654321', readAppLockRecord(f.stored()).pin)).toBe(true)
        const recovery = fixture({ record: 'malformed' })
        await recovery.controller.initialize(true)
        expect(recovery.controller.gated).toBe(false)
    })

    it('rechecks a failed recovery read before normal content but preserves a valid recovery unlock', async () => {
        const f = fixture({ record: 'malformed' })
        await f.controller.initialize(true)
        expect(f.controller.gated).toBe(false)
        await f.controller.continueNormally({ read: () => 'still malformed', commit: async () => {} })
        expect(f.controller.state.readFailed).toBe(true)
        expect(f.controller.gated).toBe(true)
        const valid = fixture()
        await valid.controller.initialize(true)
        await enter(valid.controller)
        await valid.controller.continueNormally(valid.storage)
        expect(valid.controller.gated).toBe(false)
    })

    it('requires successful confirmation before enabling native unlock and leaves cancelled requests unchanged', async () => {
        const f = fixture({ available: true })
        await f.controller.initialize()
        await enter(f.controller)
        vi.mocked(f.adapter.authenticate).mockResolvedValueOnce('cancelled')
        expect(await f.controller.setNativeUnlock(true, 'enable')).toBe('cancelled')
        expect(f.controller.state.record.nativeUnlock).toBe(false)
        vi.mocked(f.storage.commit).mockRejectedValueOnce(new Error('write failed'))
        expect(await f.controller.setNativeUnlock(true, 'enable')).toBe('saveFailed')
        expect(f.controller.state.record.nativeUnlock).toBe(false)
        expect(await f.controller.setNativeUnlock(true, 'enable')).toBe('saved')
        expect(readAppLockRecord(f.stored()).nativeUnlock).toBe(true)
    })

    it('restarts setup after a mismatch and preserves the existing PIN when a change is cancelled', async () => {
        const f = fixture({ record: null })
        await f.controller.initialize()
        expect(f.controller.gated).toBe(false)
        f.controller.beginSetup()
        await enter(f.controller, '654321')
        await enter(f.controller, '123456')
        expect(f.controller.state).toMatchObject({ flow: 'setup', phase: 'entry', digits: '', issue: 'mismatch' })
        expect(f.controller.state.record.enabled).toBe(false)
        await enter(f.controller)
        await enter(f.controller)
        expect(f.controller.gated).toBe(false)
        f.controller.beginChange()
        await enter(f.controller, '654321')
        f.controller.cancelFlow()
        expect(f.controller.gated).toBe(false)
        expect(await matchesPin('123456', f.controller.state.record.pin)).toBe(true)
    })

    it('retains the previous effective config and cached marker until a failed setup is durably retried', async () => {
        const stored = new Map<string, unknown>()
        let fail = true
        const backend: NativeDeviceSettings = {
            get: async (key) => stored.get(key) ?? null,
            readMany: async (keys) => keys.map((key) => stored.get(key) ?? null),
            set: async (key, value) => { if (fail) throw new Error('storage failure'); stored.set(key, value) },
            patch: async () => {},
        }
        const markers = createNativeDeviceMarkers(backend, DEVICE_MARKER_KEYS.map(() => null))
        const f = fixture({ record: null })
        f.controller.attachStorage({ read: () => markers.getItem(APP_LOCK_KEY), commit: (value) => markers.commitItem!(APP_LOCK_KEY, value) })
        await f.controller.initialize()
        f.controller.beginSetup()
        await enter(f.controller)
        await enter(f.controller)
        expect(f.controller.state.saveFailed).toBe(true)
        expect(f.controller.state.record.enabled).toBe(false)
        expect(markers.getItem(APP_LOCK_KEY)).toBeNull()
        fail = false
        await f.controller.retrySave()
        expect(f.controller.gated).toBe(false)
        expect(f.controller.state.record.enabled).toBe(true)
        const reopened = createNativeDeviceMarkers(backend, await backend.readMany(DEVICE_MARKER_KEYS))
        const record = readAppLockRecord(reopened.getItem(APP_LOCK_KEY))
        expect(record.enabled).toBe(true)
        expect(await matchesPin('123456', record.pin)).toBe(true)
    })

    it('does not reveal content when a PIN write completes after a genuine departure', async () => {
        const f = fixture({ record: null })
        const write = deferred<void>()
        vi.mocked(f.storage.commit).mockImplementationOnce(async () => write.promise)
        await f.controller.initialize()
        f.controller.beginSetup()
        await enter(f.controller)
        const saving = enter(f.controller)
        await vi.waitFor(() => expect(f.storage.commit).toHaveBeenCalled())
        f.emit(false, 1)
        f.emit(true, 2)
        f.controller.cancelFlow()
        expect(f.controller.gated).toBe(true)
        write.resolve()
        await saving
        expect(f.controller.state).toMatchObject({ locked: true, flow: 'unlock', busy: false })
        await enter(f.controller)
        expect(f.controller.state.locked).toBe(false)
    })

    it('serializes changes without dropping earlier successful fields or unknown extras', async () => {
        const initial: AppLockRecord = { ...defaultAppLockRecord(), enabled: true, pin: { ...pin, extra: true }, future: { retained: true } }
        const f = fixture({ record: JSON.stringify(initial) })
        await f.controller.initialize()
        await enter(f.controller)
        await Promise.all([f.controller.setTiming('after-15m'), f.controller.setHideRecentsPreview(true)])
        expect(readAppLockRecord(f.stored())).toMatchObject({ timing: 'after-15m', hideRecentsPreview: true, future: { retained: true } })
        f.controller.beginChange()
        await enter(f.controller, '654321')
        await enter(f.controller, '654321')
        expect(readAppLockRecord(f.stored()).pin.extra).toBe(true)
    })
})
