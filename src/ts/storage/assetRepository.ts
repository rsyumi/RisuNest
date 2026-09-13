import {
    inferBlobMime,
    validateBlobReadRange,
    type BlobReadRange,
    type BlobMetadata,
    type BlobStore,
    type BlobWriteMetadata,
    type InlayEncodeOptions,
    type InlayBlobMetadata,
} from './blobStore'
import {
    hashPayloadBytes,
    type ImmutablePayloadCas,
    type PreparedImmutablePayload,
} from './payloadCas'
import {
    RevisionConflictError,
    validateAssetAlias,
    validateAssetAliasIdentity,
} from './persistentDataStore'
import type {
    AssetAlias,
    AssetAliasIdentity,
    AssetAliasKind,
    AssetAliasListQuery,
    AssetAliasPage,
    DataRevision,
    PersistentDataStore,
    Versioned,
} from './persistentDataStore'

export type AssetAliasPayloadSource = 'cas' | 'remote' | 'legacy' | 'missing'

/** Remote availability is distinct from physical CAS existence. */
export interface RemoteAssetReader {
    statObject(hash: string): Promise<number | null>
    readObject(hash: string, range?: BlobReadRange): Promise<Uint8Array | null>
}

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

export interface AssetAliasCatalog {
    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null>
    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage>
    deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
}

export interface AssetAliasLegacyReader {
    read(identity: AssetAliasIdentity, range?: BlobReadRange): Promise<Uint8Array | null>
    stat(identity: AssetAliasIdentity): Promise<BlobMetadata | null>
    resolveUrl?(identity: AssetAliasIdentity): Promise<string | null>
}

interface TypedAssetRepositoryReader {
    read(
        identity: AssetAliasIdentity,
        range?: BlobReadRange,
    ): Promise<Versioned<AssetAliasRead> | null>
    stat(identity: AssetAliasIdentity): Promise<Versioned<AssetAliasStat> | null>
}

