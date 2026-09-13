import type {
    BlobMetadata,
    BlobStore,
    BlobWriteMetadata,
} from './blobStore'
import {
    createCompleteAssetRepositoryBlobStore,
    type AssetAliasLegacyReader,
    type AssetObjectUrlResolver,
    type CompleteAssetRepositoryBlobStore,
    type DurableAssetWriteSessionFactory,
    type NewInlayImageEncoder,
    type PreparedCompleteAssetWrite,
    type RemoteAssetReader,
} from './assetRepository'
import { selectAssetRepositoryAuthority } from './assetRepositoryAuthority'
import type { ImmutablePayloadCas } from './payloadCas'
import { RevisionConflictError, type PersistentDataStore } from './persistentDataStore'
import type { AssetRepositoryAuthorityState } from './persistentDataStore'
import type { PersistentStorageAuthority } from './persistentStorageAuthority'

function typedLegacyReader(legacy: BlobStore): AssetAliasLegacyReader {
    return {
        read: (identity, range) => legacy.read(identity.key, range),
        stat: (identity) => legacy.stat(identity.key),
        resolveUrl: (identity) => legacy.resolveUrl(identity.key),
    }
}

export function createNativeV2BlobStore(input: {
    remote?: RemoteAssetReader
    store: PersistentDataStore
    legacy: BlobStore
    cas: ImmutablePayloadCas
    objectUrls: AssetObjectUrlResolver
    newInlayImages: NewInlayImageEncoder
    writeSessions: DurableAssetWriteSessionFactory
}): CompleteAssetRepositoryBlobStore {
    return createCompleteAssetRepositoryBlobStore({
        remote: input.remote,
        store: input.store,
        cas: input.cas,
        legacy: typedLegacyReader(input.legacy),
        legacyFallback: true,
        objectUrls: input.objectUrls,
        newInlayImages: input.newInlayImages,
        writeSessions: input.writeSessions,
    })
}

export interface StagedRuntimeAssetWrite {
    readonly authority: AssetRepositoryAuthorityState
    readonly prepared?: PreparedCompleteAssetWrite
    readonly activateLegacy?: () => Promise<BlobMetadata>
}

export type RuntimeAssetRepositoryDispatcher = BlobStore &
    Required<Pick<BlobStore, 'putNewInlayImage'>> & {
        stagePut(
            key: string,
            ownedData: Uint8Array,
            metadata: BlobWriteMetadata,
        ): Promise<StagedRuntimeAssetWrite>
        stageNewInlayImage(
            key: string,
            ownedData: Uint8Array,
            input: { name: string },
        ): Promise<StagedRuntimeAssetWrite>
        activateStagedWrite(staged: StagedRuntimeAssetWrite): Promise<BlobMetadata>
        abortStagedWrite(staged: StagedRuntimeAssetWrite): Promise<void>
    }

export interface StorageOnlyMutationCoordinator {
    getStorageAuthorityEpoch(): number
    runStorageOnlyMutation(
        operation: (expectedRevision: number) => Promise<number>,
    ): Promise<void>
}

