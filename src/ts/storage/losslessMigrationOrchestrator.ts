import type { BlobMetadata } from './blobStore'
import type { Database } from './database.svelte'
import {
    createLosslessMigrationManifest,
    decodeLosslessMigrationPackage,
    encodeLosslessMigrationPackage,
    hashLosslessMigrationManifest,
    type DecodedLosslessMigrationEntry,
    type LosslessMigrationInputEntry,
    type LosslessMigrationManifestEntry,
} from './losslessMigrationPackage'
import type { MigrationPayloadStageFactory } from './migrationPayloadStage'
import type { RootedBlobStoreFactory } from './platformBlobStore'
import type { RootedColdPayloadStoreFactory } from './coldPayloadStore'
import type {
    ActivePersistentTuple,
    PersistentDataStore,
    PersistentRevisionLease,
    PreparedPersistentReplacement,
} from './persistentDataStore'
import { assertGeneratedStorageRootId, type BlobStorageRoot } from './storageRoot'

export interface MigrationReferenceGraph {
    assets: readonly string[]
    inlays: readonly string[]
    cold: readonly string[]
}

export type MigrationInterruptionPoint =
    | 'after-entry'
    | 'after-seal'
    | 'after-prepare'
    | 'before-activate'
    | 'after-activate'

export interface LosslessMigrationImportResult {
    tuple: ActivePersistentTuple
    manifestHash: string
    unreferenced: string[]
}

export interface LosslessMigrationOrchestrator {
    importPackage(bytes: Uint8Array): Promise<LosslessMigrationImportResult>
    exportPackage(): Promise<Uint8Array>
    recoverInterruptedMigrations(): Promise<void>
}

export interface LosslessMigrationOrchestratorDependencies {
    store: PersistentDataStore
    stages: MigrationPayloadStageFactory
    blobs: RootedBlobStoreFactory
    cold: RootedColdPayloadStoreFactory
    runMigration<T>(reason: string, operation: () => Promise<T>): Promise<T>
    decodeDatabase(bytes: Uint8Array): Promise<Database>
    prepareDatabase(database: Database): Promise<Database>
    encodeDatabase(lease: PersistentRevisionLease): Promise<Uint8Array>
    collectReferences(database: Database, coldValues: ReadonlyMap<string, unknown>): MigrationReferenceGraph
    installActiveTuple(tuple: ActivePersistentTuple): void
    publishDatabase(database: Database, tuple: ActivePersistentTuple): void | Promise<void>
    onInterruption?(point: MigrationInterruptionPoint, entry?: LosslessMigrationManifestEntry): void | Promise<void>
    onCleanupError?(error: unknown): void
}

function payloadRoot(tuple: ActivePersistentTuple): BlobStorageRoot {
    if (tuple.payloadGeneration === 'legacy') return { kind: 'legacy' }
    assertGeneratedStorageRootId(tuple.payloadGeneration)
    return { kind: 'generation', id: tuple.payloadGeneration }
}

function identity(kind: string, id: string): string {
    return `${kind}\0${id}`
}

function migrationMetadata(metadata: BlobMetadata): LosslessMigrationInputEntry['metadata'] {
    if (metadata.kind === 'asset') {
        return { kind: 'asset', mime: metadata.mime, name: metadata.name, ext: metadata.ext }
    }
    return {
        kind: 'inlay',
        mime: metadata.mime,
        name: metadata.name,
        ext: metadata.ext,
        inlayType: metadata.inlayType,
        ...(metadata.width === undefined ? {} : { width: metadata.width }),
        ...(metadata.height === undefined ? {} : { height: metadata.height }),
    }
}