interface TypedAssetRepositoryReaderOptions {
    remote?: RemoteAssetReader
    reader: Pick<AssetAliasCatalog, 'readAssetAlias'>
    cas: ImmutablePayloadCas
    legacy: AssetAliasLegacyReader
    legacyFallback: boolean | 'null-hash-only'
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
            if (alias.objectHash !== null && options.remote) {
                const data = await options.remote.readObject(alias.objectHash, range)
                if (data !== null) {
                    const expectedSize = range
                        ? Math.max(
                              0,
                              Math.min(range.endExclusive, alias.size) -
                                  Math.min(range.start, alias.size),
                          )
                        : alias.size
                    if (
                        data.byteLength !== expectedSize ||
                        (!range && (await hashPayloadBytes(data)) !== alias.objectHash)
                    ) {
                        throw new Error(`Remote asset identity mismatch for ${key}`)
                    }
                    return {
                        revision: versioned.revision,
                        value: { alias, data, source: 'remote' },
                    }
                }
            }
            if (
                options.legacyFallback
                && (alias.objectHash === null || options.legacyFallback === true)
            ) {
                const boundedNullHashRead = alias.objectHash === null ? range : undefined
                const data = boundedNullHashRead
                    ? await legacy.read(identity, boundedNullHashRead)
                    : await legacy.read(identity)
                if (data !== null) {
                    const expectedSize = boundedNullHashRead
                        ? Math.max(
                            0,
                            Math.min(boundedNullHashRead.endExclusive, alias.size)
                            - Math.min(boundedNullHashRead.start, alias.size),
                        )
                        : alias.size
                    if (data.byteLength !== expectedSize) {
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
                        value: {
                            alias,
                            data: boundedNullHashRead ? data : blobRange(data, range),
                            source: 'legacy',
                        },
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
            if (alias.objectHash !== null && options.remote) {
                const objectSize = await options.remote.statObject(alias.objectHash)
                if (objectSize !== null) {
                    if (objectSize !== alias.size)
                        throw new Error(`Remote asset size mismatch for ${key}`)
                    return {
                        revision: versioned.revision,
                        value: { alias, objectSize, source: 'remote' },
                    }
                }
            }
            if (
                options.legacyFallback
                && (alias.objectHash === null || options.legacyFallback === true)
            ) {
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

export interface CompleteAssetAliasStore extends AssetAliasCatalog,
    Pick<PersistentDataStore, 'readRoot' | 'commitAssetAlias'> {}

export interface AssetObjectUrlResolver {
    resolveObjectUrl(input: {
        contentHash: string
        mime: string
        size: number
    }): Promise<string | null>
}

export interface NewInlayImageEncoding {
    data: Uint8Array
    metadata: Omit<InlayBlobMetadata, 'key' | 'size'>
}

export interface NewInlayImageEncoder {
    encodeNewInlayImage(
        key: string,
        data: Uint8Array,
        input: { name: string, options?: InlayEncodeOptions },
    ): Promise<NewInlayImageEncoding>
}

export type DurableAssetWriteReleaseOutcome = 'committed' | 'aborted'
export type DurableAssetWriteObjectRole = 'direct-object' | 'owner-manifest'

export interface DurableAssetWriteSession {
    prepare(
        data: Uint8Array,
        role?: DurableAssetWriteObjectRole,
    ): Promise<PreparedImmutablePayload>
    seal(): Promise<void>
    release(outcome: DurableAssetWriteReleaseOutcome): Promise<void>
}

export interface DurableAssetWriteSessionFactory {
    begin(): Promise<DurableAssetWriteSession>
}

export interface CompleteAssetRepositoryBlobStoreOptions {
    remote?: RemoteAssetReader
    store: CompleteAssetAliasStore
    cas: ImmutablePayloadCas
    legacy: AssetAliasLegacyReader
    legacyFallback: boolean
    objectUrls: AssetObjectUrlResolver
    newInlayImages: NewInlayImageEncoder
    writeSessions?: DurableAssetWriteSessionFactory
    listPageSize?: number
}

export interface PreparedCompleteAssetWrite {
    activate(): Promise<BlobMetadata>
    abort(): Promise<void>
}

export type CompleteAssetRepositoryBlobStore = BlobStore &
    Required<Pick<BlobStore, 'putNewInlayImage'>> & {
        prepareOwnedPut(
            key: string,
            ownedData: Uint8Array,
            metadata: BlobWriteMetadata,
        ): Promise<PreparedCompleteAssetWrite>
        prepareOwnedNewInlayImage(
            key: string,
            ownedData: Uint8Array,
            input: { name: string, options?: InlayEncodeOptions },
        ): Promise<PreparedCompleteAssetWrite>
    }

export interface CompleteTypedAssetRepository {
    prepareOwnedPut(
        identity: AssetAliasIdentity,
        ownedData: Uint8Array,
        metadata: BlobWriteMetadata,
    ): Promise<PreparedCompleteAssetWrite>
    prepareOwnedNewInlayImage(
        identity: AssetAliasIdentity,
        ownedData: Uint8Array,
        input: { name: string, options?: InlayEncodeOptions },
    ): Promise<PreparedCompleteAssetWrite>
    put(
        identity: AssetAliasIdentity,
        data: Uint8Array,
        metadata: Parameters<BlobStore['put']>[2],
    ): Promise<BlobMetadata>
    putNewInlayImage(
        identity: AssetAliasIdentity,
        data: Uint8Array,
        input: { name: string, options?: InlayEncodeOptions },
    ): Promise<InlayBlobMetadata>
    read(identity: AssetAliasIdentity, range?: BlobReadRange): Promise<Uint8Array | null>
    stat(identity: AssetAliasIdentity): Promise<BlobMetadata | null>
    list(query?: Parameters<BlobStore['list']>[0]): Promise<BlobMetadata[]>
    remove(identity: AssetAliasIdentity): Promise<void>
    resolveUrl(identity: AssetAliasIdentity): Promise<string | null>
}

function blobIdentity(key: string): AssetAliasIdentity {
    return {
        kind: key.startsWith('assets/') ? 'asset' : 'inlay',
        key,
    }
}

async function readValidatedAlias(
    store: Pick<AssetAliasCatalog, 'readAssetAlias'>,
    identity: AssetAliasIdentity,
): Promise<Versioned<AssetAlias> | null> {
    const versioned = await store.readAssetAlias(identity)
    if (!versioned) return null
    validateAssetAlias(versioned.value)
    if (
        versioned.value.kind !== identity.kind
        || versioned.value.key !== identity.key
    ) {
        throw new TypeError('Asset alias does not match its requested identity')
    }
    return versioned
}

export function createCompleteTypedAssetRepository(
    options: CompleteAssetRepositoryBlobStoreOptions,
): CompleteTypedAssetRepository {
    const pageSize = options.listPageSize ?? 512
    if (!Number.isSafeInteger(pageSize) || pageSize <= 0) {
        throw new RangeError('Asset alias list page size must be a positive safe integer')
    }
    const repository = createTypedAssetRepositoryReader({
        remote: options.remote,
        reader: options.store,
        cas: options.cas,
        legacy: options.legacy,
        legacyFallback: options.legacyFallback ? 'null-hash-only' : false,
    })

    const preparePublish = async (
        identity: AssetAliasIdentity,
        ownedData: Uint8Array,
        metadata: Parameters<BlobStore['put']>[2],
    ): Promise<PreparedCompleteAssetWrite> => {
        validateAssetAliasIdentity(identity)
        if (metadata.kind !== identity.kind) {
            throw new TypeError('Asset alias metadata kind does not match its identity')
        }
        const pendingAlias = {
            ...metadata,
            key: identity.key,
            objectHash: null,
            size: ownedData.byteLength,
        } as AssetAlias
        validateAssetAlias(pendingAlias)
        const session = await options.writeSessions?.begin()
        try {
            const prepared = session
                ? await session.prepare(ownedData)
                : await options.cas.prepare(ownedData)
            if (prepared.byteSize !== ownedData.byteLength) {
                throw new Error(`Prepared payload size mismatch for ${identity.key}`)
            }
            const alias = { ...pendingAlias, objectHash: prepared.contentHash } as AssetAlias
            validateAssetAlias(alias)
            let state: 'prepared' | 'activating' | 'activated' | 'aborted' = 'prepared'
            return {
                async activate() {
                    if (state !== 'prepared') {
                        throw new Error(`Prepared asset write is already ${state}`)
                    }
                    state = 'activating'
                    await session?.seal()
                    for (;;) {
                        const { revision } = await options.store.readRoot()
                        try {
                            await options.store.commitAssetAlias(alias, revision)
                            break
                        } catch (error) {
                            if (!(error instanceof RevisionConflictError)) throw error
                        }
                    }
                    await session?.release('committed')
                    state = 'activated'
                    return aliasBlobMetadata(alias)
                },
                async abort() {
                    if (state !== 'prepared') {
                        throw new Error(`Prepared asset write is already ${state}`)
                    }
                    if (session) await session.release('aborted')
                    state = 'aborted'
                },
            }
        } catch (error) {
            if (session) {
                try {
                    await session.release('aborted')
                } catch (releaseError) {
                    throw new AggregateError(
                        [error, releaseError],
                        `Asset write and durable CAS session cleanup failed for ${identity.key}`,
                    )
                }
            }
            throw error
        }
    }

    const publish = async (
        identity: AssetAliasIdentity,
        data: Uint8Array,
        metadata: Parameters<BlobStore['put']>[2],
    ): Promise<BlobMetadata> => {
        const prepared = await preparePublish(identity, data.slice(), metadata)
        return prepared.activate()
    }

    const prepareNewInlayImage = async (
        identity: AssetAliasIdentity,
        ownedData: Uint8Array,
        input: { name: string },
    ): Promise<PreparedCompleteAssetWrite> => {
        validateAssetAliasIdentity(identity)
        if (identity.kind !== 'inlay') {
            throw new TypeError('New Inlay image requires an Inlay identity')
        }
        const encoded = await options.newInlayImages.encodeNewInlayImage(
            identity.key,
            ownedData,
            { ...input },
        )
        if (!(encoded.data instanceof Uint8Array)) {
            throw new TypeError('New Inlay image encoder must return Uint8Array bytes')
        }
        if (encoded.metadata.kind !== 'inlay') {
            throw new TypeError('New Inlay image encoder must return Inlay metadata')
        }
        return preparePublish(identity, encoded.data, encoded.metadata)
    }

    return {
        prepareOwnedPut: preparePublish,
        prepareOwnedNewInlayImage: prepareNewInlayImage,
        put: publish,
        async putNewInlayImage(identity, data, input) {
            const prepared = await prepareNewInlayImage(identity, data.slice(), input)
            return await prepared.activate() as InlayBlobMetadata
        },
        async read(identity, range) {
            return (await repository.read(identity, range))?.value.data ?? null
        },
        async stat(identity) {
            const result = await repository.stat(identity)
            if (!result || result.value.source === 'missing') return null
            return aliasBlobMetadata(result.value.alias)
        },
        async list(query = {}) {
            const output: BlobMetadata[] = []
            const seenCursors = new Set<string>()
            let cursor: string | undefined
            let revision: DataRevision | undefined
            do {
                const page = await options.store.listAssetAliases({
                    ...(query.kind === undefined ? {} : { kind: query.kind }),
                    limit: pageSize,
                    ...(cursor === undefined ? {} : { cursor }),
                })
                if (revision !== undefined && revision !== page.revision) {
                    throw new Error('Asset alias revision changed while listing')
                }
                revision = page.revision
                for (const alias of page.items) {
                    validateAssetAlias(alias)
                    if (query.kind !== undefined && alias.kind !== query.kind) {
                        throw new TypeError('Asset alias list returned the wrong kind')
                    }
                    output.push(aliasBlobMetadata(alias))
                }
                cursor = page.nextCursor
                if (cursor !== undefined && seenCursors.has(cursor)) {
                    throw new Error('Asset alias list cursor did not advance')
                }
                if (cursor !== undefined) seenCursors.add(cursor)
            } while (cursor !== undefined)
            return output
        },
        async remove(identity) {
            validateAssetAliasIdentity(identity)
            for (;;) {
                const versioned = await readValidatedAlias(options.store, identity)
                if (!versioned) return
                try {
                    await options.store.deleteAssetAlias(identity, versioned.revision)
                    return
                } catch (error) {
                    if (!(error instanceof RevisionConflictError)) throw error
                }
            }
        },
        async resolveUrl(identity) {
            validateAssetAliasIdentity(identity)
            const versioned = await readValidatedAlias(options.store, identity)
            if (!versioned) return null
            const alias = versioned.value
            if (alias.objectHash === null) {
                if (!options.legacyFallback) return null
                return await options.legacy.resolveUrl?.(identity) ?? null
            }
            const size = await options.cas.statObject(alias.objectHash)
                ?? await options.remote?.statObject(alias.objectHash) ?? null
            if (size === null) return null
            if (size !== alias.size) {
                throw new Error(`Asset alias size mismatch for ${identity.key}`)
            }
            return options.objectUrls.resolveObjectUrl({
                contentHash: alias.objectHash,
                mime: inferBlobMime(alias.mime, alias.ext),
                size: alias.size,
            })
        },
    }
}

function requireNamespacedBlobIdentity(
    key: string,
    kind?: AssetAliasKind,
): AssetAliasIdentity {
    const identity = blobIdentity(key)
    if (kind !== undefined && identity.kind !== kind) {
        throw new TypeError('BlobStore key namespace does not match its metadata kind')
    }
    return identity
}

export function createCompleteAssetRepositoryBlobStore(
    options: CompleteAssetRepositoryBlobStoreOptions,
): CompleteAssetRepositoryBlobStore {
    const repository = createCompleteTypedAssetRepository(options)
    return {
        prepareOwnedPut(key, ownedData, metadata) {
            return repository.prepareOwnedPut(
                requireNamespacedBlobIdentity(key, metadata.kind),
                ownedData,
                metadata,
            )
        },
        prepareOwnedNewInlayImage(key, ownedData, input) {
            return repository.prepareOwnedNewInlayImage(
                requireNamespacedBlobIdentity(key, 'inlay'),
                ownedData,
                input,
            )
        },
        async put(key, data, metadata) {
            return await repository.put(
                requireNamespacedBlobIdentity(key, metadata.kind),
                data,
                metadata,
            )
        },
        async putNewInlayImage(key, data, input) {
            return await repository.putNewInlayImage(
                requireNamespacedBlobIdentity(key, 'inlay'),
                data,
                input,
            )
        },
        read: (key, range) => repository.read(requireNamespacedBlobIdentity(key), range),
        stat: (key) => repository.stat(requireNamespacedBlobIdentity(key)),
        async list(query) {
            const items = await repository.list(query)
            for (const item of items) requireNamespacedBlobIdentity(item.key, item.kind)
            return items
        },
        remove: (key) => repository.remove(requireNamespacedBlobIdentity(key)),
        resolveUrl: (key) => repository.resolveUrl(requireNamespacedBlobIdentity(key)),
    }
}
