import { createPinRecord, defaultAppLockRecord, lockRequiredAfterDeparture, matchesPin, readAppLockRecord, type AppLockRecord, type AppLockTiming } from './record'
import type { AppLockLifecycle, AppLockNativeAdapter, OsAuthAvailability, OsAuthOutcome } from './native'

// A refused frame asks for another one, quickly at first and then for a few more seconds.
// After that, only a later lifecycle event or page signal asks again.
const READINESS_RETRY_DELAYS_MS = [32, 128, 512, 1000, 2000, 4000]
// Policy updates and frame confirmations answer within a few frames; a longer wait goes to the device log.
const NATIVE_STALL_REPORT_MS = 3000

export type PinFlow = 'unlock' | 'setup' | 'change' | 'reset'
export type AppLockIssue = 'wrongPin' | 'mismatch' | 'nativeUnavailable'
export interface AppLockState {
    record: AppLockRecord
    initialized: boolean
    readFailed: boolean
    failure: 'record' | 'bridge' | null
    locked: boolean
    returnPending: boolean
    active: boolean
    epoch: number
    presentationRevision: number
    availability: OsAuthAvailability
    previewSupported: boolean
    flow: PinFlow | null
    phase: 'entry' | 'confirm'
    digits: string
    busy: boolean
    nativeBusy: boolean
    issue: AppLockIssue | null
    saveFailed: boolean
}
export interface AppLockStorage {
    read(refresh?: boolean): Promise<unknown> | unknown
    commit(serialized: string): Promise<void>
}
export type NativeSettingResult = 'saved' | 'cancelled' | 'notEnrolled' | 'unavailable' | 'failed' | 'saveFailed'

export function initialAppLockState(): AppLockState {
    return {
        record: defaultAppLockRecord(), initialized: false, readFailed: false, failure: null,
        locked: false, returnPending: false, active: true, epoch: 0, presentationRevision: 0,
        availability: { status: 'unsupported' }, previewSupported: false,
        flow: null, phase: 'entry', digits: '', busy: false, nativeBusy: false,
        issue: null, saveFailed: false,
    }
}

export function appLockIsGated(state: AppLockState): boolean {
    return state.locked || state.readFailed || state.returnPending || state.flow !== null
}

export class AppLockController {
    state = initialAppLockState()
    private storage: AppLockStorage
    private listeners = new Set<(state: AppLockState) => void>()
    private episode = 0
    private operation = 0
    private nativeAttemptEpisode = -1
    private firstPin = ''
    private pendingPin = ''
    private savingPin = false
    private departedAtMs: number | null = null
    private departureTiming: AppLockTiming = 'launch'
    private writeTail = Promise.resolve()
    private policyTail = Promise.resolve()
    private policyRevision = 0
    private readinessRetryCount = 0
    private readinessRetry: ReturnType<typeof setTimeout> | null = null
    private confirmedEpoch = -1
    private presentingEpoch = -1
    private confirmations = 0
    private reportedStall = ''
    private backgroundColor = '#21252b'
    private unlisten: (() => void) | null = null
    private listening = false
    private bridgeFailed = false
    private recovery = false
    private recordRead = false
    private disposed = false
    private bootFailure = false
    private resetReadFailed = false
    private checkVersion = 0

    constructor(storage: AppLockStorage, private readonly native: AppLockNativeAdapter) {
        this.storage = storage
    }

    get gated(): boolean { return appLockIsGated(this.state) }

    subscribe(listener: (state: AppLockState) => void): () => void {
        this.listeners.add(listener)
        listener(this.state)
        return () => this.listeners.delete(listener)
    }

    attachStorage(storage: AppLockStorage): void { this.storage = storage }

    async continueNormally(storage: AppLockStorage): Promise<void> {
        this.storage = storage
        this.recovery = false
        this.resumeContent()
        await this.ensureListening()
        if (!this.recordRead || this.bridgeFailed) {
            await this.readRecord()
            await this.publishPolicy()
        }
    }

    private update(next: Partial<AppLockState>): void {
        if (this.disposed) return
        this.state = { ...this.state, ...next }
        for (const listener of this.listeners) listener(this.state)
    }

    async initialize(recovery = false): Promise<void> {
        if (this.state.initialized) return
        this.recovery = recovery
        await this.ensureListening()
        await this.readRecord()
        await this.refreshCapabilities()
        await this.publishPolicy()
    }

