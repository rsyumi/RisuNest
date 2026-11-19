import type { NativeDeviceSettings } from './nativeDeviceSettings'

/**
 * Settings that belong to this installation rather than to the library. A
 * native install keeps them in the device file; the web build has no device
 * tier and keeps them in local storage.
 */
export const DEVICE_MARKER_KEYS = [
    'accountst',
    'ignoreRisuAuth',
    'dosync',
    'hub',
    'risunest_tos_v1',
    'risu_service_tos_v1',
    'risu_lastsaved',
    'nightlyWarned',
    'risuNestDeviceSettings',
    'risuNestStartupExclusions',
    'risuNestPluginSyncSettings',
    'risuNestDefaultPluginsAttempted',
    'risuNestUpdateSettings',
    'risuNestServerSyncRestoreHold',
    'mcpStdioApprovals',
    'risuNestDisplayRecovery',
    'risuNestAppLock',
] as const

export type DeviceMarkerKey = (typeof DEVICE_MARKER_KEYS)[number]

export interface DeviceMarkerStorage {
    getItem(key: string): string | null
    setItem(key: string, value: string): void
    removeItem(key: string): void
    /** Publishes this value only after its own durable write succeeds. */
    commitItem?(key: string, value: string | null): Promise<void>
    /** Resolves once every queued change has committed. */
    flush(): Promise<void>
    /** Resolves once every queued change has finished, leaving a failure for the next `flush` to report. */
    settled?(): Promise<void>
    reload?(): Promise<void>
}

function requireKey(key: string): DeviceMarkerKey {
    if (!(DEVICE_MARKER_KEYS as readonly string[]).includes(key)) {
        throw new Error(`${key} is not a device marker`)
    }
    return key as DeviceMarkerKey
}

function readLoaded(key: string, loaded: unknown): string | null {
    if (loaded === null || loaded === undefined) return null
    if (typeof loaded !== 'string') {
        if (key === 'risuNestAppLock') return ''
        throw new Error(`Device marker ${key} is unreadable`)
    }
    return loaded
}

/**
 * A synchronous view over the markers for callers that read back what they just
 * wrote. Each change is sent as a single key and is durable once its native
 * transaction commits; `flush` waits for those commits and reports the first
 * one that failed.
 */
export function createNativeDeviceMarkers(
    settings: NativeDeviceSettings,
    loaded: (unknown | null)[],
): DeviceMarkerStorage {
    const values = new Map<string, string>()
    DEVICE_MARKER_KEYS.forEach((key, index) => {
        const value = readLoaded(key, loaded[index])
        if (value !== null) values.set(key, value)
    })
    const revisions = new Map<string, number>()
    const revise = (key: string) => {
        const revision = (revisions.get(key) ?? 0) + 1
        revisions.set(key, revision)
        return revision
    }
    let tail = Promise.resolve()
    let failure: unknown = null
    const enqueue = (operation: () => Promise<void>): void => {
        const next = tail.then(operation, operation)
        tail = next.then(
            () => undefined,
            (error) => {
                if (failure === null) failure = error
            },
        )
    }
    return {
        getItem: (key) => values.get(requireKey(key)) ?? null,
        setItem(key, value) {
            requireKey(key)
            if (values.get(key) === value) return
            revise(key)
            values.set(key, value)
            enqueue(() => settings.set(key, value))
        },
        removeItem(key) {
            requireKey(key)
            if (!values.has(key)) return
            revise(key)
            values.delete(key)
            enqueue(() => settings.set(key, null))
        },
        commitItem(key, value) {
            requireKey(key)
            const revision = revise(key)
            const commit = tail.then(async () => {
                await settings.set(key, value)
                if (revisions.get(key) === revision) {
                    if (value === null) values.delete(key)
                    else values.set(key, value)
                }
            })
            // Durable callers receive their own failure; legacy flush failures stay independent.
            tail = commit.then(() => undefined, () => undefined)
            return commit
        },
        async reload() {
            await this.flush()
            const loaded = await settings.readMany(DEVICE_MARKER_KEYS)
            const next = new Map<string, string>()
            DEVICE_MARKER_KEYS.forEach((key, index) => {
                const value = readLoaded(key, loaded[index])
                if (value !== null) next.set(key, value)
            })
            values.clear()
            for (const [key, value] of next) values.set(key, value)
        },
        settled: () => tail,
        async flush() {
            await tail
            if (failure !== null) {
                const error = failure
                failure = null
                throw error
            }
        },
    }
}

export function createLocalDeviceMarkers(storage: Storage): DeviceMarkerStorage {
    return {
        getItem: (key) => storage.getItem(requireKey(key)),
        setItem: (key, value) => storage.setItem(requireKey(key), value),
        removeItem: (key) => storage.removeItem(requireKey(key)),
        async commitItem(key, value) {
            if (value === null) storage.removeItem(requireKey(key))
            else storage.setItem(requireKey(key), value)
        },
        async flush() {},
    }
}

// The device maintenance that runs before the app starts reads update settings
// through these markers, so this module detects the install itself instead of
// pulling the platform module into that path.
function nativeInstall(): boolean {
    return typeof window !== 'undefined'
        && !!(window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
}

let installed: DeviceMarkerStorage | null = null

export async function initializeDeviceMarkers(
    source?: NativeDeviceSettings,
): Promise<DeviceMarkerStorage> {
    const settings = source
        ?? (await import('./nativeDeviceSettings')).createNativeDeviceSettings()
    installed = createNativeDeviceMarkers(settings, await settings.readMany(DEVICE_MARKER_KEYS))
    return installed
}

export function installDeviceMarkers(markers: DeviceMarkerStorage | null): void {
    installed = markers
}

/**
 * A native install must load the device file first, so reading before that is a
 * start-order mistake rather than a missing value.
 */
export function getDeviceMarkers(): DeviceMarkerStorage {
    if (installed) return installed
    if (nativeInstall()) throw new Error('Device markers are not loaded yet')
    installed = createLocalDeviceMarkers(localStorage)
    return installed
}

export async function reloadDeviceMarkers(): Promise<void> {
    await getDeviceMarkers().reload?.()
}
