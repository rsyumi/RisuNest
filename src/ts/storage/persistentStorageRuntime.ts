import { isNodeServer, isTauri } from '../platform'
import { configureLocalColdStorageRuntime } from '../process/coldstorage.svelte'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { createLocalColdStorageRuntime } from './localColdStorageRuntime'
import { getPersistentStorageAuthority } from './persistentDataStoreFactory'
import {
    configureActiveBlobStore,
    createGatedBlobStore,
    getLegacyBlobStore,
    getPlatformBlobKeyValueBackend,
} from './platformBlobStore'
import { migrateLegacyAssetRepository } from './assetRepositoryMigration'
import { migrateLegacyColdPayloads } from './coldPayloadMigration'
import { createCompleteColdPayloadStore } from './coldPayloadRepository'
import {
    createRuntimeColdPayloadDispatcher,
    selectRuntimeColdPayloadStore,
} from './coldPayloadRuntime'
import {
    createNativeV2BlobStore,
    createRuntimeAssetRepositoryDispatcher,
    selectRuntimeAssetRepository,
} from './assetRepositoryRuntime'
import {
    createNativeAssetObjectUrlResolver,
    createNativeDurableAssetWriteSessionFactory,
    createNativeImmutablePayloadCas,
    createNativeNewInlayImageEncoder,
} from './nativeAssetRepository'
import {
    createGatedColdPayloadStore,
    createLegacyBrowserOpfsColdPayloadStore,
    createLegacyNodeColdPayloadStore,
    createLegacyTauriColdPayloadStore,
} from './platformColdPayloadStore'
import { RevisionConflictError } from './persistentDataStore'
import type { BlobStore } from './blobStore'
import type { ColdPayloadStore } from './coldPayloadStore'
import type { PersistentStorageAuthority } from './persistentStorageAuthority'

async function createLocalColdPayloadStore() {
    const backend = await getPlatformBlobKeyValueBackend()
    if (isTauri) return createLegacyTauriColdPayloadStore(backend)
    if (isNodeServer) return createLegacyNodeColdPayloadStore(backend)
    return createLegacyBrowserOpfsColdPayloadStore(() => navigator.storage.getDirectory())
}

function createCoordinatorOwnedColdPayloadStore(
    store: ColdPayloadStore,
    authority: PersistentStorageAuthority,
): ColdPayloadStore {
    const mutate = (operation: () => Promise<void>) =>
        getPersistentDataRuntime().runStorageOnlyMutation((expectedRevision) =>
            authority.gate.runTransition(async () => {
                const before = await authority.rawStore.readRoot()
                if (before.revision !== expectedRevision) {
                    throw new RevisionConflictError(expectedRevision, before.revision)
                }
                await operation()
                return (await authority.rawStore.readRoot()).revision
            }))
    return {
        read: (key) => store.read(key),
        async write(key, data) {
            const ownedData = data.slice()
            await mutate(() => store.write(key, ownedData))
        },
        list: () => store.list(),
        remove: (key) => mutate(() => store.remove(key)),
    }
}

function createConfiguredColdPayloadStore(
    store: ColdPayloadStore,
    authority: PersistentStorageAuthority,
): ColdPayloadStore {
    return isTauri
        ? createCoordinatorOwnedColdPayloadStore(store, authority)
        : createGatedColdPayloadStore(store, authority.gate)
}

function createCoordinatorOwnedAssetBlobStore(
    store: BlobStore,
    authority: PersistentStorageAuthority,
): BlobStore {
    const mutate = async <T>(key: string, operation: () => Promise<T>): Promise<T> => {
        let result!: T
        await getPersistentDataRuntime().runStorageOnlyMutation((expectedRevision) =>
            authority.gate.runKeyedWrite(key, async () => {
                const before = await authority.rawStore.readRoot()
                if (before.revision !== expectedRevision) {
                    throw new RevisionConflictError(expectedRevision, before.revision)
                }
                result = await operation()
                return (await authority.rawStore.readRoot()).revision
            }))
        return result
    }
    const coordinated: BlobStore = {
        async put(key, data, metadata) {
            const ownedData = data.slice()
            const ownedMetadata = { ...metadata }
            return mutate(key, () => store.put(key, ownedData, ownedMetadata))
        },
        read: (key, range) => store.read(key, range),
        stat: (key) => store.stat(key),
        list: (query) => store.list(query),
        remove: (key) => mutate(key, () => store.remove(key)),
        resolveUrl: (key) => store.resolveUrl(key),
    }
    if (store.putNewInlayImage) {
        coordinated.putNewInlayImage = (key, data, input) => {
            const ownedData = data.slice()
            const ownedInput = { ...input }
            return mutate(key, () => store.putNewInlayImage!(key, ownedData, ownedInput))
        }
    }
    return coordinated
}

