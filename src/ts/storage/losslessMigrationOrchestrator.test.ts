import { compressSync } from 'fflate'
import { describe, expect, test, vi } from 'vitest'
import type { Database } from './database.svelte'
import {
    decodeLosslessMigrationPackage,
    encodeLosslessMigrationPackage,
    type LosslessMigrationInputEntry,
} from './losslessMigrationPackage'
import { createKeyValueRootedBlobStoreFactory } from './platformBlobStore'
import { createKeyValueColdPayloadStore, createRootedColdPayloadStoreFactory } from './platformColdPayloadStore'
import { createMigrationPayloadStageFactory } from './migrationPayloadStage'
import { createLosslessMigrationOrchestrator } from './losslessMigrationOrchestrator'
import {
    RevisionConflictError,
    type ActivePersistentTuple,
    type PreparedPersistentReplacement,
} from './persistentDataStore'
import { makeLosslessMigrationFixture, migrationFixtureReferences } from './tests/losslessMigrationFixtures'

function fixture(options: {
    onInterruption?: (point: string) => void | Promise<void>
    onCleanupError?: (error: unknown) => void
    blobsOverride?: ReturnType<typeof createKeyValueRootedBlobStoreFactory>
} = {}) {
    const values = new Map<string, Uint8Array>()
    const backend = {
        write: async (key: string, value: Uint8Array) => void values.set(key, value.slice()),
        read: async (key: string) => values.get(key)?.slice() ?? null,
        keys: async () => [...values.keys()],
        remove: async (key: string) => void values.delete(key),
    }
    const blobs = createKeyValueRootedBlobStoreFactory(backend)
    const cold = createRootedColdPayloadStoreFactory({
        legacy: createKeyValueColdPayloadStore(backend, {
            key: (id) => `coldstorage/${id}`, prefix: 'coldstorage/', suffix: '',
        }),
        generatedBackend: backend,
    })
    const stages = createMigrationPayloadStageFactory({ backend, blobs, cold, createGenerationId: () => 'import_1' })
    let tuple: ActivePersistentTuple = { revision: 1, dataGeneration: 'data-1', payloadGeneration: 'legacy' }
    let prepared: PreparedPersistentReplacement[] = []
    let activeDatabase = { characters: [], marker: 'old' } as unknown as Database
    const store = {
        readActiveTuple: vi.fn(async () => tuple),
        prepareReplacement: vi.fn(async (database: Database, manifestHash: string, payloadGeneration: string) => {
            activeDatabase = database
            const handle = { id: 'prepared-1', baseRevision: tuple.revision, dataGeneration: 'data-2', payloadGeneration, manifestHash }
            prepared.push(handle)
            return handle
        }),
        activatePreparedReplacement: vi.fn(async ({ prepared: handle }: { prepared: PreparedPersistentReplacement }) => {
            tuple = { revision: 2, dataGeneration: handle.dataGeneration, payloadGeneration: handle.payloadGeneration }
            prepared = prepared.filter((value) => value.id !== handle.id)
            return { revision: 2 }
        }),
        discardPreparedReplacement: vi.fn(async (handle: PreparedPersistentReplacement) => {
            prepared = prepared.filter((value) => value.id !== handle.id)
        }),
        listPreparedReplacements: vi.fn(async () => prepared),
        acquireRevision: vi.fn(async () => ({ revision: tuple.revision, release: vi.fn(async () => undefined) })),
    }
    const install = vi.fn()
    const publish = vi.fn()
    const orchestrator = createLosslessMigrationOrchestrator({
        store: store as never,
        stages,
        blobs: options.blobsOverride ?? blobs,
        cold,
        runMigration: async (_reason, operation) => operation(),
        decodeDatabase: async (bytes) => JSON.parse(new TextDecoder().decode(bytes)) as Database,
        prepareDatabase: async (database) => database,
        encodeDatabase: async () => new TextEncoder().encode(JSON.stringify(activeDatabase)),
        collectReferences: () => migrationFixtureReferences,
        installActiveTuple: install,
        publishDatabase: publish,
        onInterruption: options.onInterruption,
        onCleanupError: options.onCleanupError,
    })
    return { values, backend, blobs, cold, stages, store, orchestrator, install, publish, getTuple: () => tuple }
}

async function packageFixture(includeAsset = true) {
    const entries: LosslessMigrationInputEntry[] = makeLosslessMigrationFixture()
    if (!includeAsset) entries.splice(entries.findIndex((entry) => entry.id === 'assets/photo.png'), 1)
    return encodeLosslessMigrationPackage(entries)
}

