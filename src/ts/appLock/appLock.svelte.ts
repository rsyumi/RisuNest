import { getDeviceMarkers } from '../storage/deviceMarkers'
import { AppLockController, appLockIsGated, initialAppLockState, type AppLockStorage } from './controller'
import { createAppLockNativeAdapter, normalizeAppLockBackground } from './native'
import { createStartupSurface, type StartupSurface } from './startupSurface'
import { APP_LOCK_KEY } from './record'

function markerStorage(): AppLockStorage {
    return {
        read: async (refresh = false) => {
            const markers = getDeviceMarkers()
            if (refresh) await markers.reload?.()
            return markers.getItem(APP_LOCK_KEY)
        },
        commit: async (serialized) => {
            const markers = getDeviceMarkers()
            if (!markers.commitItem) throw new Error('App lock requires durable device markers')
            await markers.commitItem(APP_LOCK_KEY, serialized)
        },
    }
}

class AppLockFacade {
    state = $state.raw(initialAppLockState())
    private controller: AppLockController | null = null
    private starting: Promise<void> | null = null
    private backgroundColor = '#21252b'
    private startupSurface: StartupSurface | null = null
    private failureStarting: Promise<void> | null = null
    get gated(): boolean { return appLockIsGated(this.state) }

    initialize(options: { read?: AppLockStorage['read']; commit?: AppLockStorage['commit']; recovery?: boolean } = {}): Promise<void> {
        const storage = markerStorage()
        if (options.read) storage.read = options.read
        if (options.commit) storage.commit = options.commit
        if (this.controller) {
            const controller = this.controller
            this.starting = (this.starting ?? Promise.resolve()).then(async () => {
                if (options.recovery) controller.attachStorage(storage)
                else await controller.continueNormally(storage)
            })
            return this.starting
        }
        this.startupSurface?.stop()
        this.startupSurface = null
        this.controller = new AppLockController(storage, createAppLockNativeAdapter())
        const controller = this.controller
        controller.subscribe((state) => { this.state = state })
        if (typeof window !== 'undefined') {
            const shown = () => { if (document.visibilityState === 'visible') controller.retryPendingPresentation() }
            window.addEventListener('focus', shown)
            window.addEventListener('pageshow', shown)
            document.addEventListener('visibilitychange', shown)
        }
        this.starting = (this.failureStarting ?? Promise.resolve()).then(async () => {
            controller.setBackgroundColor(this.backgroundColor)
            await controller.initialize(options.recovery)
        })
        return this.starting
    }

    digit(digit: string) { return this.controller?.digit(digit) }
    deleteDigit() { this.controller?.deleteDigit() }
    beginSetup() { this.controller?.beginSetup() }
    beginChange() { this.controller?.beginChange() }
    cancelFlow() { this.controller?.cancelFlow() }
    retrySave() { return this.controller?.retrySave() }
    retryRead() { return this.controller?.retryRead() }
    refreshCapabilities() { return this.controller?.refreshCapabilities() }
    unlockWithDevice(reason: string) { return this.controller?.unlockWithDevice(reason) }
    resetPin(reason: string) { return this.controller?.resetPin(reason) }
    setEnabled(enabled: false) { return this.controller?.setEnabled(enabled) ?? Promise.resolve(false) }
    setTiming(timing: Parameters<AppLockController['setTiming']>[0]) { return this.controller?.setTiming(timing) ?? Promise.resolve(false) }
    setNativeUnlock(enabled: boolean, reason: string) { return this.controller?.setNativeUnlock(enabled, reason) ?? Promise.resolve('saveFailed' as const) }
    setHideRecentsPreview(enabled: boolean) { return this.controller?.setHideRecentsPreview(enabled) ?? Promise.resolve(false) }
    async renderLoading(): Promise<void> {
        if (this.controller) return this.controller.renderLoading()
        return this.renderBeforeInitialization()
    }
    async renderBootFailure(): Promise<void> {
        if (this.controller) return this.controller.renderBootFailure()
        return this.renderBeforeInitialization()
    }
    private renderBeforeInitialization(): Promise<void> {
        if (this.failureStarting) return this.failureStarting
        this.startupSurface ??= createStartupSurface()
        this.failureStarting = this.startupSurface.present(this.backgroundColor).finally(() => { this.failureStarting = null })
        return this.failureStarting
    }
    rendered(reason: string) { return this.controller?.rendered(reason) }
    setBackgroundColor(color: string) {
        const normalized = normalizeAppLockBackground(color)
        if (normalized) {
            this.backgroundColor = normalized
            this.controller?.setBackgroundColor(normalized)
        }
    }
}

export const appLock = new AppLockFacade()
export const initializeAppLock = (options?: Parameters<AppLockFacade['initialize']>[0]) => appLock.initialize(options)
export type { AppLockState, PinFlow, AppLockIssue } from './controller'
export type { AppLockTiming } from './record'
