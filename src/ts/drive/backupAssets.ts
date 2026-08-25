import type { BlobStore, BlobWriteMetadata, InlayBlobMetadata } from '../storage/blobStore'
import type { Database } from '../storage/database.svelte'
import { readActiveAsset, storeActiveAsset } from '../storage/accountAssetAccess'
import { listCharacterResources, listDatabaseRootResources } from '../process/coldstorageData'

const INLAY_ENTRY_NAME = /^inlay_((?:[0-9a-f]{2})+)\.risuinlay$/
const INLAY_TYPES = new Set(['image', 'video', 'audio', 'signature'])

export function isLegacyBackupAssetKey(key: string): boolean {
    const normalized = key.replace(/\\/g, '/')
    return normalized.startsWith('assets/') && normalized.length > 'assets/'.length
}

export function selectLegacyBackupAssetKeys(keys: readonly string[]): string[] {
    return keys.filter(isLegacyBackupAssetKey)
}

export async function collectBackupAssetKeys(
    _store: BlobStore,
    referencedKeys: Iterable<string>,
): Promise<string[]> {
    const keys = new Set<string>()
    for (const key of referencedKeys) {
        if (isLegacyBackupAssetKey(key)) keys.add(key.replace(/\\/g, '/'))
    }
    return Array.from(keys).sort()
}

export function collectPinnedBackupAssetReferences(
    database: Database,
    coldPayloadValues: readonly unknown[],
): string[] {
    const coldCharacters = new Map<string, Database['characters'][number]>()
    for (const value of coldPayloadValues) {
        if (!value || typeof value !== 'object' || Array.isArray(value) || !('character' in value)) {
            continue
        }
        const character = value.character
        if (!character || typeof character !== 'object' || !('chaId' in character)
            || typeof character.chaId !== 'string') {
            continue
        }
        coldCharacters.set(character.chaId, character as Database['characters'][number])
    }

    const references = new Set<string>()
    const add = (key: string) => {
        if (isLegacyBackupAssetKey(key)) references.add(key.replace(/\\/g, '/'))
    }
    for (const key of listDatabaseRootResources(database)) add(key)
    for (const character of database.characters) {
        for (const key of listCharacterResources(
            coldCharacters.get(character.chaId) ?? character,
        )) add(key)
    }
    return Array.from(references).sort()
}

function collectInlayReferences(value: unknown, references: Set<string>, seen: WeakSet<object>): void {
    if (typeof value === 'string') {
        for (const match of value.matchAll(/\{\{(?:inlay|inlayed|inlayeddata)::(.+?)\}\}/g)) {
            references.add(match[1])
        }
        return
    }
    if (!value || typeof value !== 'object' || seen.has(value)) return
    seen.add(value)
    for (const child of Object.values(value)) collectInlayReferences(child, references, seen)
}

export async function collectReferencedBackupInlays(
    store: BlobStore,
    logicalSnapshot: unknown,
): Promise<InlayBlobMetadata[]> {
    const references = new Set<string>()
    collectInlayReferences(logicalSnapshot, references, new WeakSet())
    return (await store.list({ kind: 'inlay' })).filter(
        (metadata): metadata is InlayBlobMetadata =>
            metadata.kind === 'inlay'
            && !isLegacyBackupAssetKey(metadata.key)
            && references.has(metadata.key),
    )
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

export interface BackupInlayEntry {
    key: string
    metadata: BlobWriteMetadata
    data: Uint8Array
}

/** Hex keeps the id single segment, so the basename the writer applies cannot truncate it. */
export function getBackupInlayName(key: string): string {
    return `inlay_${Buffer.from(key, 'utf-8').toString('hex')}.risuinlay`
}

export function encodeBackupInlayEntry(metadata: InlayBlobMetadata, data: Uint8Array): Uint8Array {
    const header = new TextEncoder().encode(JSON.stringify(metadata))
    const entry = new Uint8Array(4 + header.byteLength + data.byteLength)
    new DataView(entry.buffer).setUint32(0, header.byteLength, true)
    entry.set(header, 4)
    entry.set(data, 4 + header.byteLength)
    return entry
}

export function decodeBackupInlayEntry(name: string, entry: Uint8Array): BackupInlayEntry | null {
    if (!INLAY_ENTRY_NAME.test(name) || entry.byteLength < 4) return null
    const headerLength = new DataView(entry.buffer, entry.byteOffset, entry.byteLength).getUint32(0, true)
    if (headerLength === 0 || 4 + headerLength > entry.byteLength) return null
    let header: unknown
    try {
        header = JSON.parse(new TextDecoder().decode(entry.subarray(4, 4 + headerLength)))
    } catch {
        return null
    }
    if (!header || typeof header !== 'object' || Array.isArray(header)) return null
    const { key, size: _size, ...metadata } = header as Partial<InlayBlobMetadata>
    if (typeof key !== 'string' || key === '' || isLegacyBackupAssetKey(key)) return null
    if (metadata.kind !== 'inlay' || !INLAY_TYPES.has(metadata.inlayType as string)) return null
    if (typeof metadata.mime !== 'string' || typeof metadata.name !== 'string'
        || typeof metadata.ext !== 'string') {
        return null
    }
    return {
        key,
        metadata: metadata as BlobWriteMetadata,
        data: entry.slice(4 + headerLength),
    }
}