function createConfiguredAssetBlobStore(
    store: BlobStore,
    authority: PersistentStorageAuthority,
): BlobStore {
    return isTauri
        ? createCoordinatorOwnedAssetBlobStore(store, authority)
        : createGatedBlobStore(store, authority.gate)
}

async function installPersistentStorage(): Promise<void> {
    const authority = getPersistentStorageAuthority()
    await authority.rawStore.open()
    const coldLegacy = await createLocalColdPayloadStore()
    const legacy = getLegacyBlobStore()
    const v2 = isTauri
        ? createNativeV2BlobStore({
            store: authority.rawStore,
            legacy,
            cas: createNativeImmutablePayloadCas(),
            objectUrls: createNativeAssetObjectUrlResolver(),
            newInlayImages: createNativeNewInlayImageEncoder(),
            writeSessions: createNativeDurableAssetWriteSessionFactory(),
        })
        : undefined
    const selection = {
        store: authority.rawStore,
        legacy,
        v2,
        v2Capability: isTauri,
    }
    await selectRuntimeAssetRepository(selection)
    const coldV2 = isTauri
        ? createCompleteColdPayloadStore({
            catalog: authority.rawStore,
            cas: createNativeImmutablePayloadCas(),
            legacy: coldLegacy,
        })
        : undefined
    const coldSelection = {
        store: authority.rawStore,
        legacy: coldLegacy,
        v2: coldV2,
        v2Capability: isTauri,
    }
    await selectRuntimeColdPayloadStore(coldSelection)
    configureActiveBlobStore(
        authority.gate,
        createConfiguredAssetBlobStore(
            createRuntimeAssetRepositoryDispatcher(selection),
            authority,
        ),
        { alreadyGuarded: true },
    )
    configureLocalColdStorageRuntime(
        createLocalColdStorageRuntime(createConfiguredColdPayloadStore(
            createRuntimeColdPayloadDispatcher(coldSelection),
            authority,
        )),
    )
}

export async function activateNativeAssetRepository(): Promise<number | null> {
    if (!isTauri) return null
    const authority = getPersistentStorageAuthority()
    return authority.gate.runTransition(async () => {
        const legacy = getLegacyBlobStore()
        const coldLegacy = await createLocalColdPayloadStore()
        const cas = createNativeImmutablePayloadCas()
        const current = await authority.rawStore.readAssetRepositoryAuthority()
        const currentCold = await authority.rawStore.readColdPayloadAuthority()
        if (current.value.format === 'preparing') {
            throw new Error('Active asset repository generation cannot be preparing')
        }
        if (currentCold.value.format === 'preparing') {
            throw new Error('Active cold payload generation cannot be preparing')
        }
        if (current.value.format === 'legacy') {
            if (currentCold.value.format !== 'legacy') {
                throw new Error('Asset migration cannot replace an active cold payload v2 generation')
            }
            await migrateLegacyAssetRepository({
                store: authority.rawStore,
                legacy,
                cas,
            })
        }
        const migratedCold = await authority.rawStore.readColdPayloadAuthority()
        if (migratedCold.value.format === 'legacy') {
            await migrateLegacyColdPayloads({
                store: authority.rawStore,
                legacy: coldLegacy,
                cas,
            })
        }
        const v2 = createNativeV2BlobStore({
            store: authority.rawStore,
            legacy,
            cas,
            objectUrls: createNativeAssetObjectUrlResolver(),
            newInlayImages: createNativeNewInlayImageEncoder(),
            writeSessions: createNativeDurableAssetWriteSessionFactory(),
        })
        const selection = {
            store: authority.rawStore,
            legacy,
            v2,
            v2Capability: true,
        }
        const coldV2 = createCompleteColdPayloadStore({
            catalog: authority.rawStore,
            cas,
            legacy: coldLegacy,
        })
        const coldSelection = {
            store: authority.rawStore,
            legacy: coldLegacy,
            v2: coldV2,
            v2Capability: true,
        }
        await selectRuntimeAssetRepository(selection)
        await selectRuntimeColdPayloadStore(coldSelection)
        configureActiveBlobStore(
            authority.gate,
            createConfiguredAssetBlobStore(
                createRuntimeAssetRepositoryDispatcher(selection),
                authority,
            ),
            { alreadyGuarded: true },
        )
        configureLocalColdStorageRuntime(
            createLocalColdStorageRuntime(createConfiguredColdPayloadStore(
                createRuntimeColdPayloadDispatcher(coldSelection),
                authority,
            )),
        )
        return (await authority.rawStore.readColdPayloadAuthority()).revision
    })
}

let installation: Promise<void> | null = null

export function initializePersistentStorage(): Promise<void> {
    return installation ??= installPersistentStorage().catch((error) => {
        installation = null
        throw error
    })
}
