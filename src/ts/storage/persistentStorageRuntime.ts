import { isTauri } from '../platform'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { getPersistentStorageAuthority } from './persistentDataStoreFactory'
import {
    configureActiveBlobStore,
    createGatedBlobStore,
    getLegacyBlobStore,
} from './platformBlobStore'
import { migrateLegacyAssetRepository } from './assetRepositoryMigration'
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
import type { BlobStore } from './blobStore'
import type { ImmutablePayloadCas } from './payloadCas'
import type { PersistentStorageAuthority } from './persistentStorageAuthority'

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
        v2Capability: boolean
        createAssetCas(): ImmutablePayloadCas
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
    await selectRuntimeAssetRepository(selection)
    configureActiveBlobStore(
        authority.gate,
        createConfiguredAssetBlobStore(
            createRuntimeAssetRepositoryDispatcher(selection),
            authority,
        ),
        { alreadyGuarded: true },
    )
}

async function installPersistentStorage(): Promise<void> {
    const authority = getPersistentStorageAuthority()
    await authority.rawStore.open()
    const legacy = getLegacyBlobStore()
    await installRepositorySelections(authority, {
        legacy,
        v2Capability: isTauri,
        createAssetCas: () => createNativeImmutablePayloadCas(),
    })
}

export async function activateNativeAssetRepository(): Promise<number | null> {
    if (!isTauri) return null
    const authority = getPersistentStorageAuthority()
    return authority.gate.runTransition(async () => {
        const legacy = getLegacyBlobStore()
        const cas = createNativeImmutablePayloadCas()
        const assetMigrationSessions = createNativeDurableCasJobSessionFactory(
            'direct-asset-or-inlay-write',
        )
        const current = await authority.rawStore.readAssetRepositoryAuthority()
        if (current.value.format === 'preparing') {
            throw new Error('Active asset repository generation cannot be preparing')
        }
        if (current.value.format === 'legacy') {
            await migrateLegacyAssetRepository({
                store: authority.rawStore,
                legacy,
                cas,
                writeSessions: assetMigrationSessions,
            })
        }
        await installRepositorySelections(authority, {
            legacy,
            v2Capability: true,
            createAssetCas: () => cas,
        })
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