    private async ensureListening(): Promise<void> {
        if (this.listening) return
        try {
            this.unlisten = await this.native.listen((event) => this.lifecycle(event, true))
            this.listening = true
        } catch { this.bridgeFailed = true }
    }

    private protectBridge(): void {
        this.operation++
        this.bridgeFailed = true
        if (!this.bootFailure && !(this.recovery && !this.recordRead)
            && (!this.recordRead || this.state.record.enabled)) {
            this.update({ readFailed: true, failure: this.state.failure === 'record' ? 'record' : 'bridge', busy: false })
        }
    }

    private async readRecord(refresh = false): Promise<void> {
        try {
            const record = readAppLockRecord(await this.storage.read(refresh))
            this.recordRead = true
            this.operation++
            if (record.enabled) this.episode++
            this.update({
                record, initialized: true, readFailed: false, failure: null, locked: record.enabled,
                flow: record.enabled ? 'unlock' : null, digits: '', phase: 'entry',
                busy: false, issue: null, saveFailed: false,
            })
            if (!this.listening) this.protectBridge()
        } catch {
            this.recordRead = false
            this.operation++
            this.update({
                initialized: true, readFailed: !this.recovery, failure: this.recovery ? null : 'record', locked: false, flow: null,
                busy: false, digits: '', issue: null,
            })
        }
    }

    async retryRead(): Promise<void> {
        if (this.state.busy) return
        this.update({ busy: true })
        await this.ensureListening()
        if (this.state.failure === 'bridge') {
            if (!this.listening) {
                this.protectBridge()
                return
            }
            await this.publishPolicy()
            if (!this.bridgeFailed) {
                this.update({ readFailed: false, failure: null, busy: false })
                await this.publishPolicy()
            }
            return
        }
        await this.readRecord(true)
        await this.refreshCapabilities()
        await this.publishPolicy()
    }

    async refreshCapabilities(): Promise<void> {
        const version = ++this.checkVersion
        try {
            const capabilities = await this.native.check()
            if (version === this.checkVersion) this.update({
                availability: capabilities.auth, previewSupported: capabilities.previewSupported,
            })
        } catch {
            // A failed capability check never changes the stored preference.
            if (version === this.checkVersion) this.update({ availability: { status: 'unsupported' } })
        }
    }

    private requireLock(): void {
        if (this.state.locked || this.state.readFailed) return
        this.episode++
        this.operation++
        this.firstPin = ''
        this.pendingPin = ''
        this.update({ locked: true, flow: 'unlock', phase: 'entry', digits: '', busy: this.savingPin, issue: null, saveFailed: false })
        void this.refreshCapabilities()
    }

    lifecycle(snapshot: AppLockLifecycle, refreshPresentation = false): void {
        if (this.disposed || snapshot.epoch < this.state.epoch) return
        const changed = snapshot.epoch !== this.state.epoch || snapshot.active !== this.state.active
        if (!changed) {
            if (refreshPresentation && snapshot.active) {
                this.readinessRetryCount = 0
                this.update({ presentationRevision: this.state.presentationRevision + 1 })
            }
            return
        }
        this.operation++
        this.readinessRetryCount = 0
        if (this.readinessRetry) clearTimeout(this.readinessRetry)
        this.readinessRetry = null
        if (!snapshot.active) {
            this.departedAtMs = snapshot.departedAtMs ?? snapshot.atMs
            this.departureTiming = this.state.record.timing
            this.update({ active: false, epoch: snapshot.epoch, busy: this.savingPin, digits: '' })
            if (this.state.record.enabled && this.departureTiming === 'immediate') this.requireLock()
        } else {
            const departed = snapshot.departedAtMs ?? this.departedAtMs
            const returnPending = this.gated
                || (this.state.record.enabled && this.departureTiming !== 'launch')
            this.update({ active: true, epoch: snapshot.epoch, returnPending, busy: this.savingPin })
            if (this.state.record.enabled && departed !== null
                && lockRequiredAfterDeparture(this.departureTiming, departed, snapshot.atMs)) this.requireLock()
            this.departedAtMs = null
            this.watchPresentation(snapshot.epoch)
        }
        void this.publishPolicy()
    }

    setBackgroundColor(color: string): void {
        if (!color || color === this.backgroundColor) return
        this.backgroundColor = color
        void this.publishPolicy()
    }

