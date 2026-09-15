import {
    setRuntimePerformanceProfile,
    type RuntimePerformanceProfile,
} from '../runtimePerformanceProfile'
import {
    RECOVERY_EXCLUSIONS,
    type RecoveryExclusion,
} from './startupExclusions'

export interface RisuNestDeviceSettings {
    schema: 'risunest.device-settings/v2'
    performanceProfile: RuntimePerformanceProfile
    androidKeepAliveDuringGeneration: boolean
    nativeFileLogEnabled: boolean
    /**
     * What every start on this device leaves switched off. Written only after a start that
     * finished and only when the reader confirms it, and kept per device rather than in the
     * library, so it never travels through a backup or a sync.
     */
    startupExclusions: RecoveryExclusion[]
}

const storageKey = 'risuNestDeviceSettings'

const defaults: RisuNestDeviceSettings = {
    schema: 'risunest.device-settings/v2',
    performanceProfile: 'normal',
    androidKeepAliveDuringGeneration: true,
    nativeFileLogEnabled: true,
    startupExclusions: [],
}

function snapshot(settings: RisuNestDeviceSettings): RisuNestDeviceSettings {
    return { ...settings, startupExclusions: [...settings.startupExclusions] }
}

function isExclusionList(value: unknown): value is RecoveryExclusion[] {
    return (
        Array.isArray(value) &&
        value.every((item) =>
            RECOVERY_EXCLUSIONS.includes(item as RecoveryExclusion),
        )
    )
}

function isValidSettings(value: unknown): value is RisuNestDeviceSettings {
    if (!value || typeof value !== 'object') return false
    const settings = value as Record<string, unknown>
    return (
        Object.keys(settings).length === 5 &&
        settings.schema === defaults.schema &&
        (settings.performanceProfile === 'normal' ||
            settings.performanceProfile === 'low-spec') &&
        typeof settings.androidKeepAliveDuringGeneration === 'boolean' &&
        typeof settings.nativeFileLogEnabled === 'boolean' &&
        isExclusionList(settings.startupExclusions)
    )
}

function readSettings(): RisuNestDeviceSettings {
    let stored: string | null
    try {
        stored = localStorage.getItem(storageKey)
    } catch {
        return snapshot(defaults)
    }

    if (!stored) return snapshot(defaults)

    let parsed: unknown
    try {
        parsed = JSON.parse(stored)
    } catch {
        return snapshot(defaults)
    }

    return isValidSettings(parsed)
        ? snapshot(parsed)
        : snapshot(defaults)
}

let settings: RisuNestDeviceSettings | undefined
const listeners = new Set<(settings: RisuNestDeviceSettings) => void>()

function initializeSettings(): RisuNestDeviceSettings {
    if (settings) return settings
    settings = readSettings()
    setRuntimePerformanceProfile(settings.performanceProfile)
    return settings
}

export function getDeviceSettings(): RisuNestDeviceSettings {
    return snapshot(initializeSettings())
}

export function updateDeviceSettings(
    partial: Partial<Omit<RisuNestDeviceSettings, 'schema'>>,
): RisuNestDeviceSettings {
    const { schema: _schema, ...updates } = partial as Partial<RisuNestDeviceSettings>
    const currentSettings = initializeSettings()
    const next = { ...currentSettings, ...updates }
    const previousPerformanceProfile = currentSettings.performanceProfile
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
    initializeSettings()
    listeners.add(listener)
    return () => listeners.delete(listener)
}
