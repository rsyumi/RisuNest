import type { BlobStore } from './blobStore'
import {
    createCompleteAssetRepositoryBlobStore,
    type AssetAliasLegacyReader,
    type AssetObjectUrlResolver,
    type NewInlayImageEncoder,
} from './assetRepository'
import { selectAssetRepositoryAuthority } from './assetRepositoryAuthority'
import type { ImmutablePayloadCas } from './payloadCas'
import type { PersistentDataStore } from './persistentDataStore'

function typedLegacyReader(legacy: BlobStore): AssetAliasLegacyReader {
    return {
        read: (identity, range) => legacy.read(identity.key, range),
        stat: (identity) => legacy.stat(identity.key),
        resolveUrl: (identity) => legacy.resolveUrl(identity.key),
    }
}

export function createNativeV2BlobStore(input: {
    store: PersistentDataStore
    legacy: BlobStore
    cas: ImmutablePayloadCas
    objectUrls: AssetObjectUrlResolver
    newInlayImages: NewInlayImageEncoder
}): BlobStore {
    return createCompleteAssetRepositoryBlobStore({
        store: input.store,
        cas: input.cas,
        legacy: typedLegacyReader(input.legacy),
        legacyFallback: true,
        objectUrls: input.objectUrls,
        newInlayImages: input.newInlayImages,
    })
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
}): BlobStore & Required<Pick<BlobStore, 'putNewInlayImage'>> {
    const selected = () => selectRuntimeAssetRepository(input)
    return {
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