    private publishPolicy(): Promise<void> {
        const revision = ++this.policyRevision
        const policy = {
            enabled: this.state.record.enabled, timing: this.state.record.timing,
            gateRequired: !this.bootFailure && (this.state.locked || this.state.readFailed || this.state.flow !== null), hideRecentsPreview: this.state.record.hideRecentsPreview,
            backgroundColor: this.backgroundColor,
        }
        const next = this.policyTail.then(async () => {
            if (revision !== this.policyRevision || this.disposed) return
            const snapshot = await this.native.setPolicy(policy)
            if (revision === this.policyRevision) this.bridgeFailed = !this.listening
            this.lifecycle(snapshot)
        })
        this.policyTail = next.catch(() => {
            if (revision === this.policyRevision) this.protectBridge()
        })
        return this.policyTail
    }

    async renderLoading(): Promise<void> {
        if (!this.state.initialized || this.bootFailure) return
        this.presentingEpoch = this.state.epoch
        if (this.bridgeFailed && this.recordRead && !this.state.record.enabled) await this.publishPolicy()
        const epoch = this.state.epoch
        const operation = this.operation
        await this.watch(epoch, 'policy update', this.policyTail)
        if (!this.state.active || epoch !== this.state.epoch || operation !== this.operation) return
        let accepted = false
        try { accepted = await this.confirm(epoch, this.state.locked || this.state.readFailed || this.state.flow !== null) } catch { return }
        if (epoch !== this.state.epoch || operation !== this.operation) return
        if (!accepted) this.retryPresentation(epoch, operation)
        else this.confirmed(epoch)
    }

    async renderBootFailure(): Promise<void> {
        if (!this.bootFailure) this.operation++
        this.bootFailure = true
        this.presentingEpoch = this.state.epoch
        await this.watch(this.state.epoch, 'policy update', this.publishPolicy())
        const epoch = this.state.epoch
        const operation = this.operation
        if (!this.bootFailure || !this.state.active) return
        let accepted = false
        try { accepted = await this.confirm(epoch, this.bridgeFailed) } catch { return }
        if (epoch !== this.state.epoch || operation !== this.operation) return
        if (!accepted) this.retryPresentation(epoch, operation)
        else this.confirmed(epoch)
    }

    resumeContent(): void {
        if (!this.bootFailure) return
        this.bootFailure = false
        this.operation++
        void this.publishPolicy()
    }

    async rendered(reason: string): Promise<void> {
        this.presentingEpoch = this.state.epoch
        if (this.bridgeFailed && this.recordRead && !this.state.record.enabled) await this.publishPolicy()
        const epoch = this.state.epoch
        const operation = this.operation
        await this.watch(epoch, 'policy update', this.policyTail)
        if (this.bootFailure || !this.state.active || epoch !== this.state.epoch || operation !== this.operation) return
        const locked = this.state.locked || this.state.readFailed || this.state.flow !== null
        let accepted = false
        try { accepted = await this.confirm(epoch, locked) } catch { return }
        if (epoch !== this.state.epoch || operation !== this.operation) return
        if (!accepted) {
            this.retryPresentation(epoch, operation)
            return
        }
        this.confirmed(epoch)
        if (this.state.returnPending) {
            this.update({ returnPending: false })
            void this.publishPolicy()
        }
        if (this.state.locked && this.state.flow === 'unlock' && this.state.record.nativeUnlock
            && this.nativeAttemptEpisode !== this.episode && !this.state.nativeBusy) {
            this.nativeAttemptEpisode = this.episode
            await this.unlockWithDevice(reason)
        }
    }

    private reportStall(epoch: number, step: string): void {
        const report = `${epoch}:${step}`
        if (this.disposed || this.reportedStall === report) return
        this.reportedStall = report
        this.native.report(`app-lock ${step} still pending after ${NATIVE_STALL_REPORT_MS} ms: epoch ${epoch}`)
    }

    /** Reports a return whose frame was never presented for confirmation. */
    private watchPresentation(epoch: number): void {
        setTimeout(() => {
            if (this.state.active && this.state.epoch === epoch && this.presentingEpoch !== epoch) this.reportStall(epoch, 'frame presentation')
        }, NATIVE_STALL_REPORT_MS)
    }

    private async watch<T>(epoch: number, step: string, wait: Promise<T>): Promise<T> {
        const stall = setTimeout(() => this.reportStall(epoch, step), NATIVE_STALL_REPORT_MS)
        try { return await wait } finally { clearTimeout(stall) }
    }

    private async confirm(epoch: number, locked: boolean): Promise<boolean> {
        this.confirmations++
        try { return await this.watch(epoch, 'frame confirmation', this.native.ready(epoch, locked)) }
        finally { this.confirmations-- }
    }