export function createCoordinatorOwnedAssetBlobStore(
    store: RuntimeAssetRepositoryDispatcher,
    authority: PersistentStorageAuthority,
    runtime: StorageOnlyMutationCoordinator,
): BlobStore {
    const invocationTails = new Map<string, Promise<void>>()
    const reserveInvocation = (key: string) => {
        const previous = invocationTails.get(key) ?? Promise.resolve()
        let releaseOwn!: () => void
        const own = new Promise<void>((resolve) => { releaseOwn = resolve })
        const tail = previous.then(() => own)
        invocationTails.set(key, tail)
        return {
            previous,
            release() {
                releaseOwn()
                void tail.then(() => {
                    if (invocationTails.get(key) === tail) invocationTails.delete(key)
                })
            },
        }
    }
    const mutate = async <T>(
        key: string,
        operation: () => Promise<T>,
        expectedAuthorityEpoch?: number,
    ): Promise<T> => {
        let result!: T
        let operationFailure: { error: unknown } | null = null
        await runtime.runStorageOnlyMutation((expectedRevision) =>
            authority.gate.runKeyedWrite(key, async () => {
                const before = await authority.rawStore.readRoot()
                if (before.revision !== expectedRevision) {
                    throw new RevisionConflictError(expectedRevision, before.revision)
                }
                if (
                    expectedAuthorityEpoch !== undefined
                    && runtime.getStorageAuthorityEpoch() !== expectedAuthorityEpoch
                ) {
                    throw new Error(
                        'Asset storage authority changed before staged write activation',
                    )
                }
                try {
                    result = await operation()
                } catch (error) {
                    operationFailure = { error }
                }
                try {
                    return (await authority.rawStore.readRoot()).revision
                } catch (error) {
                    if (!operationFailure) throw error
                    throw new AggregateError(
                        [operationFailure.error, error],
                        `Asset mutation and revision read failed for ${key}`,
                    )
                }
            }))
        if (operationFailure) throw operationFailure.error
        return result
    }
    const stageInInvocationOrder = async <T>(
        key: string,
        prepare: () => ReturnType<RuntimeAssetRepositoryDispatcher['stagePut']>,
    ): Promise<T> => {
        const invocation = reserveInvocation(key)
        const expectedAuthorityEpoch = runtime.getStorageAuthorityEpoch()
        let staged: StagedRuntimeAssetWrite | undefined
        try {
            staged = await prepare()
            await invocation.previous
            try {
                return await mutate(
                    key,
                    () => store.activateStagedWrite(staged!) as Promise<T>,
                    expectedAuthorityEpoch,
                )
            } catch (error) {
                try {
                    await store.abortStagedWrite(staged)
                } catch (abortError) {
                    throw new AggregateError(
                        [error, abortError],
                        `Staged asset write and cleanup failed for ${key}`,
                    )
                }
                throw error
            }
        } finally {
            invocation.release()
        }
    }
    const mutateInInvocationOrder = async <T>(
        key: string,
        operation: () => Promise<T>,
    ): Promise<T> => {
        const invocation = reserveInvocation(key)
        const expectedAuthorityEpoch = runtime.getStorageAuthorityEpoch()
        try {
            await invocation.previous
            return await mutate(key, operation, expectedAuthorityEpoch)
        } finally {
            invocation.release()
        }
    }
    const coordinated: BlobStore = {
        async put(key, data, metadata) {
            const ownedData = data.slice()
            const ownedMetadata = { ...metadata }
            return stageInInvocationOrder(
                key,
                () => store.stagePut(key, ownedData, ownedMetadata),
            )
        },
        read: (key, range) => store.read(key, range),
        stat: (key) => store.stat(key),
        list: (query) => store.list(query),
        remove: (key) => mutateInInvocationOrder(key, () => store.remove(key)),
        resolveUrl: (key) => store.resolveUrl(key),
    }
    coordinated.putNewInlayImage = (key, data, input) => {
        const ownedData = data.slice()
        const ownedInput = { ...input }
        return stageInInvocationOrder(
            key,
            () => store.stageNewInlayImage(key, ownedData, ownedInput),
        )
    }
    return coordinated
}

function hasPreparationSeam(store: BlobStore): store is CompleteAssetRepositoryBlobStore {
    const candidate = store as Partial<CompleteAssetRepositoryBlobStore>
    return typeof candidate.prepareOwnedPut === 'function'
        && typeof candidate.prepareOwnedNewInlayImage === 'function'
}

function sameAuthority(
    left: AssetRepositoryAuthorityState,
    right: AssetRepositoryAuthorityState,
): boolean {
    if (left.format !== right.format) return false
    if (left.format === 'legacy') return true
    if (right.format === 'legacy') return false
    if (left.format === 'preparing' || right.format === 'preparing') {
        return left.format === 'preparing'
            && right.format === 'preparing'
            && left.migrationId === right.migrationId
            && left.sourceRevision === right.sourceRevision
    }
    return left.migrationId === right.migrationId
        && left.compatibilityHash === right.compatibilityHash
}

