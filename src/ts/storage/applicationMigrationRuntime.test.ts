import 'fake-indexeddb/auto'
import { IDBObjectStore } from 'fake-indexeddb'
import { compressSync } from 'fflate'
import { describe, expect, it, vi } from 'vitest'
import type { BlobWriteMetadata } from './blobStore'
import { createApplicationMigrationRuntime } from './applicationMigrationRuntime'
import { ActivePayloadRoot } from './activePayloadRoot'
import { createKeyValueRootedBlobStoreFactory } from './platformBlobStore'
import { createLegacyNodeColdPayloadStore, createRootedColdPayloadStoreFactory } from './platformColdPayloadStore'
import { createMigrationPayloadStageFactory } from './migrationPayloadStage'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { createStorageMutationGate } from './storageMutationGate'
import { bootstrapPersistentDatabase } from './persistentBootstrap'
import {
    encodeLosslessMigrationPackage,
    type LosslessMigrationInputEntry,
    type LosslessMigrationManifestEntry,
} from './losslessMigrationPackage'
import type { MigrationInterruptionPoint } from './losslessMigrationOrchestrator'
import { makeLosslessMigrationFixture, migrationFixtureReferences } from './tests/losslessMigrationFixtures'
import type { Database } from './database.svelte'

interface MemoryBackendState {
    values: Map<string, Uint8Array>
    removeFailures: number
}

function memoryBackend(state: MemoryBackendState = {
    values: new Map<string, Uint8Array>(),
    removeFailures: 0,
}) {
    return {
        values: state.values,
        write: async (key: string, value: Uint8Array) => void state.values.set(key, value.slice()),
        read: async (key: string) => state.values.get(key)?.slice() ?? null,
        keys: async () => [...state.values.keys()],
        remove: async (key: string) => {
            if (state.removeFailures > 0) {
                state.removeFailures--
                throw new Error('simulated process loss during cleanup')
            }
            state.values.delete(key)
        },
    }
}

const encoder = new TextEncoder()

function makeLegacyPayloadFixture(): LosslessMigrationInputEntry[] {
    let blobIndex = 0
    return makeLosslessMigrationFixture().flatMap((entry) => {
        if (entry.kind === 'database') return []
        if (entry.kind === 'cold') {
            return [{
                ...entry,
                data: entry.id === 'character-cold'
                    ? compressSync(encoder.encode(JSON.stringify({ character: { chaId: 'legacy-character' } })))
                    : compressSync(encoder.encode(JSON.stringify([{ role: 'user', data: 'legacy message' }]))),
            }]
        }
        blobIndex++
        return [{ ...entry, data: new Uint8Array([90 + blobIndex, 190 + blobIndex]) }]
    })
}

async function seedPayload(
    entries: readonly LosslessMigrationInputEntry[],
    blobs: ReturnType<typeof createKeyValueRootedBlobStoreFactory>,
    cold: ReturnType<typeof createRootedColdPayloadStoreFactory>,
): Promise<void> {
    const blobStore = blobs.open({ kind: 'legacy' })
    const coldStore = cold.open({ kind: 'legacy' })
    for (const entry of entries) {
        if (entry.kind === 'asset' || entry.kind === 'inlay') {
            await blobStore.put(entry.id, entry.data, entry.metadata as BlobWriteMetadata)
        } else if (entry.kind === 'cold') {
            await coldStore.write(entry.id, entry.data)
        }
    }
}

async function expectExactPayload(
    entries: readonly LosslessMigrationInputEntry[],
    root: { kind: 'legacy' } | { kind: 'generation'; id: string },
    blobs: ReturnType<typeof createKeyValueRootedBlobStoreFactory>,
    cold: ReturnType<typeof createRootedColdPayloadStoreFactory>,
): Promise<void> {
    const expectedBlobs = entries.filter((entry) => entry.kind === 'asset' || entry.kind === 'inlay')
    const expectedCold = entries.filter((entry) => entry.kind === 'cold')
    const blobStore = blobs.open(root)
    const coldStore = cold.open(root)

    expect((await blobStore.list()).map((entry) => entry.key).sort()).toEqual(
        expectedBlobs.map((entry) => entry.id).sort(),
    )
    for (const entry of expectedBlobs) {
        expect(await blobStore.stat(entry.id)).toEqual({
            ...entry.metadata,
            key: entry.id,
            size: entry.data.byteLength,
        })
        expect(await blobStore.read(entry.id)).toEqual(entry.data)
        expect(await blobStore.resolveUrl(entry.id)).toBeNull()
    }
    expect(await coldStore.list()).toEqual(expectedCold.map((entry) => entry.id).sort())
    for (const entry of expectedCold) expect(await coldStore.read(entry.id)).toEqual(entry.data)
}

