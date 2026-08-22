import { isNodeServer, isTauri } from '../platform'
import { configureLocalColdStorageRuntime } from '../process/coldstorage.svelte'
import { listLegacyInlayAssetIds, readLegacyInlayPayload } from '../process/files/inlays'
import { createApplicationMigrationRuntime, type ApplicationMigrationRuntime } from './applicationMigrationRuntime'
import { prepareDatabaseForPersistence } from './databasePreparation'
import { createLocalColdStorageRuntime } from './localColdStorageRuntime'
import { collectMigrationReferenceGraph } from './migrationReferenceGraph'
import { createMigrationPayloadStageFactory } from './migrationPayloadStage'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { getPersistentStorageAuthority } from './persistentDataStoreFactory'
import { configurePersistentSavePayloadRefresh } from './persistentSaveNotifications'
import {
    configureActiveBlobStore,
    getPlatformBlobKeyValueBackend,
    getRootedBlobStoreFactory,
} from './platformBlobStore'
import {
    createGatedResolvingColdPayloadStore,
    createLegacyBrowserOpfsColdPayloadStore,
    createLegacyNodeColdPayloadStore,
    createLegacyTauriColdPayloadStore,
    createPlatformRootedColdPayloadStoreFactory,
} from './platformColdPayloadStore'
import { decodeRisuSave } from './risuSave'
import { streamRisuSaveFromLease } from './risuSaveStoreAdapter'

async function concatenate(chunks: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
    const values: Uint8Array[] = []
    let size = 0
    for await (const chunk of chunks) {
        values.push(chunk)
        size += chunk.byteLength
    }
    const result = new Uint8Array(size)
    let offset = 0
    for (const value of values) {
        result.set(value, offset)
        offset += value.byteLength
    }
    return result
}

async function createProductionMigrationRuntime(): Promise<ApplicationMigrationRuntime> {
    const authority = getPersistentStorageAuthority()
    const backend = await getPlatformBlobKeyValueBackend()
    const blobs = getRootedBlobStoreFactory()
    const legacyCold = isTauri
        ? createLegacyTauriColdPayloadStore(backend)
        : isNodeServer
            ? createLegacyNodeColdPayloadStore(backend)
            : createLegacyBrowserOpfsColdPayloadStore(await navigator.storage.getDirectory())
    const cold = await createPlatformRootedColdPayloadStoreFactory(
        legacyCold,
        async () => backend,
    )
    const activeCold = createGatedResolvingColdPayloadStore(
        cold,
        authority.activePayloadRoot,
        authority.gate,
    )
    configureActiveBlobStore(authority.activePayloadRoot, authority.gate)
    configureLocalColdStorageRuntime(createLocalColdStorageRuntime(activeCold))
    configurePersistentSavePayloadRefresh(() => authority.activePayloadRoot.refresh())

    const runtime = getPersistentDataRuntime()
    return createApplicationMigrationRuntime({
        store: authority.rawStore,
        stages: createMigrationPayloadStageFactory({ backend, blobs, cold }),
        blobs,
        cold,
        gate: authority.gate,
        activePayloadRoot: authority.activePayloadRoot,
        runMigration: (reason, operation) => runtime.runMigration(reason, operation),
        adoptActivatedDatabase: (database, revision) =>
            runtime.adoptActivatedDatabase(database, revision),
        decodeDatabase: async (bytes) => await decodeRisuSave(bytes),
        prepareDatabase: prepareDatabaseForPersistence,
        encodeDatabase: (lease) => concatenate(streamRisuSaveFromLease(lease)),
        collectReferences: collectMigrationReferenceGraph,
        listLegacyInlayAssetIds,
        readLegacyInlayPayload,
        onCleanupError: (error) => console.error(error),
    })
}

let productionRuntime: Promise<ApplicationMigrationRuntime> | null = null

function getProductionMigrationRuntime(): Promise<ApplicationMigrationRuntime> {
    return productionRuntime ??= createProductionMigrationRuntime()
}

export async function initializePersistentMigrationAuthority(): Promise<void> {
    await (await getProductionMigrationRuntime()).initializeAuthority()
}

export async function refreshActivePayloadRoot(): Promise<void> {
    await (await getProductionMigrationRuntime()).refreshActiveTuple()
}

export async function importLosslessMigrationPackage(bytes: Uint8Array) {
    return (await getProductionMigrationRuntime()).importPackage(bytes)
}

export async function exportLosslessMigrationPackage(): Promise<Uint8Array> {
    return (await getProductionMigrationRuntime()).exportPackage()
}

export async function migrateLegacyPersistentDataInPlace() {
    return (await getProductionMigrationRuntime()).migrateLegacyInPlace()
}
