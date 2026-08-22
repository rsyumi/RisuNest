import { isNodeServer, isTauri } from '../platform'
import { configureLocalColdStorageRuntime } from '../process/coldstorage.svelte'
import { createLocalColdStorageRuntime } from './localColdStorageRuntime'
import { getPersistentStorageAuthority } from './persistentDataStoreFactory'
import {
    configureActiveBlobStore,
    getPlatformBlobKeyValueBackend,
} from './platformBlobStore'
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
    const cold = await createLocalColdPayloadStore()
    configureActiveBlobStore(authority.gate)
    configureLocalColdStorageRuntime(
        createLocalColdStorageRuntime(createGatedColdPayloadStore(cold, authority.gate)),
    )
    await authority.rawStore.open()
}

let installation: Promise<void> | null = null

export function initializePersistentStorage(): Promise<void> {
    return installation ??= installPersistentStorage().catch((error) => {
        installation = null
        throw error
    })
}
