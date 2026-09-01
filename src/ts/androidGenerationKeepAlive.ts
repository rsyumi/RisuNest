import { getDeviceSettings } from './storage/deviceSettings'

export interface AndroidGenerationKeepAliveBridge {
    begin(): boolean
    end(): boolean | void
    notificationsEnabled(): boolean
    openNotificationSettings(): boolean | void
    webViewVersion(): string
    transferMode(): string
}

declare global {
    interface Window {
        RisuGenerationKeepAlive?: AndroidGenerationKeepAliveBridge
    }
}

function bridge(): AndroidGenerationKeepAliveBridge | undefined {
    return typeof window === 'undefined' ? undefined : window.RisuGenerationKeepAlive
}

export function beginAndroidGenerationKeepAlive(enabled = getDeviceSettings().androidKeepAliveDuringGeneration): boolean {
    if (!enabled) return false
    try {
        return bridge()?.begin() === true
    } catch {
        return false
    }
}

export function endAndroidGenerationKeepAlive(acquired: boolean): void {
    if (!acquired) return
    try {
        bridge()?.end()
    } catch {
        // Ending is best effort. The Android service timeout is the final cleanup path.
    }
}

export function androidGenerationNotificationsEnabled(): boolean | null {
    try {
        const value = bridge()?.notificationsEnabled()
        return typeof value === 'boolean' ? value : null
    } catch {
        return null
    }
}
