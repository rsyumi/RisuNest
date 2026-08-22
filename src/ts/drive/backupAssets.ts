import type { BlobStore } from '../storage/blobStore'
import { readActiveAsset, storeActiveAsset } from '../storage/accountAssetAccess'

export const legacyBackupIncludesInlays = false

export function isLegacyBackupAssetKey(key: string): boolean {
    const normalized = key.replace(/\\/g, '/')
    return normalized.startsWith('assets/') && normalized.length > 'assets/'.length
}

export function selectLegacyBackupAssetKeys(keys: readonly string[]): string[] {
    return keys.filter(isLegacyBackupAssetKey)
}

export async function collectBackupAssetKeys(
    store: BlobStore,
    referencedKeys: Iterable<string>,
): Promise<string[]> {
    const keys = new Set((await store.list({ kind: 'asset' })).map((asset) => asset.key))
    for (const key of referencedKeys) {
        if (isLegacyBackupAssetKey(key)) keys.add(key.replace(/\\/g, '/'))
    }
    return Array.from(keys).sort()
}

export function readBackupAsset(
    store: BlobStore,
    key: string,
    officialAccount: boolean,
): Promise<Uint8Array | null> {
    return readActiveAsset(store, key, { officialAccount, tauri: false })
}

export async function writeBackupAsset(
    store: BlobStore,
    key: string,
    data: Uint8Array,
): Promise<void> {
    const name = key.replace(/\\/g, '/').split('/').pop() ?? key
    await storeActiveAsset(store, key, data, {
        kind: 'asset',
        mime: '',
        name,
        ext: name.split('.').pop() ?? '',
    })
}
