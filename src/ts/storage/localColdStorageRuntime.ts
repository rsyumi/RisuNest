import { decodeColdStoragePayload, encodeColdStoragePayload } from '../process/coldstorageData'
import type { ColdPayloadStore } from './coldPayloadStore'

export interface LocalColdStorageRuntime {
    read(key: string): Promise<unknown | null>
    write(key: string, value: unknown): Promise<boolean>
    list(): Promise<string[]>
    remove(keys: readonly string[]): Promise<void>
}

export function createLocalColdStorageRuntime(store: ColdPayloadStore): LocalColdStorageRuntime {
    return {
        async read(key) {
            try {
                const bytes = await store.read(key)
                return bytes === null ? null : await decodeColdStoragePayload(bytes)
            } catch {
                return null
            }
        },
        async write(key, value) {
            try {
                await store.write(key, await encodeColdStoragePayload(value))
                return true
            } catch {
                return false
            }
        },
        list: () => store.list(),
        async remove(keys) {
            for (const key of keys) await store.remove(key)
        },
    }
}
