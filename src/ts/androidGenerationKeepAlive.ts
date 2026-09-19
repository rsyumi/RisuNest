import './androidNativeControl'
import { getDeviceSettings } from './storage/deviceSettings'
import { isTauriAndroid } from './platform'

export interface AndroidGenerationKeepAliveBridge {
    begin(): boolean | Promise<boolean>
    end(): boolean | void | Promise<boolean | void>
    notificationsEnabled(): boolean | Promise<boolean>
    requestNotifications(): void | Promise<void>
    openNotificationSettings(): boolean | void | Promise<boolean | void>
    webViewVersion(): string | Promise<string>
}

declare global {
    interface Window {
        RisuGenerationKeepAlive?: AndroidGenerationKeepAliveBridge
    }
}

function bridge(): AndroidGenerationKeepAliveBridge | undefined {
    return typeof window === 'undefined' ? undefined : window.RisuGenerationKeepAlive
}

export async function beginAndroidGenerationKeepAlive(enabled = getDeviceSettings().androidKeepAliveDuringGeneration): Promise<boolean> {
    if (!isTauriAndroid || !enabled) return false
    try {
        return await bridge()?.begin() === true
    } catch {
        return false
    }
}

export async function endAndroidGenerationKeepAlive(acquired: boolean): Promise<void> {
    if (!isTauriAndroid || !acquired) return
    try {
        await bridge()?.end()
    } catch {
        // Ending is best effort. The Android service timeout is the final cleanup path.
    }
}

export async function requestAndroidGenerationNotifications(): Promise<void> {
    if (!isTauriAndroid) return
    try {
        await bridge()?.requestNotifications()
    } catch {
        // Permission readiness is read separately; a failed request never implies a grant.
    }
}

export async function androidGenerationNotificationsEnabled(): Promise<boolean | null> {
    if (!isTauriAndroid) return null
    try {
        const value = await bridge()?.notificationsEnabled()
        return typeof value === 'boolean' ? value : null
    } catch {
        return null
    }
}