type InterruptionCase = {
    caseNumber: number
    name: string
    point?: MigrationInterruptionPoint
    entryKind?: LosslessMigrationManifestEntry['kind']
    entryId?: string
    activationAbort?: boolean
    expectedAuthority: 'old' | 'new'
}

const interruptionCases = [
    {
        caseNumber: 1, name: 'first asset', point: 'after-entry', entryKind: 'asset',
        entryId: 'assets/photo.png', expectedAuthority: 'old',
    },
    {
        caseNumber: 2, name: 'final asset', point: 'after-entry', entryKind: 'asset',
        entryId: 'assets/movie.webm', expectedAuthority: 'old',
    },
    {
        caseNumber: 3, name: 'first inlay', point: 'after-entry', entryKind: 'inlay',
        entryId: 'image-inlay', expectedAuthority: 'old',
    },
    {
        caseNumber: 4, name: 'final inlay', point: 'after-entry', entryKind: 'inlay',
        entryId: 'signature-inlay', expectedAuthority: 'old',
    },
    {
        caseNumber: 5, name: 'first cold payload', point: 'after-entry', entryKind: 'cold',
        entryId: 'character-cold', expectedAuthority: 'old',
    },
    {
        caseNumber: 6, name: 'final cold payload', point: 'after-entry', entryKind: 'cold',
        entryId: 'chat-cold', expectedAuthority: 'old',
    },
    {
        caseNumber: 7, name: 'payload read-back verification', point: 'after-verify',
        expectedAuthority: 'old',
    },
    { caseNumber: 8, name: 'seal write', point: 'after-seal', expectedAuthority: 'old' },
    {
        caseNumber: 9, name: 'prepared database creation', point: 'after-prepare',
        expectedAuthority: 'old',
    },
    {
        caseNumber: 10, name: 'pointer transaction', point: 'before-activate',
        expectedAuthority: 'old',
    },
    {
        caseNumber: 11, name: 'activation transaction abort', activationAbort: true,
        expectedAuthority: 'old',
    },
    {
        caseNumber: 12, name: 'committed pointer before publication', point: 'after-activate',
        expectedAuthority: 'new',
    },
] satisfies InterruptionCase[]

