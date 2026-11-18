import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { recordNativeLogError } from '../nativeLog'
import type { AppLockTiming } from './record'

export type OsAuthAvailability =
    | { status: 'unsupported' }
    | { status: 'notEnrolled' }
    | { status: 'available'; method: 'windowsHello' | 'faceId' | 'touchId' | 'device' }
export type OsAuthOutcome = 'success' | 'cancelled' | 'unavailable' | 'failed'
export interface AppLockCapabilities { auth: OsAuthAvailability; previewSupported: boolean }
export interface AppLockLifecycle {
    active: boolean
    epoch: number
    atMs: number
    departedAtMs: number | null
    /** Set by the iOS shell after it took off a cover whose frame was never confirmed. */
    unconfirmedRelease?: { epoch: number; afterSeconds: number }
}
export interface AppLockNativePolicy {
    enabled: boolean
    timing: AppLockTiming
    gateRequired: boolean
    hideRecentsPreview: boolean
    backgroundColor: string
}
export interface AppLockNativeAdapter {
    check(): Promise<AppLockCapabilities>
    authenticate(reason: string): Promise<OsAuthOutcome>
    setPolicy(policy: AppLockNativePolicy): Promise<AppLockLifecycle>
    ready(epoch: number, locked: boolean): Promise<boolean>
    listen(listener: (snapshot: AppLockLifecycle) => void): Promise<() => void>
    /** Leaves one line in the device log. Never throws. */
    report(message: string): void
}

/** Cover releases this page already wrote to the device log, shared by the startup surface and the controller. */
const reportedReleases = new Set<number>()

/** The same release can arrive in several events and policy answers, in any order. */
function reportUnconfirmedRelease(snapshot: AppLockLifecycle) {
    const release = snapshot.unconfirmedRelease
    if (!release || reportedReleases.has(release.epoch)) return
    reportedReleases.add(release.epoch)
    void recordNativeLogError(`app-lock cover released after ${release.afterSeconds} s without a confirmed frame: epoch ${release.epoch}`).catch(() => {})
}

export function createAppLockNativeAdapter(): AppLockNativeAdapter {
    return {
        async check() {
            return invoke<AppLockCapabilities>('app_lock_check')
        },
        async authenticate(reason) {
            return invoke<OsAuthOutcome>('app_lock_authenticate', { reason })
        },
        async setPolicy(policy) {
            const snapshot = await invoke<AppLockLifecycle>('app_lock_set_policy', { policy })
            reportUnconfirmedRelease(snapshot)
            return snapshot
        },
        async ready(epoch, locked) {
            return invoke<boolean>('app_lock_ready', { epoch, locked })
        },
        async listen(listener) {
            const deliver = (snapshot: AppLockLifecycle) => {
                reportUnconfirmedRelease(snapshot)
                listener(snapshot)
            }
            const nativeUnlisten = await listen<AppLockLifecycle>('risunest-app-lock', (event) => deliver(event.payload))
            const onMobileLifecycle = (event: Event) => deliver((event as CustomEvent<AppLockLifecycle>).detail)
            window.addEventListener('risunest-app-lock', onMobileLifecycle)
            return () => {
                nativeUnlisten()
                window.removeEventListener('risunest-app-lock', onMobileLifecycle)
            }
        },
        report(message) {
            void recordNativeLogError(message).catch(() => {})
        },
    }
}

export function normalizeAppLockBackground(color: string): string | null {
    const value = color.trim()
    if (/^#[\da-f]{6}$/i.test(value)) return value.toLowerCase()
    if (/^#[\da-f]{3}$/i.test(value)) return '#' + [...value.slice(1)].map((digit) => digit + digit).join('').toLowerCase()
    const channels = value.match(/^rgba?\(\s*([\d.]+%?)[,\s]+([\d.]+%?)[,\s]+([\d.]+%?)(?:\s*[,/]\s*[\d.]+%?)?\s*\)$/i)
    if (channels) {
        const bytes = channels.slice(1, 4).map((channel) => Math.round(Math.max(0, Math.min(255, parseFloat(channel) * (channel.endsWith('%') ? 2.55 : 1)))))
        return '#' + bytes.map((byte) => byte.toString(16).padStart(2, '0')).join('')
    }
    if (typeof document === 'undefined' || typeof CSS === 'undefined' || !CSS.supports('color', value)) return null
    const canvas = document.createElement('canvas')
    canvas.width = canvas.height = 1
    const context = canvas.getContext('2d', { willReadFrequently: true })
    if (!context) return null
    context.fillStyle = value
    context.fillRect(0, 0, 1, 1)
    const pixel = context.getImageData(0, 0, 1, 1).data
    return '#' + [...pixel.slice(0, 3)].map((byte) => byte.toString(16).padStart(2, '0')).join('')
}
