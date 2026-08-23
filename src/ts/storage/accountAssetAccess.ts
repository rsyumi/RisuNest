import type { BlobStore, BlobWriteMetadata } from './blobStore'
import type { AccountStorage } from './accountStorage'

export type OfficialAccountAssetReader = (key: string) => Promise<Uint8Array | null>

let officialAccountAssetReader: OfficialAccountAssetReader | null = null

export function configureOfficialAccountAssetReader(
    reader: OfficialAccountAssetReader | null,
): void {
    officialAccountAssetReader = reader
}

export function createStructuredAccountAssetReader(
    account: Pick<AccountStorage, 'readItem'>,
): OfficialAccountAssetReader {
    return async (key) => {
        const result = await account.readItem(key)
        return result.kind === 'missing' ? null : result.bytes
    }
}

function isAccountAssetKey(key: string): boolean {
    const normalized = key.replace(/\\/g, '/')
    return normalized.startsWith('assets/') && normalized.length > 'assets/'.length
}

export async function readActiveAsset(
    blobStore: BlobStore,
    key: string,
    mode: { officialAccount: boolean, tauri: boolean },
): Promise<Uint8Array | null> {
    let value = await blobStore.read(key)
    if (value === null
        && mode.officialAccount
        && isAccountAssetKey(key)
        && officialAccountAssetReader) {
        value = await officialAccountAssetReader(key)
    }
    if (value === null && mode.tauri) throw new Error(`Missing asset: ${key}`)
    return value
}

export async function storeActiveAsset(
    blobStore: BlobStore,
    key: string,
    data: Uint8Array,
    metadata: BlobWriteMetadata,
): Promise<void> {
    await blobStore.put(key, data, metadata)
}
