import {
    setRuntimePerformanceProfile,
    type RuntimePerformanceProfile,
} from '../runtimePerformanceProfile'

export interface RisuNestDeviceSettings {
    schema: 'risunest.device-settings/v1'
    performanceProfile: RuntimePerformanceProfile
    androidKeepAliveDuringGeneration: boolean
    nativeFileLogEnabled: boolean
    syncAutoListen: boolean
    syncListenMethod: 'lan' | 'quick' | 'fixed-url'
    syncFixedPort: number
    syncPublicBaseUrl: string
}

const storageKey = 'risuNestDeviceSettings'

const defaults: RisuNestDeviceSettings = {
    schema: 'risunest.device-settings/v1',
    performanceProfile: 'normal',
    androidKeepAliveDuringGeneration: false,
    nativeFileLogEnabled: true,
    syncAutoListen: false,
    syncListenMethod: 'lan',
    syncFixedPort: 32145,
    syncPublicBaseUrl: '',
}

function snapshot(settings: RisuNestDeviceSettings): RisuNestDeviceSettings {
    return { ...settings }
}

function isValidSettings(value: unknown): value is RisuNestDeviceSettings {
    if (!value || typeof value !== 'object') return false
    const settings = value as Record<string, unknown>
    const syncFixedPort = settings.syncFixedPort
    return settings.schema === defaults.schema
        && (settings.performanceProfile === 'normal' || settings.performanceProfile === 'low-spec')
        && typeof settings.androidKeepAliveDuringGeneration === 'boolean'
        && typeof settings.nativeFileLogEnabled === 'boolean'
        && typeof settings.syncAutoListen === 'boolean'
        && (settings.syncListenMethod === 'lan' || settings.syncListenMethod === 'quick' || settings.syncListenMethod === 'fixed-url')
        && typeof syncFixedPort === 'number'
        && Number.isInteger(syncFixedPort)
        && syncFixedPort >= 1
        && syncFixedPort <= 65535
        && typeof settings.syncPublicBaseUrl === 'string'
}

function readSettings(): RisuNestDeviceSettings {
    try {
        const stored = localStorage.getItem(storageKey)
        if (!stored) return snapshot(defaults)
        const parsed: unknown = JSON.parse(stored)
        return isValidSettings(parsed)
            ? snapshot(parsed)
            : snapshot(defaults)
    } catch {
        return snapshot(defaults)
    }
}

let settings = readSettings()
setRuntimePerformanceProfile(settings.performanceProfile)
const listeners = new Set<(settings: RisuNestDeviceSettings) => void>()

export function getDeviceSettings(): RisuNestDeviceSettings {
    return snapshot(settings)
}

export function updateDeviceSettings(
    partial: Partial<Omit<RisuNestDeviceSettings, 'schema'>>,
): RisuNestDeviceSettings {
    const { schema: _schema, ...updates } = partial as Partial<RisuNestDeviceSettings>
    const next = { ...settings, ...updates }
    const previousPerformanceProfile = settings.performanceProfile
    settings = isValidSettings(next) ? next : snapshot(defaults)
    if (settings.performanceProfile !== previousPerformanceProfile) {
        setRuntimePerformanceProfile(settings.performanceProfile)
    }
    try {
        localStorage.setItem(storageKey, JSON.stringify(settings))
    } catch {
        // Device settings remain available in memory when local storage is unavailable.
    }
    for (const listener of listeners) {
        listener(snapshot(settings))
    }
    return snapshot(settings)
}

export function subscribeDeviceSettings(
    listener: (settings: RisuNestDeviceSettings) => void,
): () => void {
    listeners.add(listener)
    return () => listeners.delete(listener)
}
