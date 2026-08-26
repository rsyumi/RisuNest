import { isNodeServer, isTauri } from '../platform'
import { configureLocalColdStorageRuntime } from '../process/coldstorage.svelte'
import { createLocalColdStorageRuntime } from './localColdStorageRuntime'
import { getPersistentStorageAuthority } from './persistentDataStoreFactory'
import {
    configureActiveBlobStore,
    getLegacyBlobStore,
    getPlatformBlobKeyValueBackend,
} from './platformBlobStore'
import { migrateLegacyAssetRepository } from './assetRepositoryMigration'
import {
    createNativeV2BlobStore,
    createRuntimeAssetRepositoryDispatcher,
    selectRuntimeAssetRepository,
} from './assetRepositoryRuntime'
import {
    createNativeAssetObjectUrlResolver,
    createNativeImmutablePayloadCas,
    createNativeNewInlayImageEncoder,
} from './nativeAssetRepository'
import {
    createGatedColdPayloadStore,
    createLegacyBrowserOpfsColdPayloadStore,
    createLegacyNodeColdPayloadStore,
    createLegacyTauriColdPayloadStore,
} from './platformColdPayloadStore'

async function createLocalColdPayloadStore() {
    const backend = await getPlatformBlobKeyValueBackend()
    if (isTauri) return createLegacyTauriColdPayloadStore(backend)
    if (isNodeServer) return createLegacyNodeColdPayloadStore(backend)
    return createLegacyBrowserOpfsColdPayloadStore(() => navigator.storage.getDirectory())
}

async function installPersistentStorage(): Promise<void> {
    const authority = getPersistentStorageAuthority()
    await authority.rawStore.open()
    const cold = await createLocalColdPayloadStore()
    const legacy = getLegacyBlobStore()
    const v2 = isTauri
        ? createNativeV2BlobStore({
            store: authority.rawStore,
            legacy,
            cas: createNativeImmutablePayloadCas(),
            objectUrls: createNativeAssetObjectUrlResolver(),
            newInlayImages: createNativeNewInlayImageEncoder(),
        })
        : undefined
    const selection = {
        store: authority.rawStore,
        legacy,
        v2,
        v2Capability: isTauri,
    }
    await selectRuntimeAssetRepository(selection)
    configureActiveBlobStore(
        authority.gate,
        createRuntimeAssetRepositoryDispatcher(selection),
    )
    configureLocalColdStorageRuntime(
        createLocalColdStorageRuntime(createGatedColdPayloadStore(cold, authority.gate)),
    )
}

export async function activateNativeAssetRepository(): Promise<number | null> {
    if (!isTauri) return null
    const authority = getPersistentStorageAuthority()
    return authority.gate.runTransition(async () => {
        const legacy = getLegacyBlobStore()
        const cas = createNativeImmutablePayloadCas()
        const current = await authority.rawStore.readAssetRepositoryAuthority()
        if (current.value.format === 'preparing') {
            throw new Error('Active asset repository generation cannot be preparing')
        }
        if (current.value.format === 'legacy') {
            await migrateLegacyAssetRepository({
                store: authority.rawStore,
                legacy,
                cas,
            })
        }
        const v2 = createNativeV2BlobStore({
            store: authority.rawStore,
            legacy,
            cas,
            objectUrls: createNativeAssetObjectUrlResolver(),
            newInlayImages: createNativeNewInlayImageEncoder(),
        })
        const selection = {
            store: authority.rawStore,
            legacy,
            v2,
            v2Capability: true,
        }
        await selectRuntimeAssetRepository(selection)
        configureActiveBlobStore(
            authority.gate,
            createRuntimeAssetRepositoryDispatcher(selection),
        )
        return (await authority.rawStore.readAssetRepositoryAuthority()).revision
    })
}

let installation: Promise<void> | null = null

export function initializePersistentStorage(): Promise<void> {
    return installation ??= installPersistentStorage().catch((error) => {
        installation = null
        throw error
    })
}