describe('application migration runtime', () => {
    it('recovers before installing authority and installs activation before working-copy adoption', async () => {
        const databaseName = `migration-runtime-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(
            databaseName,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase({ characters: [], marker: 'old' } as unknown as Database)
        const backend = memoryBackend()
        const blobs = createKeyValueRootedBlobStoreFactory(backend)
        const cold = createRootedColdPayloadStoreFactory({
            legacy: createLegacyNodeColdPayloadStore(backend),
            generatedBackend: backend,
        })
        const stages = createMigrationPayloadStageFactory({
            backend,
            blobs,
            cold,
            createGenerationId: () => 'production_1',
        })
        const activePayloadRoot = new ActivePayloadRoot(store)
        const gate = createStorageMutationGate({ allowInRealmMigration: true })
        const events: string[] = []
        const adoptActivatedDatabase = vi.fn(async (_database: Database, revision: number) => {
            events.push(`adopt:${activePayloadRoot.current().payloadGeneration}:${revision}`)
        })
        const runMigration = async <T,>(_reason: string, operation: () => Promise<T>) =>
            gate.runMigration(operation)
        const runtime = createApplicationMigrationRuntime({
            store,
            stages,
            blobs,
            cold,
            gate,
            activePayloadRoot,
            runMigration,
            adoptActivatedDatabase,
            decodeDatabase: async (bytes) => JSON.parse(new TextDecoder().decode(bytes)) as Database,
            prepareDatabase: async (database) => database,
            encodeDatabase: async () => new TextEncoder().encode(JSON.stringify({ characters: [] })),
            collectReferences: () => migrationFixtureReferences,
            listLegacyInlayAssetIds: async () => [],
            readLegacyInlayPayload: async () => null,
        })

        const initial = await runtime.initializeAuthority()
        const result = await runtime.importPackage(await encodeLosslessMigrationPackage(
            makeLosslessMigrationFixture(),
        ))
        const exported = await runtime.exportPackage()
        const repeated = await runtime.migrateLegacyInPlace()
        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        expect(initial.payloadGeneration).toBe('legacy')
        expect(result.tuple.payloadGeneration).toBe('production_1')
        expect(events).toEqual(['adopt:production_1:2'])
        expect(adoptActivatedDatabase).toHaveBeenCalledOnce()
        expect(await store.materializeDatabase()).toMatchObject({ marker: 'new' })
        expect(await reopened.readActiveTuple()).toEqual(result.tuple)
        expect(await reopened.materializeDatabase()).toMatchObject({ marker: 'new' })
        expect(exported.byteLength).toBeGreaterThan(0)
        expect(repeated).toEqual({ status: 'already_migrated', tuple: result.tuple })
    })

    it('reopens with the old coherent tuple and discards inactive prepared payload first', async () => {
        const databaseName = `migration-recovery-${crypto.randomUUID()}`
        const original = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await original.open()
        await original.replaceFromDatabase({ characters: [], marker: 'old' } as unknown as Database)
        await original.prepareReplacement(
            { characters: [], marker: 'prepared' } as unknown as Database,
            'manifest',
            'abandoned_1',
        )
        const backend = memoryBackend()
        await backend.write(
            'blobstore/generations/abandoned_1/assets/uncommitted',
            new Uint8Array([1]),
        )
        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const originalDiscard = reopened.discardPreparedReplacement.bind(reopened)
        const events: string[] = []
        vi.spyOn(reopened, 'discardPreparedReplacement').mockImplementation(async (handle) => {
            expect([...backend.values.keys()].some((key) => key.includes('abandoned_1'))).toBe(false)
            events.push('discard')
            return originalDiscard(handle)
        })
        const blobs = createKeyValueRootedBlobStoreFactory(backend)
        const cold = createRootedColdPayloadStoreFactory({
            legacy: createLegacyNodeColdPayloadStore(backend),
            generatedBackend: backend,
        })
        const activePayloadRoot = new ActivePayloadRoot(reopened)
        const gate = createStorageMutationGate({ allowInRealmMigration: true })
        const runtime = createApplicationMigrationRuntime({
            store: reopened,
            stages: createMigrationPayloadStageFactory({ backend, blobs, cold }),
            blobs,
            cold,
            gate,
            activePayloadRoot,
            runMigration: (_reason, operation) => gate.runMigration(operation),
            adoptActivatedDatabase: async () => {},
            decodeDatabase: async () => ({ characters: [] }) as Database,
            prepareDatabase: async (database) => database,
            encodeDatabase: async () => new Uint8Array(),
            collectReferences: () => ({ assets: [], inlays: [], cold: [] }),
            listLegacyInlayAssetIds: async () => [],
            readLegacyInlayPayload: async () => null,
        })

        const tuple = await runtime.initializeAuthority()

        expect(tuple).toMatchObject({ revision: 1, payloadGeneration: 'legacy' })
        expect(activePayloadRoot.current()).toEqual(tuple)
        expect(await reopened.listPreparedReplacements()).toEqual([])
        expect(events).toEqual(['discard'])
    })

    it.each(interruptionCases)(
        'case $caseNumber reopens the complete $expectedAuthority tuple after interruption at $name',
        async (testCase) => {
            const databaseName = `migration-interruption-${testCase.caseNumber}-${crypto.randomUUID()}`
            const databaseBefore = { characters: [], marker: 'old' } as unknown as Database
            const databaseAfter = { characters: [], marker: 'new' } as unknown as Database
            const legacyEntries = makeLegacyPayloadFixture()
            const migrationEntries = makeLosslessMigrationFixture()
            const backendState: MemoryBackendState = {
                values: new Map<string, Uint8Array>(),
                removeFailures: 0,
            }
            const firstBackend = memoryBackend(backendState)
            const firstBlobs = createKeyValueRootedBlobStoreFactory(firstBackend)
            const firstCold = createRootedColdPayloadStoreFactory({
                legacy: createLegacyNodeColdPayloadStore(firstBackend),
                generatedBackend: firstBackend,
            })
            await seedPayload(legacyEntries, firstBlobs, firstCold)

            const firstStore = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
            await firstStore.open()
            await firstStore.replaceFromDatabase(databaseBefore)
            const oldTuple = await firstStore.readActiveTuple()
            expect(oldTuple).toEqual({
                revision: 1,
                dataGeneration: 'revision-1',
                payloadGeneration: 'legacy',
            })
            const firstStages = createMigrationPayloadStageFactory({
                backend: firstBackend,
                blobs: firstBlobs,
                cold: firstCold,
                createGenerationId: () => 'production_1',
            })
            const firstRoot = new ActivePayloadRoot(firstStore)
            const firstGate = createStorageMutationGate({ allowInRealmMigration: true })
            const firstAdoption = vi.fn(async (_database: Database, _revision: number) => undefined)
            const processLoss = new Error(`process loss at ${testCase.name}`)
            const runtime = createApplicationMigrationRuntime({
                store: firstStore,
                stages: firstStages,
                blobs: firstBlobs,
                cold: firstCold,
                gate: firstGate,
                activePayloadRoot: firstRoot,
                runMigration: (_reason, operation) => firstGate.runMigration(operation),
                adoptActivatedDatabase: firstAdoption,
                decodeDatabase: async (bytes) => JSON.parse(new TextDecoder().decode(bytes)) as Database,
                prepareDatabase: async (database) => database,
                encodeDatabase: async () => new Uint8Array(),
                collectReferences: () => migrationFixtureReferences,
                listLegacyInlayAssetIds: async () => [],
                readLegacyInlayPayload: async () => null,
                onInterruption: async (point, entry) => {
                    if (point !== testCase.point) return
                    if (testCase.entryKind !== undefined && entry?.kind !== testCase.entryKind) return
                    if (testCase.entryId !== undefined && entry?.id !== testCase.entryId) return
                    backendState.removeFailures = 1
                    throw processLoss
                },
                onCleanupError: () => undefined,
            })
            await runtime.initializeAuthority()

            let activationAbortSpy: ReturnType<typeof vi.spyOn> | undefined
            if (testCase.activationAbort) {
                const originalPut = IDBObjectStore.prototype.put
                activationAbortSpy = vi
                    .spyOn(IDBObjectStore.prototype, 'put')
                    .mockImplementation(function (this: IDBObjectStore, value: unknown, key?: IDBValidKey) {
                        const request = originalPut.call(this, value, key)
                        if ((value as { key?: string }).key === 'activePayloadGeneration') {
                            backendState.removeFailures = 1
                            this.transaction.abort()
                        }
                        return request
                    })
            }

            try {
                await expect(runtime.importPackage(
                    await encodeLosslessMigrationPackage(migrationEntries),
                )).rejects.toThrow()
            } finally {
                activationAbortSpy?.mockRestore()
            }

            expect(firstAdoption).not.toHaveBeenCalled()
            expect(await firstStages.listGenerations()).toContain('production_1')
            const preparedBeforeReopen = await firstStore.listPreparedReplacements()
            expect(preparedBeforeReopen).toHaveLength(
                testCase.caseNumber >= 9 && testCase.caseNumber <= 11 ? 1 : 0,
            )
            const committedTuple = await firstStore.readActiveTuple()
            expect(committedTuple).toEqual(testCase.expectedAuthority === 'old'
                ? oldTuple
                : {
                    revision: 2,
                    dataGeneration: expect.stringMatching(/^prepared-/),
                    payloadGeneration: 'production_1',
                })

            const reopenedBackend = memoryBackend(backendState)
            const reopenedBlobs = createKeyValueRootedBlobStoreFactory(reopenedBackend)
            const reopenedCold = createRootedColdPayloadStoreFactory({
                legacy: createLegacyNodeColdPayloadStore(reopenedBackend),
                generatedBackend: reopenedBackend,
            })
            const reopenedStages = createMigrationPayloadStageFactory({
                backend: reopenedBackend,
                blobs: reopenedBlobs,
                cold: reopenedCold,
                createGenerationId: () => 'unused_reopen_generation',
            })
            const reopenedStore = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
            const reopenedRoot = new ActivePayloadRoot(reopenedStore)
            const reopenedGate = createStorageMutationGate({ allowInRealmMigration: true })
            const reopenedAdoption = vi.fn(async (_database: Database, _revision: number) => undefined)
            const recoveryEvents: string[] = []
            if (testCase.caseNumber >= 9 && testCase.caseNumber <= 11) {
                const discard = reopenedStore.discardPreparedReplacement.bind(reopenedStore)
                vi.spyOn(reopenedStore, 'discardPreparedReplacement').mockImplementation(async (prepared) => {
                    expect(await reopenedStages.listGenerations()).not.toContain(prepared.payloadGeneration)
                    recoveryEvents.push(`discard:${prepared.payloadGeneration}`)
                    await discard(prepared)
                })
            }
            const reopenedRuntime = createApplicationMigrationRuntime({
                store: reopenedStore,
                stages: reopenedStages,
                blobs: reopenedBlobs,
                cold: reopenedCold,
                gate: reopenedGate,
                activePayloadRoot: reopenedRoot,
                runMigration: (_reason, operation) => reopenedGate.runMigration(operation),
                adoptActivatedDatabase: reopenedAdoption,
                decodeDatabase: async (bytes) => JSON.parse(new TextDecoder().decode(bytes)) as Database,
                prepareDatabase: async (database) => database,
                encodeDatabase: async () => new Uint8Array(),
                collectReferences: () => migrationFixtureReferences,
                listLegacyInlayAssetIds: async () => [],
                readLegacyInlayPayload: async () => null,
                onCleanupError: (error) => { throw error },
            })

            const reopenedTuple = await reopenedRuntime.initializeAuthority()
            expect(reopenedTuple).toEqual(committedTuple)
            expect(reopenedRoot.current()).toEqual(committedTuple)
            expect(await reopenedStore.listPreparedReplacements()).toEqual([])
            expect(recoveryEvents).toEqual(
                testCase.caseNumber >= 9 && testCase.caseNumber <= 11
                    ? ['discard:production_1']
                    : [],
            )

            const expectedDatabase = testCase.expectedAuthority === 'old' ? databaseBefore : databaseAfter
            expect(await reopenedStore.materializeDatabase()).toEqual(expectedDatabase)
            await expectExactPayload(legacyEntries, { kind: 'legacy' }, reopenedBlobs, reopenedCold)
            if (testCase.expectedAuthority === 'old') {
                expect(await reopenedStages.listGenerations()).toEqual([])
            } else {
                expect(await reopenedStages.listGenerations()).toEqual(['production_1'])
                await expectExactPayload(
                    migrationEntries,
                    { kind: 'generation', id: 'production_1' },
                    reopenedBlobs,
                    reopenedCold,
                )
                const loadLegacyCandidate = vi.fn()
                const bootstrapped = await bootstrapPersistentDatabase({
                    store: reopenedStore,
                    loadLegacyCandidate,
                    prepareDatabase: async (database) => database,
                })
                await reopenedAdoption(bootstrapped.database, bootstrapped.revision)

                expect(loadLegacyCandidate).not.toHaveBeenCalled()
                expect(bootstrapped).toEqual({
                    database: databaseAfter,
                    revision: committedTuple.revision,
                    source: 'persistent',
                })
                expect(reopenedAdoption).toHaveBeenCalledOnce()
                expect(reopenedAdoption).toHaveBeenCalledWith(databaseAfter, committedTuple.revision)
                expect(await reopenedStore.readActiveTuple()).toEqual(committedTuple)
            }
        },
    )
})
