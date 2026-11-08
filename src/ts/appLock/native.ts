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

export function createAppLockNativeAdapter(): AppLockNativeAdapter {
    return {
        async check() {
            return invoke<AppLockCapabilities>('app_lock_check')
        },
        async authenticate(reason) {
            return invoke<OsAuthOutcome>('app_lock_authenticate', { reason })
        },
        async setPolicy(policy) {
            return invoke<AppLockLifecycle>('app_lock_set_policy', { policy })
        },
        async ready(epoch, locked) {
            return invoke<boolean>('app_lock_ready', { epoch, locked })
        },
        async listen(listener) {
            const nativeUnlisten = await listen<AppLockLifecycle>('risunest-app-lock', (event) => listener(event.payload))
            const onMobileLifecycle = (event: Event) => listener((event as CustomEvent<AppLockLifecycle>).detail)
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
