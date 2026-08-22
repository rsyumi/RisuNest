import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import { createApplicationMigrationRuntime } from './applicationMigrationRuntime'
import { ActivePayloadRoot } from './activePayloadRoot'
import { createKeyValueRootedBlobStoreFactory } from './platformBlobStore'
import { createLegacyNodeColdPayloadStore, createRootedColdPayloadStoreFactory } from './platformColdPayloadStore'
import { createMigrationPayloadStageFactory } from './migrationPayloadStage'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { createStorageMutationGate } from './storageMutationGate'
import { encodeLosslessMigrationPackage } from './losslessMigrationPackage'
import { makeLosslessMigrationFixture, migrationFixtureReferences } from './tests/losslessMigrationFixtures'
import type { Database } from './database.svelte'

function memoryBackend() {
    const values = new Map<string, Uint8Array>()
    return {
        values,
        write: async (key: string, value: Uint8Array) => void values.set(key, value.slice()),
        read: async (key: string) => values.get(key)?.slice() ?? null,
        keys: async () => [...values.keys()],
        remove: async (key: string) => void values.delete(key),
    }
}

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
})
