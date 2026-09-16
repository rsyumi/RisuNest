import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'

export interface NativeDeviceSettings {
    get(key: string): Promise<unknown | null>
    /** Reads several settings at once, answering in request order. */
    readMany(keys: readonly string[]): Promise<(unknown | null)[]>
    /** A null value removes the setting. */
    set(key: string, value: unknown | null): Promise<void>
    /** Merges single entries of a stored object. A null entry removes it. */
    patch(key: string, entries: Record<string, string | null>): Promise<void>
}

export interface NativeDeviceSettingsBag {
    storage: {
        getItem(key: string): string | null
        setItem(key: string, value: string): void
        removeItem(key: string): void
    }
    flush(): Promise<void>
    clear(): Promise<void>
}

function requireTauri(): void {
    if (!isTauri) throw new Error('Device settings require Tauri')
}

export function createNativeDeviceSettings(): NativeDeviceSettings {
    return {
        async get(key: string): Promise<unknown | null> {
            requireTauri()
            return invoke<unknown | null>('pds_get_device_setting', { key })
        },
        async readMany(keys: readonly string[]): Promise<(unknown | null)[]> {
            requireTauri()
            return invoke<(unknown | null)[]>('pds_read_device_settings', { keys: [...keys] })
        },
        async set(key: string, value: unknown | null): Promise<void> {
            requireTauri()
            await invoke('pds_set_device_setting', { key, value: value ?? null })
        },
        async patch(key: string, entries: Record<string, string | null>): Promise<void> {
            requireTauri()
            await invoke('pds_patch_device_setting', { key, entries })
        },
    }
}

function readEntries(key: string, loaded: unknown): Record<string, string> {
    if (loaded === null || loaded === undefined) return {}
    if (typeof loaded !== 'object' || Array.isArray(loaded)) {
        throw new Error(`Device setting ${key} does not hold entries`)
    }
    const entries: Record<string, string> = {}
    for (const [entry, value] of Object.entries(loaded)) {
        if (typeof value !== 'string') {
            throw new Error(`Device setting ${key} holds an unreadable entry`)
        }
        entries[entry] = value
    }
    return entries
}

/**
 * A synchronous view over one stored object for callers that read back what
 * they just wrote. Each change is sent as a single entry and is durable once
 * its native transaction commits; `flush` waits for those commits and reports
 * the first one that failed.
 */
export async function createNativeDeviceSettingsBag(
    settings: NativeDeviceSettings,
    key: string,
): Promise<NativeDeviceSettingsBag> {
    let tail = Promise.resolve()
    let failure: unknown = null
    // A device file that cannot be read must not take the library with it. The
    // bag starts empty and reports the failure at the first flush instead.
    let values: Record<string, string> = {}
    try {
        values = readEntries(key, await settings.get(key))
    } catch (error) {
        failure = error
    }
    const enqueue = (operation: () => Promise<void>): void => {
        const next = tail.then(operation, operation)
        tail = next.then(
            () => undefined,
            (error) => {
                if (failure === null) failure = error
            },
        )
    }
    const flush = async (): Promise<void> => {
        await tail
        if (failure !== null) {
            const error = failure
            failure = null
            throw error
        }
    }
    return {
        storage: {
            getItem: (entry) => values[entry] ?? null,
            setItem(entry, value) {
                if (values[entry] === value) return
                values[entry] = value
                enqueue(() => settings.patch(key, { [entry]: value }))
            },
            removeItem(entry) {
                if (!(entry in values)) return
                delete values[entry]
                enqueue(() => settings.patch(key, { [entry]: null }))
            },
        },
        flush,
        async clear() {
            for (const entry of Object.keys(values)) delete values[entry]
            enqueue(() => settings.set(key, null))
            await flush()
        },
    }
}
