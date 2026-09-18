import localforage from 'localforage'
import type { BlobMetadata, BlobStore, BlobWriteMetadata } from '../storage/blobStore'

type Attachment = { data: Uint8Array, metadata: BlobWriteMetadata }

export function createLegacyBackupAttachments(store: BlobStore) {
    const staging = localforage.createInstance({
        name: `legacy-restore-${crypto.randomUUID()}`,
        driver: localforage.INDEXEDDB,
    })
    const keys = new Set<string>()
    const written: string[] = []
    let rollbackFailed = false
    return {
        async put(key: string, data: Uint8Array, metadata: BlobWriteMetadata): Promise<BlobMetadata> {
            await staging.setItem(`incoming:${key}`, { data, metadata })
            keys.add(key)
            return { ...metadata, key, size: data.byteLength } as BlobMetadata
        },
        async activate(checkCancelled: () => void) {
            for (const key of keys) {
                checkCancelled()
                const metadata = await store.stat(key)
                const data = await store.read(key)
                if ((metadata === null) !== (data === null)) {
                    throw new Error('Cannot preserve incomplete attachment before restore')
                }
                await staging.setItem(`original:${key}`, data === null ? null : { data, metadata })
                const incoming = await staging.getItem<Attachment>(`incoming:${key}`)
                if (!incoming) throw new Error('Missing staged backup attachment')
                // A failed put may already have replaced the payload.
                written.push(key)
                await store.put(key, incoming.data, incoming.metadata)
            }
            checkCancelled()
        },
        async rollback() {
            const failures: unknown[] = []
            for (const key of [...written].reverse()) {
                try {
                    const original = await staging.getItem<Attachment>(`original:${key}`)
                    if (original) await store.put(key, original.data, original.metadata)
                    else await store.remove(key)
                } catch (error) {
                    failures.push(error)
                }
            }
            if (failures.length) {
                rollbackFailed = true
                throw new AggregateError(failures, 'Backup attachment rollback failed')
            }
        },
        // Keep the original payloads if storage could not restore them.
        dispose: async () => { if (!rollbackFailed) await staging.dropInstance() },
    }
}