export async function selectRuntimeAssetRepository(input: {
    store: PersistentDataStore
    legacy: BlobStore
    v2?: BlobStore
    v2Capability: boolean
}): Promise<BlobStore> {
    const authority = await input.store.readAssetRepositoryAuthority()
    return selectAssetRepositoryAuthority(authority.value, {
        legacy: input.legacy,
        v2: input.v2,
        v2Capability: input.v2Capability,
    })
}

export function createRuntimeAssetRepositoryDispatcher(input: {
    store: PersistentDataStore
    legacy: BlobStore
    v2?: BlobStore
    v2Capability: boolean
}): RuntimeAssetRepositoryDispatcher {
    const selected = () => selectRuntimeAssetRepository(input)
    const activationStarted = new WeakSet<StagedRuntimeAssetWrite>()
    const abortStarted = new WeakSet<StagedRuntimeAssetWrite>()
    const stage = async (
        activateLegacy: (store: BlobStore) => Promise<BlobMetadata>,
        prepareV2: (store: CompleteAssetRepositoryBlobStore) => Promise<PreparedCompleteAssetWrite>,
    ): Promise<StagedRuntimeAssetWrite> => {
        const versioned = await input.store.readAssetRepositoryAuthority()
        const store = selectAssetRepositoryAuthority(versioned.value, input)
        if (store === input.v2 && hasPreparationSeam(store)) {
            return {
                authority: versioned.value,
                prepared: await prepareV2(store),
            }
        }
        return {
            authority: versioned.value,
            activateLegacy: () => activateLegacy(store),
        }
    }
    return {
        stagePut(key, ownedData, metadata) {
            return stage(
                (store) => store.put(key, ownedData, metadata),
                (store) => store.prepareOwnedPut(key, ownedData, metadata),
            )
        },
        stageNewInlayImage(key, ownedData, request) {
            return stage(
                async (store) => {
                    if (!store.putNewInlayImage) {
                        throw new Error('Selected asset repository cannot encode new Inlay images')
                    }
                    return store.putNewInlayImage(key, ownedData, request)
                },
                (store) => store.prepareOwnedNewInlayImage(key, ownedData, request),
            )
        },
        async activateStagedWrite(staged) {
            const current = await input.store.readAssetRepositoryAuthority()
            if (!sameAuthority(staged.authority, current.value)) {
                const authorityError = new Error(
                    'Asset repository authority changed before staged write activation',
                )
                if (staged.prepared) {
                    try {
                        await this.abortStagedWrite(staged)
                    } catch (abortError) {
                        throw new AggregateError(
                            [authorityError, abortError],
                            'Asset repository authority change and staged write cleanup failed',
                        )
                    }
                }
                throw authorityError
            }
            activationStarted.add(staged)
            if (staged.prepared) return staged.prepared.activate()
            if (staged.activateLegacy) return staged.activateLegacy()
            throw new Error('Staged asset write has no activation operation')
        },
        async abortStagedWrite(staged) {
            if (activationStarted.has(staged) || abortStarted.has(staged)) return
            abortStarted.add(staged)
            await staged.prepared?.abort()
        },
        async put(key, data, metadata) {
            return (await selected()).put(key, data, metadata)
        },
        async putNewInlayImage(key, data, request) {
            const store = await selected()
            if (!store.putNewInlayImage) {
                throw new Error('Selected asset repository cannot encode new Inlay images')
            }
            return store.putNewInlayImage(key, data, request)
        },
        async read(key, range) {
            return (await selected()).read(key, range)
        },
        async stat(key) {
            return (await selected()).stat(key)
        },
        async list(query) {
            return (await selected()).list(query)
        },
        async remove(key) {
            return (await selected()).remove(key)
        },
        async resolveUrl(key) {
            return (await selected()).resolveUrl(key)
        },
    }
}