export function createLosslessMigrationOrchestrator(
    dependencies: LosslessMigrationOrchestratorDependencies,
): LosslessMigrationOrchestrator {
    const interrupt = async (point: MigrationInterruptionPoint, entry?: LosslessMigrationManifestEntry) => {
        await dependencies.onInterruption?.(point, entry)
    }

    return {
        async importPackage(bytes) {
            const decoded = await decodeLosslessMigrationPackage(bytes)
            return dependencies.runMigration('lossless-import', async () => {
                const previous = await dependencies.store.readActiveTuple()
                const stage = dependencies.stages.create(previous.payloadGeneration)
                let prepared: PreparedPersistentReplacement | undefined
                let committed = false
                try {
                    let databaseBytes: Uint8Array | undefined
                    const coldValues = new Map<string, unknown>()
                    for await (const entry of decoded.entries()) {
                        if (entry.kind === 'database') {
                            databaseBytes = entry.data
                            continue
                        }
                        await stage.put(entry)
                        if (entry.kind === 'cold') coldValues.set(entry.id, await stage.readColdValue(entry.id))
                        await interrupt('after-entry', entry)
                    }
                    if (!databaseBytes) throw new Error('Migration package is missing database.risudat')
                    const database = await dependencies.prepareDatabase(await dependencies.decodeDatabase(databaseBytes))
                    const manifest = createLosslessMigrationManifest(decoded.manifest.entries)
                    const manifestHash = await hashLosslessMigrationManifest(manifest)
                    const references = dependencies.collectReferences(database, coldValues)
                    const byIdentity = new Map(manifest.entries.map((entry) => [identity(entry.kind, entry.id), entry]))
                    const required = [
                        ...references.assets.map((id) => ['asset', id] as const),
                        ...references.inlays.map((id) => ['inlay', id] as const),
                        ...references.cold.map((id) => ['cold', id] as const),
                    ]
                    for (const [kind, id] of required) {
                        const entry = byIdentity.get(identity(kind, id))
                        if (!entry) throw new Error(`Missing referenced ${kind} migration entry ${id}`)
                        await stage.verifyEntry(entry)
                    }
                    await stage.seal(manifest, manifestHash)
                    await interrupt('after-seal')
                    prepared = await dependencies.store.prepareReplacement(
                        database,
                        manifestHash,
                        stage.payloadGeneration,
                    )
                    await interrupt('after-prepare')
                    const seal = await stage.verifySeal()
                    if (seal.manifestHash !== prepared.manifestHash
                        || seal.payloadGeneration !== prepared.payloadGeneration) {
                        throw new Error('Prepared replacement does not match the sealed payload stage')
                    }
                    await interrupt('before-activate')
                    const activated = await dependencies.store.activatePreparedReplacement({ prepared, manifestHash })
                    committed = true
                    await interrupt('after-activate')
                    const tuple = await dependencies.store.readActiveTuple()
                    if (tuple.revision !== activated.revision
                        || tuple.dataGeneration !== prepared.dataGeneration
                        || tuple.payloadGeneration !== prepared.payloadGeneration) {
                        throw new Error('Committed persistent tuple does not match its prepared replacement')
                    }
                    dependencies.installActiveTuple(tuple)
                    await dependencies.publishDatabase(database, tuple)

                    const requiredIdentities = new Set(required.map(([kind, id]) => identity(kind, id)))
                    const unreferenced = manifest.entries
                        .filter((entry) => entry.kind !== 'database'
                            && !requiredIdentities.has(identity(entry.kind, entry.id)))
                        .map((entry) => `${entry.kind}:${entry.id}`)
                    if (previous.payloadGeneration !== 'legacy') {
                        try {
                            await dependencies.stages.removeGeneration(previous.payloadGeneration, tuple.payloadGeneration)
                        } catch (error) {
                            dependencies.onCleanupError?.(error)
                        }
                    }
                    return { tuple, manifestHash, unreferenced }
                } catch (error) {
                    if (!committed) {
                        let payloadRemoved = false
                        try {
                            const active = await dependencies.store.readActiveTuple()
                            await dependencies.stages.removeGeneration(stage.payloadGeneration, active.payloadGeneration)
                            payloadRemoved = true
                        } catch (cleanupError) {
                            dependencies.onCleanupError?.(cleanupError)
                        }
                        if (payloadRemoved && prepared) {
                            await dependencies.store.discardPreparedReplacement(prepared)
                        }
                    }
                    throw error
                }
            })
        },

        async exportPackage() {
            return dependencies.runMigration('lossless-export', async () => {
                const tuple = await dependencies.store.readActiveTuple()
                const lease = await dependencies.store.acquireRevision(tuple.revision)
                try {
                    const root = payloadRoot(tuple)
                    const blobs = dependencies.blobs.open(root)
                    const cold = dependencies.cold.open(root)
                    const database = await dependencies.encodeDatabase(lease)
                    const entries: LosslessMigrationInputEntry[] = [{
                        kind: 'database', id: 'database.risudat', metadata: {}, data: database,
                    }]
                    for (const metadata of [...await blobs.list()].sort((a, b) => a.key.localeCompare(b.key))) {
                        const data = await blobs.read(metadata.key)
                        if (data === null) throw new Error(`Missing live blob ${metadata.key}`)
                        entries.push({ kind: metadata.kind, id: metadata.key, metadata: migrationMetadata(metadata), data })
                    }
                    for (const id of await cold.list()) {
                        const data = await cold.read(id)
                        if (data === null) throw new Error(`Missing live cold payload ${id}`)
                        entries.push({ kind: 'cold', id, metadata: {}, data })
                    }
                    return await encodeLosslessMigrationPackage(entries)
                } finally {
                    await lease.release()
                }
            })
        },

        async recoverInterruptedMigrations() {
            await dependencies.runMigration('lossless-recovery', async () => {
                const active = await dependencies.store.readActiveTuple()
                const prepared = await dependencies.store.listPreparedReplacements()
                for (const handle of prepared) {
                    if (handle.payloadGeneration === active.payloadGeneration) {
                        dependencies.onCleanupError?.(new Error('Active payload generation has a prepared marker'))
                        continue
                    }
                    try {
                        await dependencies.stages.removeGeneration(handle.payloadGeneration, active.payloadGeneration)
                        await dependencies.store.discardPreparedReplacement(handle)
                    } catch (error) {
                        dependencies.onCleanupError?.(error)
                    }
                }

                const retained = new Set(prepared.map((handle) => handle.payloadGeneration))
                for (const generation of await dependencies.stages.listGenerations()) {
                    if (generation === active.payloadGeneration || retained.has(generation)) continue
                    try {
                        await dependencies.stages.removeGeneration(generation, active.payloadGeneration)
                    } catch (error) {
                        dependencies.onCleanupError?.(error)
                    }
                }

                if (active.payloadGeneration !== 'legacy') {
                    try {
                        const seal = await dependencies.stages.readSeal(active.payloadGeneration)
                        const previous = seal?.previousPayloadGeneration
                        if (previous && previous !== 'legacy' && previous !== active.payloadGeneration) {
                            await dependencies.stages.removeGeneration(previous, active.payloadGeneration)
                        }
                    } catch (error) {
                        dependencies.onCleanupError?.(error)
                    }
                }
            })
        },
    }
}
