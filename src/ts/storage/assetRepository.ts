import {
    validateBlobReadRange,
    type BlobReadRange,
    type BlobMetadata,
    type BlobStore,
} from './blobStore'
import { hashPayloadBytes, type ImmutablePayloadCas } from './payloadCas'
import { validateAssetAlias } from './persistentDataStore'
import type {
    AssetAlias,
    PersistentDataStore,
    Versioned,
} from './persistentDataStore'

export type AssetAliasPayloadSource = 'cas' | 'legacy' | 'missing'

export interface AssetAliasRead {
    alias: AssetAlias
    data: Uint8Array | null
    source: AssetAliasPayloadSource
}

export interface AssetAliasStat {
    alias: AssetAlias
    objectSize: number | null
    source: AssetAliasPayloadSource
}

export interface AssetRepository {
    read(key: string, range?: BlobReadRange): Promise<Versioned<AssetAliasRead> | null>
    stat(key: string): Promise<Versioned<AssetAliasStat> | null>
}

export interface AssetRepositoryOptions {
    reader: Pick<PersistentDataStore, 'readAssetAlias'>
    cas: ImmutablePayloadCas
    legacy: BlobStore
    legacyFallback: boolean
}

function blobRange(data: Uint8Array, range?: BlobReadRange): Uint8Array {
    if (!range) return data
    validateBlobReadRange(range)
    return data.slice(
        Math.min(range.start, data.byteLength),
        Math.min(range.endExclusive, data.byteLength),
    )
}

function aliasBlobMetadata(alias: AssetAlias): BlobMetadata {
    if (alias.kind === 'asset') {
        return {
            key: alias.key,
            kind: 'asset',
            size: alias.size,
            mime: alias.mime,
            name: alias.name,
            ext: alias.ext,
        }
    }
    if (alias.inlayType === undefined) {
        throw new Error(`Inlay asset alias is missing inlayType for ${alias.key}`)
    }
    return {
        key: alias.key,
        kind: 'inlay',
        size: alias.size,
        mime: alias.mime,
        name: alias.name,
        ext: alias.ext,
        inlayType: alias.inlayType,
        ...(alias.width === undefined ? {} : { width: alias.width }),
        ...(alias.height === undefined ? {} : { height: alias.height }),
    }
}

export function createAssetRepository(options: AssetRepositoryOptions): AssetRepository {
    const { reader, cas, legacy } = options
    return {
        async read(key, range) {
            if (range) validateBlobReadRange(range)
            const versioned = await reader.readAssetAlias({ kind: 'asset', key })
            if (!versioned) return null
            const alias = versioned.value
            validateAssetAlias(alias)
            if (alias.objectHash !== null) {
                const data = await cas.readObject(alias.objectHash)
                if (data !== null) {
                    if (data.byteLength !== alias.size) {
                        throw new Error(`Asset alias size mismatch for ${key}`)
                    }
                    return {
                        revision: versioned.revision,
                        value: { alias, data: blobRange(data, range), source: 'cas' },
                    }
                }
            }
            if (options.legacyFallback) {
                const data = await legacy.read(key)
                if (data !== null) {
                    if (data.byteLength !== alias.size) {
                        throw new Error(`Asset alias legacy size mismatch for ${key}`)
                    }
                    if (
                        alias.objectHash !== null
                        && await hashPayloadBytes(data) !== alias.objectHash
                    ) {
                        throw new Error(`Asset alias legacy hash mismatch for ${key}`)
                    }
                    return {
                        revision: versioned.revision,
                        value: { alias, data: blobRange(data, range), source: 'legacy' },
                    }
                }
            }
            return {
                revision: versioned.revision,
                value: { alias, data: null, source: 'missing' },
            }
        },
        async stat(key) {
            const versioned = await reader.readAssetAlias({ kind: 'asset', key })
            if (!versioned) return null
            const alias = versioned.value
            validateAssetAlias(alias)
            if (alias.objectHash !== null) {
                const objectSize = await cas.statObject(alias.objectHash)
                if (objectSize !== null) {
                    if (objectSize !== alias.size) {
                        throw new Error(`Asset alias size mismatch for ${key}`)
                    }
                    return {
                        revision: versioned.revision,
                        value: { alias, objectSize, source: 'cas' },
                    }
                }
            }
            if (options.legacyFallback) {
                const metadata = await legacy.stat(key)
                if (metadata !== null) {
                    if (metadata.size !== alias.size) {
                        throw new Error(`Asset alias legacy size mismatch for ${key}`)
                    }
                    return {
                        revision: versioned.revision,
                        value: { alias, objectSize: metadata.size, source: 'legacy' },
                    }
                }
            }
            return {
                revision: versioned.revision,
                value: { alias, objectSize: null, source: 'missing' },
            }
        },
    }
}

export interface AssetRepositoryBlobStoreOptions {
    store: PersistentDataStore
    cas: ImmutablePayloadCas
    legacy: BlobStore
    legacyFallback: boolean
    removal: 'disabled' | 'legacy-only'
}

export type AssetRepositoryBlobStoreFacade = Pick<
    BlobStore,
    'put' | 'read' | 'stat' | 'remove'
>

export function createAssetRepositoryBlobStore(
    options: AssetRepositoryBlobStoreOptions,
): AssetRepositoryBlobStoreFacade {
    const { store, cas, legacy } = options
    const repository = createAssetRepository({
        reader: store,
        cas,
        legacy,
        legacyFallback: options.legacyFallback,
    })
    return {
        async put(key, data, metadata) {
            const ownedData = data.slice()
            const pendingAlias = {
                ...metadata,
                key,
                objectHash: null,
                size: ownedData.byteLength,
            } as AssetAlias
            validateAssetAlias(pendingAlias)
            const prepared = await cas.prepare(ownedData)
            const alias: AssetAlias = {
                ...pendingAlias,
                objectHash: prepared.contentHash,
            }
            validateAssetAlias(alias)
            const { revision } = await store.readRoot()
            await store.commitAssetAlias(alias, revision)
            return aliasBlobMetadata(alias)
        },
        async read(key, range) {
            const result = await repository.read(key, range)
            if (result) return result.value.data
            return options.legacyFallback ? legacy.read(key, range) : null
        },
        async stat(key) {
            const result = await repository.stat(key)
            if (result) {
                if (result.value.source === 'missing') return null
                return aliasBlobMetadata(result.value.alias)
            }
            return options.legacyFallback ? legacy.stat(key) : null
        },
        async remove(key) {
            if (options.legacyFallback && options.removal === 'legacy-only') {
                await legacy.remove(key)
            }
        },
    }
}