    private confirmed(epoch: number): void {
        this.confirmedEpoch = epoch
        this.readinessRetryCount = 0
        if (this.readinessRetry) clearTimeout(this.readinessRetry)
        this.readinessRetry = null
    }

    /** Asks again for a frame the native shell has not confirmed, after a later sign that the page is shown. */
    retryPendingPresentation(): void {
        if (this.disposed || !this.state.initialized || this.state.nativeBusy || this.confirmations > 0
            || (this.state.active && this.confirmedEpoch === this.state.epoch)) return
        this.readinessRetryCount = 0
        // The policy answer carries the current lifecycle, which also catches up a missed event.
        void this.publishPolicy().then(() => {
            if (this.disposed || !this.state.active || this.state.nativeBusy || this.confirmations > 0
                || this.confirmedEpoch === this.state.epoch) return
            this.update({ presentationRevision: this.state.presentationRevision + 1 })
        })
    }

    private retryPresentation(epoch: number, operation: number): void {
        if (!this.state.active || this.state.nativeBusy || this.readinessRetry
            || this.readinessRetryCount >= READINESS_RETRY_DELAYS_MS.length) return
        const delay = READINESS_RETRY_DELAYS_MS[this.readinessRetryCount++]
        this.readinessRetry = setTimeout(() => {
            this.readinessRetry = null
            if (!this.disposed && this.state.active && epoch === this.state.epoch && operation === this.operation) {
                this.update({ presentationRevision: this.state.presentationRevision + 1 })
            }
        }, delay)
    }

    private unlock(): void {
        this.operation++
        this.resetReadFailed = false
        this.firstPin = ''
        this.pendingPin = ''
        this.update({ locked: false, readFailed: false, failure: null, flow: null, digits: '', busy: false, issue: null, saveFailed: false })
        void this.publishPolicy()
    }

    beginSetup(): void {
        if (!this.state.initialized || this.gated || this.state.record.enabled) return
        this.beginFlow('setup')
    }

    beginChange(): void {
        if (!this.state.initialized || this.gated || !this.state.record.enabled) return
        this.beginFlow('change')
    }

    private beginFlow(flow: Exclude<PinFlow, 'unlock'>): void {
        this.operation++
        this.firstPin = ''
        this.pendingPin = ''
        this.update({ flow, digits: '', phase: 'entry', busy: false, issue: null, saveFailed: false })
        void this.publishPolicy()
    }

    cancelFlow(): void {
        if (this.state.busy || this.savingPin || this.state.flow === 'unlock' || this.state.flow === null) return
        this.operation++
        this.firstPin = ''
        this.pendingPin = ''
        this.update({ flow: this.state.locked && !this.resetReadFailed ? 'unlock' : null, readFailed: this.resetReadFailed, failure: this.resetReadFailed ? 'record' : null,
            digits: '', phase: 'entry', issue: null, saveFailed: false })
        this.resetReadFailed = false
        void this.publishPolicy()
    }

    async digit(digit: string): Promise<void> {
        if (!/^\d$/.test(digit) || this.state.busy || this.state.saveFailed || !this.state.active
            || !this.state.flow || this.state.digits.length >= 6) return
        const digits = this.state.digits + digit
        this.update({ digits, issue: null })
        if (digits.length !== 6) return
        if (this.state.flow !== 'unlock') {
            if (this.state.phase === 'entry') {
                this.firstPin = digits
                this.update({ digits: '', phase: 'confirm' })
            } else if (digits !== this.firstPin) {
                this.firstPin = ''
                this.update({ digits: '', phase: 'entry', issue: 'mismatch' })
            } else {
                this.pendingPin = digits
                await this.savePin()
            }
            return
        }
        const operation = this.operation
        this.update({ busy: true })
        let matched = false
        try { matched = !!this.state.record.pin && await matchesPin(digits, this.state.record.pin) } catch { /* Keep the gate on verifier failure. */ }
        if (operation !== this.operation || !this.state.active) return
        if (matched) this.unlock()
        else {
            this.update({ issue: 'wrongPin' })
            await new Promise<void>((resolve) => setTimeout(resolve, 360))
            if (operation === this.operation) this.update({ digits: '', busy: false })
        }
    }

    deleteDigit(): void {
        if (this.state.busy || this.state.saveFailed || !this.state.flow) return
        this.update({ digits: this.state.digits.slice(0, -1), issue: null })
    }

    async retrySave(): Promise<void> {
        if (this.state.saveFailed && !this.state.busy && this.pendingPin) await this.savePin()
    }

