import type { BlobWriteMetadata } from './blobStore'
import type { Database } from './database.svelte'
import {
    createLosslessMigrationOrchestrator,
    type LosslessInPlaceMigrationResult,
    type LosslessMigrationImportResult,
    type MigrationInterruptionPoint,
    type MigrationReferenceGraph,
} from './losslessMigrationOrchestrator'
import type { LosslessMigrationManifestEntry } from './losslessMigrationPackage'
import type { MigrationPayloadStageFactory } from './migrationPayloadStage'
import type { ActivePayloadRoot } from './activePayloadRoot'
import type { PersistentDataStore, PersistentRevisionLease, ActivePersistentTuple } from './persistentDataStore'
import type { RootedBlobStoreFactory } from './platformBlobStore'
import type { RootedColdPayloadStoreFactory } from './coldPayloadStore'
import type { StorageMutationGate } from './storageMutationGate'

export interface ApplicationMigrationRuntime {
    initializeAuthority(): Promise<ActivePersistentTuple>
    refreshActiveTuple(): Promise<ActivePersistentTuple>
    importPackage(bytes: Uint8Array): Promise<LosslessMigrationImportResult>
    exportPackage(): Promise<Uint8Array>
    migrateLegacyInPlace(): Promise<LosslessInPlaceMigrationResult>
}

export interface ApplicationMigrationRuntimeDependencies {
    store: PersistentDataStore
    stages: MigrationPayloadStageFactory
    blobs: RootedBlobStoreFactory
    cold: RootedColdPayloadStoreFactory
    gate: StorageMutationGate
    activePayloadRoot: ActivePayloadRoot
    runMigration<T>(reason: string, operation: () => Promise<T>): Promise<T>
    adoptActivatedDatabase(database: Database, revision: number): Promise<void>
    decodeDatabase(bytes: Uint8Array): Promise<Database>
    prepareDatabase(database: Database): Promise<Database>
    encodeDatabase(lease: PersistentRevisionLease): Promise<Uint8Array>
    collectReferences(
        database: Database,
        coldValues: ReadonlyMap<string, unknown>,
    ): MigrationReferenceGraph
    listLegacyInlayAssetIds(): Promise<string[]>
    readLegacyInlayPayload(id: string): Promise<{
        data: Uint8Array
        metadata: BlobWriteMetadata
    } | null>
    onInterruption?(
        point: MigrationInterruptionPoint,
        entry?: LosslessMigrationManifestEntry,
    ): void | Promise<void>
    onCleanupError?(error: unknown): void
}

export function createApplicationMigrationRuntime(
    dependencies: ApplicationMigrationRuntimeDependencies,
): ApplicationMigrationRuntime {
    const common = {
        store: dependencies.store,
        stages: dependencies.stages,
        blobs: dependencies.blobs,
        cold: dependencies.cold,
        decodeDatabase: dependencies.decodeDatabase,
        prepareDatabase: dependencies.prepareDatabase,
        encodeDatabase: dependencies.encodeDatabase,
        collectReferences: dependencies.collectReferences,
        listLegacyInlayAssetIds: dependencies.listLegacyInlayAssetIds,
        readLegacyInlayPayload: dependencies.readLegacyInlayPayload,
        onInterruption: dependencies.onInterruption,
        installActiveTuple: (tuple: ActivePersistentTuple) =>
            dependencies.activePayloadRoot.install(tuple),
        publishDatabase: (database: Database, tuple: ActivePersistentTuple) =>
            dependencies.adoptActivatedDatabase(database, tuple.revision),
        onCleanupError: dependencies.onCleanupError,
    }
    const application = createLosslessMigrationOrchestrator({
        ...common,
        runMigration: dependencies.runMigration,
    })
    const recovery = createLosslessMigrationOrchestrator({
        ...common,
        runMigration: (_reason, operation) => dependencies.gate.runMigration(operation),
    })

    return {
        async initializeAuthority() {
            await dependencies.store.open()
            await recovery.recoverInterruptedMigrations()
            return dependencies.activePayloadRoot.refresh()
        },
        refreshActiveTuple: () => dependencies.activePayloadRoot.refresh(),
        importPackage: (bytes) => application.importPackage(bytes),
        exportPackage: () => application.exportPackage(),
        migrateLegacyInPlace: () => application.migrateLegacyInPlace(),
    }
}
