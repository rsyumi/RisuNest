import {
    validateBlobReadRange,
    type BlobReadRange,
    type BlobMetadata,
    type BlobStore,
} from './blobStore'
import { hashPayloadBytes, type ImmutablePayloadCas } from './payloadCas'
import {
    validateAssetAlias,
    validateAssetAliasIdentity,
} from './persistentDataStore'
import type {
    AssetAlias,
    AssetAliasIdentity,
    AssetAliasKind,
    DataRevision,
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

export interface AssetAliasListQuery {
    kind?: AssetAliasKind
    limit: number
    cursor?: string
}

export interface AssetAliasPage {
    revision: DataRevision
    items: AssetAlias[]
    nextCursor?: string
}

export interface AssetAliasCatalog {
    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null>
    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage>
    deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
}

export interface AssetAliasLegacyReader {
    read(identity: AssetAliasIdentity): Promise<Uint8Array | null>
    stat(identity: AssetAliasIdentity): Promise<BlobMetadata | null>
}

export interface TypedAssetRepository {
    read(
        identity: AssetAliasIdentity,
        range?: BlobReadRange,
    ): Promise<Versioned<AssetAliasRead> | null>
    stat(identity: AssetAliasIdentity): Promise<Versioned<AssetAliasStat> | null>
    list(query: AssetAliasListQuery): Promise<AssetAliasPage>
    remove(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
}

export interface TypedAssetRepositoryOptions {
    catalog: AssetAliasCatalog
    cas: ImmutablePayloadCas
    legacy: AssetAliasLegacyReader
    legacyFallback: boolean
}

type TypedAssetRepositoryReader = Pick<TypedAssetRepository, 'read' | 'stat'>

interface TypedAssetRepositoryReaderOptions {
    reader: Pick<AssetAliasCatalog, 'readAssetAlias'>
    cas: ImmutablePayloadCas
    legacy: AssetAliasLegacyReader
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

function createTypedAssetRepositoryReader(
    options: TypedAssetRepositoryReaderOptions,
): TypedAssetRepositoryReader {
    const { reader, cas, legacy } = options
    return {
        async read(identity, range) {
            validateAssetAliasIdentity(identity)
            const { key } = identity
            if (range) validateBlobReadRange(range)
            const versioned = await reader.readAssetAlias(identity)
            if (!versioned) return null
            const alias = versioned.value
            validateAssetAlias(alias)
            if (alias.kind !== identity.kind || alias.key !== identity.key) {
                throw new TypeError('Asset alias does not match its requested identity')
            }
            if (alias.objectHash !== null) {
                if (range) {
                    const objectSize = await cas.statObject(alias.objectHash)
                    if (objectSize !== null) {
                        if (objectSize !== alias.size) {
                            throw new Error(`Asset alias size mismatch for ${key}`)
                        }
                        const data = await cas.readObjectRange(alias.objectHash, range)
                        if (data !== null) {
                            const expectedSize = Math.max(
                                0,
                                Math.min(range.endExclusive, alias.size)
                                - Math.min(range.start, alias.size),
                            )
                            if (data.byteLength !== expectedSize) {
                                throw new Error(`Asset alias range size mismatch for ${key}`)
                            }
                            return {
                                revision: versioned.revision,
                                value: { alias, data, source: 'cas' },
                            }
                        }
                    }
                } else {
                    const data = await cas.readObject(alias.objectHash)
                    if (data !== null) {
                        if (data.byteLength !== alias.size) {
                            throw new Error(`Asset alias size mismatch for ${key}`)
                        }
                        return {
                            revision: versioned.revision,
                            value: { alias, data, source: 'cas' },
                        }
                    }
                }
            }
            if (options.legacyFallback) {
                const data = await legacy.read(identity)
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
        async stat(identity) {
            validateAssetAliasIdentity(identity)
            const { key } = identity
            const versioned = await reader.readAssetAlias(identity)
            if (!versioned) return null
            const alias = versioned.value
            validateAssetAlias(alias)
            if (alias.kind !== identity.kind || alias.key !== identity.key) {
                throw new TypeError('Asset alias does not match its requested identity')
            }
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
                const metadata = await legacy.stat(identity)
                if (metadata !== null) {
                    if (metadata.kind !== identity.kind || metadata.key !== identity.key) {
                        throw new TypeError(`Asset alias legacy metadata does not match ${key}`)
                    }
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

export function createAssetRepository(options: AssetRepositoryOptions): AssetRepository {
    const repository = createTypedAssetRepositoryReader({
        reader: options.reader,
        cas: options.cas,
        legacy: {
            read: (identity) => options.legacy.read(identity.key),
            stat: (identity) => options.legacy.stat(identity.key),
        },
        legacyFallback: options.legacyFallback,
    })
    return {
        read: (key, range) => repository.read({ kind: 'asset', key }, range),
        stat: (key) => repository.stat({ kind: 'asset', key }),
    }
}

export function createTypedAssetRepository(
    options: TypedAssetRepositoryOptions,
): TypedAssetRepository {
    const repository = createTypedAssetRepositoryReader({
        reader: options.catalog,
        cas: options.cas,
        legacy: options.legacy,
        legacyFallback: options.legacyFallback,
    })
    return {
        ...repository,
        async list(query) {
            const page = await options.catalog.listAssetAliases(query)
            for (const alias of page.items) validateAssetAlias(alias)
            return page
        },
        async remove(identity, expectedRevision) {
            validateAssetAliasIdentity(identity)
            return options.catalog.deleteAssetAlias(identity, expectedRevision)
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