    private async savePin(): Promise<void> {
        const operation = this.operation
        this.savingPin = true
        this.update({ busy: true, saveFailed: false })
        try {
            const pin = await createPinRecord(this.pendingPin)
            if (operation !== this.operation || !this.state.active) {
                this.update({ busy: false, saveFailed: true })
                return
            }
            const succeeded = await this.changeRecord((record) => ({ ...record, enabled: true, pin: { ...record.pin, ...pin } }))
            if (operation !== this.operation) {
                if (succeeded) {
                    this.requireLock()
                    this.update({ busy: false, flow: 'unlock', digits: '', saveFailed: false })
                } else this.update({ busy: false, saveFailed: true })
                return
            }
            if (succeeded) this.unlock()
            else this.update({ busy: false, saveFailed: true })
        } catch {
            if (operation === this.operation) this.update({ busy: false, saveFailed: true })
        } finally { this.savingPin = false }
    }

    private changeRecord(change: (record: AppLockRecord) => AppLockRecord): Promise<boolean> {
        const next = this.writeTail.then(async () => {
            const record = change(this.state.record)
            if (record.enabled && !this.listening) {
                await this.ensureListening()
                if (!this.listening) return false
            }
            try { await this.storage.commit(JSON.stringify(record)) } catch { return false }
            this.recordRead = true
            this.update({ record })
            await this.publishPolicy()
            return true
        })
        this.writeTail = next.then(() => undefined, () => undefined)
        return next
    }

    async setEnabled(enabled: false): Promise<boolean> {
        const saved = await this.changeRecord((record) => ({ ...record, enabled }))
        if (saved) this.unlock()
        return saved
    }

    setTiming(timing: AppLockTiming): Promise<boolean> {
        return this.changeRecord((record) => ({ ...record, timing }))
    }

    setHideRecentsPreview(hideRecentsPreview: boolean): Promise<boolean> {
        return this.changeRecord((record) => ({ ...record, hideRecentsPreview }))
    }

    private async authenticate(reason: string): Promise<OsAuthOutcome | 'stale'> {
        if (this.state.nativeBusy) return 'stale'
        const episode = this.episode
        const epoch = this.state.epoch
        const operation = this.operation
        this.update({ nativeBusy: true })
        try {
            await this.refreshCapabilities()
            if (epoch !== this.state.epoch || operation !== this.operation || !this.state.active) return 'stale'
            if (this.state.availability.status !== 'available') return 'unavailable'
            const outcome = await this.native.authenticate(reason)
            return this.state.active && epoch === this.state.epoch && episode === this.episode && operation === this.operation ? outcome : 'stale'
        } catch { return epoch === this.state.epoch && operation === this.operation && this.state.active ? 'failed' : 'stale' }
        finally { this.update({ nativeBusy: false }) }
    }

    async unlockWithDevice(reason: string): Promise<void> {
        if (!this.state.locked || this.state.flow !== 'unlock' || !this.state.record.nativeUnlock) return
        this.nativeAttemptEpisode = this.episode
        const outcome = await this.authenticate(reason)
        if (outcome === 'success') this.unlock()
        else if (outcome === 'failed' || outcome === 'unavailable') this.update({ issue: 'nativeUnavailable' })
    }

    async resetPin(reason: string): Promise<void> {
        if (this.state.failure === 'bridge' || (!this.state.locked && !this.state.readFailed) || this.state.busy) return
        const outcome = await this.authenticate(reason)
        if (outcome === 'success') {
            this.resetReadFailed = this.state.readFailed
            this.update({ readFailed: false, failure: null, locked: true })
            this.beginFlow('reset')
        }
    }

    async setNativeUnlock(enabled: boolean, reason: string): Promise<NativeSettingResult> {
        if (!enabled) return await this.changeRecord((record) => ({ ...record, nativeUnlock: false })) ? 'saved' : 'saveFailed'
        const result = await this.authenticate(reason)
        if (result === 'stale' || result === 'cancelled') return 'cancelled'
        if (result === 'unavailable') return this.state.availability.status === 'notEnrolled' ? 'notEnrolled' : 'unavailable'
        if (result !== 'success') return 'failed'
        return await this.changeRecord((record) => ({ ...record, nativeUnlock: true })) ? 'saved' : 'saveFailed'
    }

    dispose(): void {
        this.disposed = true
        this.operation++
        this.unlisten?.()
        if (this.readinessRetry) clearTimeout(this.readinessRetry)
        this.listeners.clear()
    }
}
