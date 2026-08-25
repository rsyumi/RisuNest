import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'

export interface NativeAppKv {
    get(key: string): Promise<unknown | null>
    set(key: string, value: unknown): Promise<void>
    remove(key: string): Promise<void>
}

export interface NativeAppKvStringStorage {
    storage: {
        getItem(key: string): string | null
        setItem(key: string, value: string): void
        removeItem(key: string): void
    }
    flush(): Promise<void>
    reset(): void
}

function requireTauri(): void {
    if (!isTauri) throw new Error('Native app KV requires Tauri')
}

export function createNativeAppKv(): NativeAppKv {
    return {
        async get(key: string): Promise<unknown | null> {
            requireTauri()
            return invoke<unknown | null>('pds_get_app_kv', { key })
        },
        async set(key: string, value: unknown): Promise<void> {
            requireTauri()
            await invoke('pds_set_app_kv', { key, value })
        },
        async remove(key: string): Promise<void> {
            requireTauri()
            await invoke('pds_remove_app_kv', { key })
        },
    }
}

export async function createNativeAppKvStringStorage(
    appKv: NativeAppKv,
    key: string,
): Promise<NativeAppKvStringStorage> {
    const loaded = await appKv.get(key)
    const values: Record<string, string> = loaded && typeof loaded === 'object'
        ? { ...loaded }
        : {}
    let dirty = false
    return {
        storage: {
            getItem: (itemKey) => values[itemKey] ?? null,
            setItem(itemKey, value) {
                if (values[itemKey] === value) return
                values[itemKey] = value
                dirty = true
            },
            removeItem(itemKey) {
                if (!(itemKey in values)) return
                delete values[itemKey]
                dirty = true
            },
        },
        async flush() {
            if (!dirty) return
            dirty = false
            try {
                await appKv.set(key, { ...values })
            } catch (error) {
                dirty = true
                throw error
            }
        },
        reset() {
            for (const itemKey of Object.keys(values)) delete values[itemKey]
            dirty = false
        },
    }
}
