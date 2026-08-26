import type { BlobStore } from './blobStore'
import type { AssetRepositoryAuthorityState } from './persistentDataStore'

export type { AssetRepositoryAuthorityState } from './persistentDataStore'

export type CompleteAssetRepositoryBlobStore = BlobStore & Required<Pick<BlobStore, 'putNewInlayImage'>>

export interface AssetRepositoryAuthorityStores {
    legacy: BlobStore
    v2?: BlobStore
    v2Capability?: boolean
}

function stateRecord(value: unknown): Record<string, unknown> {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
        throw new TypeError('Asset repository authority state must be an object')
    }
    return value as Record<string, unknown>
}

function requireExactKeys(record: Record<string, unknown>, expected: string[]): void {
    const actual = Object.keys(record).sort()
    const sortedExpected = [...expected].sort()
    if (
        actual.length !== sortedExpected.length
        || actual.some((key, index) => key !== sortedExpected[index])
    ) {
        throw new TypeError('Asset repository authority state fields are invalid')
    }
}

function migrationId(value: unknown): string {
    if (
        typeof value !== 'string'
        || value.length === 0
        || value.length > 64
        || !/^[A-Za-z0-9_-]+$/.test(value)
    ) {
        throw new TypeError('Asset repository migrationId is invalid')
    }
    return value
}

export function parseAssetRepositoryAuthorityState(
    value: unknown,
): AssetRepositoryAuthorityState {
    const record = stateRecord(value)
    if (record.format === 'legacy') {
        requireExactKeys(record, ['format'])
        return { format: 'legacy' }
    }
    if (record.format === 'preparing') {
        requireExactKeys(record, ['format', 'migrationId', 'sourceRevision'])
        const sourceRevision = record.sourceRevision
        if (
            typeof sourceRevision !== 'number'
            || !Number.isSafeInteger(sourceRevision)
            || sourceRevision < 0
        ) {
            throw new TypeError('Asset repository sourceRevision is invalid')
        }
        return {
            format: 'preparing',
            migrationId: migrationId(record.migrationId),
            sourceRevision,
        }
    }
    if (record.format === 'v2') {
        requireExactKeys(record, ['format', 'migrationId', 'compatibilityHash'])
        if (
            typeof record.compatibilityHash !== 'string'
            || !/^[0-9a-f]{64}$/.test(record.compatibilityHash)
        ) {
            throw new TypeError('Asset repository compatibilityHash is invalid')
        }
        return {
            format: 'v2',
            migrationId: migrationId(record.migrationId),
            compatibilityHash: record.compatibilityHash,
        }
    }
    throw new TypeError('Asset repository authority format is invalid')
}

function requireCompleteV2Facade(store: BlobStore | undefined): CompleteAssetRepositoryBlobStore {
    if (!store) {
        throw new TypeError('Asset repository v2 requires a complete BlobStore facade')
    }
    for (const operation of [
        'put',
        'putNewInlayImage',
        'read',
        'stat',
        'list',
        'remove',
        'resolveUrl',
    ] as const) {
        if (typeof store[operation] !== 'function') {
            throw new TypeError('Asset repository v2 requires a complete BlobStore facade')
        }
    }
    return store as CompleteAssetRepositoryBlobStore
}

export function selectAssetRepositoryAuthority(
    state: unknown,
    stores: AssetRepositoryAuthorityStores,
): BlobStore {
    const parsed = parseAssetRepositoryAuthorityState(state)
    if (parsed.format === 'legacy') return stores.legacy
    if (parsed.format === 'preparing') {
        throw new Error('Asset repository preparing generation cannot be selected as authority')
    }
    if (stores.v2Capability !== true || stores.v2 === stores.legacy) {
        throw new Error('Asset repository v2 capability is unavailable, refusing legacy fallback')
    }
    return requireCompleteV2Facade(stores.v2)
}