describe('lossless migration orchestrator', () => {
    test('imports a complete package through seal, prepare, activation, and publication', async () => {
        const target = fixture()
        const result = await target.orchestrator.importPackage(await packageFixture())

        expect(result.tuple).toEqual({ revision: 2, dataGeneration: 'data-2', payloadGeneration: 'import_1' })
        expect(result.unreferenced).toEqual([])
        expect(await target.blobs.open({ kind: 'generation', id: 'import_1' }).read('assets/photo.png')).toEqual(new Uint8Array([1, 2, 3]))
        expect(await target.cold.open({ kind: 'generation', id: 'import_1' }).list()).toEqual(['character-cold', 'chat-cold'])
        expect(target.install).toHaveBeenCalledWith(result.tuple)
        expect(target.publish).toHaveBeenCalledOnce()
    })

    test('rejects a missing referenced entry without changing the active tuple', async () => {
        const target = fixture()
        await expect(target.orchestrator.importPackage(await packageFixture(false))).rejects.toThrow(/assets\/photo\.png/)
        expect(target.getTuple()).toEqual({ revision: 1, dataGeneration: 'data-1', payloadGeneration: 'legacy' })
        expect([...target.values.keys()].some((key) => key.includes('/import_1/'))).toBe(false)
        expect(target.store.activatePreparedReplacement).not.toHaveBeenCalled()
    })

    test('cleans an interrupted sealed stage and leaves the old tuple authoritative', async () => {
        const target = fixture({ onInterruption: (point) => {
            if (point === 'after-seal') throw new Error('simulated interruption')
        } })
        await expect(target.orchestrator.importPackage(await packageFixture())).rejects.toThrow('simulated interruption')
        expect(target.getTuple()).toEqual({ revision: 1, dataGeneration: 'data-1', payloadGeneration: 'legacy' })
        expect([...target.values.keys()].some((key) => key.includes('/import_1/'))).toBe(false)
        expect(target.store.prepareReplacement).not.toHaveBeenCalled()
    })

    test('exports one database entry plus every live blob and cold value and releases its lease', async () => {
        const target = fixture()
        const blobs = target.blobs.open({ kind: 'legacy' })
        await blobs.put('assets/a.png', new Uint8Array([1]), {
            kind: 'asset', mime: 'image/png', name: 'a.png', ext: 'png',
        })
        await blobs.put('inlay-a', new Uint8Array([2]), {
            kind: 'inlay', mime: 'image/png', name: 'inlay', ext: 'png', inlayType: 'image',
        })
        const cold = compressSync(new TextEncoder().encode(JSON.stringify([])))
        await target.cold.open({ kind: 'legacy' }).write('cold-a', cold)

        const decoded = await decodeLosslessMigrationPackage(await target.orchestrator.exportPackage())
        const entries = []
        for await (const entry of decoded.entries()) entries.push(`${entry.kind}:${entry.id}`)
        expect(entries).toEqual([
            'database:database.risudat', 'asset:assets/a.png', 'inlay:inlay-a', 'cold:cold-a',
        ])
        const lease = await target.store.acquireRevision.mock.results[0].value
        expect(lease.release).toHaveBeenCalledOnce()
    })

    test('snapshots each exported source before a backend reuses its read buffer', async () => {
        const scratch = new Uint8Array(1)
        const metadata = [
            { key: 'assets/first.png', kind: 'asset' as const, size: 1, mime: 'image/png', name: 'first.png', ext: 'png' },
            { key: 'assets/second.png', kind: 'asset' as const, size: 1, mime: 'image/png', name: 'second.png', ext: 'png' },
        ]
        const blobsOverride = {
            open: () => ({
                put: vi.fn(), stat: vi.fn(), remove: vi.fn(), resolveUrl: vi.fn(),
                list: async () => metadata,
                read: async (key: string) => {
                    scratch[0] = key.includes('first') ? 1 : 2
                    return scratch
                },
            }),
        } as never
        const target = fixture({ blobsOverride })
        const decoded = await decodeLosslessMigrationPackage(await target.orchestrator.exportPackage())
        const values = new Map<string, number>()
        for await (const entry of decoded.entries()) {
            if (entry.kind === 'asset') values.set(entry.id, entry.data[0])
        }

        expect(values).toEqual(new Map([
            ['assets/first.png', 1],
            ['assets/second.png', 2],
        ]))
    })

    test('recovers inactive prepared roots payload-first and never auto-activates', async () => {
        const target = fixture()
        const stage = target.stages.create('legacy')
        await target.cold.open({ kind: 'generation', id: stage.payloadGeneration }).write('partial', new Uint8Array([1]))
        const handle: PreparedPersistentReplacement = {
            id: 'abandoned', baseRevision: 1, dataGeneration: 'data-abandoned',
            payloadGeneration: stage.payloadGeneration, manifestHash: '0'.repeat(64),
        }
        ;(await target.store.listPreparedReplacements()).push(handle)

        await target.orchestrator.recoverInterruptedMigrations()

        expect(target.store.discardPreparedReplacement).toHaveBeenCalledWith(handle)
        expect(target.store.activatePreparedReplacement).not.toHaveBeenCalled()
        expect([...target.values.keys()].some((key) => key.includes(`/${stage.payloadGeneration}/`))).toBe(false)
    })

    test('preserves the activation conflict when prepared-marker cleanup also fails', async () => {
        const onCleanupError = vi.fn()
        const target = fixture({ onCleanupError })
        const conflict = new RevisionConflictError(1, 2)
        const cleanup = new Error('discard failed')
        target.store.activatePreparedReplacement.mockRejectedValueOnce(conflict)
        target.store.discardPreparedReplacement.mockRejectedValueOnce(cleanup)

        await expect(target.orchestrator.importPackage(await packageFixture())).rejects.toBe(conflict)

        expect(target.store.activatePreparedReplacement).toHaveBeenCalledOnce()
        expect(target.getTuple()).toEqual({ revision: 1, dataGeneration: 'data-1', payloadGeneration: 'legacy' })
        expect(await target.store.listPreparedReplacements()).toHaveLength(1)
        expect(onCleanupError).toHaveBeenCalledWith(cleanup)

        await target.orchestrator.recoverInterruptedMigrations()
        expect(await target.store.listPreparedReplacements()).toHaveLength(0)
        expect(target.store.discardPreparedReplacement).toHaveBeenCalledTimes(2)
    })
})
