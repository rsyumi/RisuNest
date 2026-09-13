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
    createCoordinatorOwnedAssetBlobStore,
    createRuntimeAssetRepositoryDispatcher,
    selectRuntimeAssetRepository,
    type RuntimeAssetRepositoryDispatcher,
} from './assetRepositoryRuntime'
import {
    createNativeAssetObjectUrlResolver,
    createNativeRemoteAssetReader,
    createNativeDurableAssetWriteSessionFactory,
    createNativeDurableCasJobSessionFactory,
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
import type { ImmutablePayloadCas } from './payloadCas'
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

function createConfiguredAssetBlobStore(
    store: RuntimeAssetRepositoryDispatcher,
    authority: PersistentStorageAuthority,
): BlobStore {
    return isTauri
        ? createCoordinatorOwnedAssetBlobStore(
            store,
            authority,
            getPersistentDataRuntime(),
        )
        : createGatedBlobStore(store, authority.gate)
}

async function installRepositorySelections(
    authority: PersistentStorageAuthority,
    input: {
        legacy: BlobStore
        coldLegacy: ColdPayloadStore
        v2Capability: boolean
        createAssetCas(): ImmutablePayloadCas
        createColdCas(): ImmutablePayloadCas
    },
): Promise<void> {
    const v2 = input.v2Capability
        ? createNativeV2BlobStore({
            store: authority.rawStore,
            legacy: input.legacy,
            cas: input.createAssetCas(),
            objectUrls: createNativeAssetObjectUrlResolver(),
            remote: createNativeRemoteAssetReader(),
            newInlayImages: createNativeNewInlayImageEncoder(),
            writeSessions: createNativeDurableAssetWriteSessionFactory(),
        })
        : undefined
    const selection = {
        store: authority.rawStore,
        legacy: input.legacy,
        v2,
        v2Capability: input.v2Capability,
    }
    const coldV2 = input.v2Capability
        ? createCompleteColdPayloadStore({
            catalog: authority.rawStore,
            cas: input.createColdCas(),
            legacy: input.coldLegacy,
            writeSessions: createNativeDurableCasJobSessionFactory('cold-direct-write'),
        })
        : undefined
    const coldSelection = {
        store: authority.rawStore,
        legacy: input.coldLegacy,
        v2: coldV2,
        v2Capability: input.v2Capability,
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
}

async function installPersistentStorage(): Promise<void> {
    const authority = getPersistentStorageAuthority()
    await authority.rawStore.open()
    const coldLegacy = await createLocalColdPayloadStore()
    const legacy = getLegacyBlobStore()
    await installRepositorySelections(authority, {
        legacy,
        coldLegacy,
        v2Capability: isTauri,
        createAssetCas: () => createNativeImmutablePayloadCas(),
        createColdCas: () => createNativeImmutablePayloadCas(),
    })
}

export async function activateNativeAssetRepository(): Promise<number | null> {
    if (!isTauri) return null
    const authority = getPersistentStorageAuthority()
    return authority.gate.runTransition(async () => {
        const legacy = getLegacyBlobStore()
        const coldLegacy = await createLocalColdPayloadStore()
        const cas = createNativeImmutablePayloadCas()
        const assetMigrationSessions = createNativeDurableCasJobSessionFactory(
            'direct-asset-or-inlay-write',
        )
        const coldMigrationSessions = createNativeDurableCasJobSessionFactory('cold-migration')
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
                writeSessions: assetMigrationSessions,
            })
        }
        const migratedCold = await authority.rawStore.readColdPayloadAuthority()
        if (migratedCold.value.format === 'legacy') {
            await migrateLegacyColdPayloads({
                store: authority.rawStore,
                legacy: coldLegacy,
                cas,
                writeSessions: coldMigrationSessions,
            })
        }
        await installRepositorySelections(authority, {
            legacy,
            coldLegacy,
            v2Capability: true,
            createAssetCas: () => cas,
            createColdCas: () => cas,
        })
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
