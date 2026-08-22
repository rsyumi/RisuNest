import { compressSync } from 'fflate'
import { describe, expect, test, vi } from 'vitest'
import type { BlobWriteMetadata } from './blobStore'
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

function writeMetadata(entry: LosslessMigrationInputEntry): BlobWriteMetadata {
    if ((entry.kind === 'asset' && entry.metadata.kind === 'asset')
        || (entry.kind === 'inlay' && entry.metadata.kind === 'inlay')) return entry.metadata
    throw new TypeError(`Expected blob metadata for ${entry.kind}:${entry.id}`)
}

function fixture(options: {
    onInterruption?: (point: string, entry?: { kind: string; id: string }) => void | Promise<void>
    onCleanupError?: (error: unknown) => void
    blobsOverride?: ReturnType<typeof createKeyValueRootedBlobStoreFactory>
    listLegacyInlayAssetIds?: () => Promise<string[]>
    readLegacyInlayPayload?: (id: string) => Promise<{
        data: Uint8Array
        metadata: {
            kind: 'inlay'
            mime: string
            name: string
            ext: string
            inlayType: 'image' | 'audio' | 'video' | 'signature'
            width?: number
            height?: number
        }
    } | null>
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
    const runMigrationCalls = vi.fn()
    const runMigration = async <T,>(reason: string, operation: () => Promise<T>): Promise<T> => {
        runMigrationCalls(reason, operation)
        return operation()
    }
    const listLegacyInlayAssetIds = vi.fn(options.listLegacyInlayAssetIds ?? (async () => []))
    const readLegacyInlayPayload = vi.fn(options.readLegacyInlayPayload ?? (async () => null))
    const orchestrator = createLosslessMigrationOrchestrator({
        store: store as never,
        stages,
        blobs: options.blobsOverride ?? blobs,
        cold,
        runMigration,
        decodeDatabase: async (bytes) => JSON.parse(new TextDecoder().decode(bytes)) as Database,
        prepareDatabase: async (database) => database,
        encodeDatabase: async () => new TextEncoder().encode(JSON.stringify(activeDatabase)),
        collectReferences: () => migrationFixtureReferences,
        listLegacyInlayAssetIds,
        readLegacyInlayPayload,
        installActiveTuple: install,
        publishDatabase: publish,
        onInterruption: options.onInterruption,
        onCleanupError: options.onCleanupError,
    })
    return {
        values, backend, blobs, cold, stages, store, orchestrator, install, publish,
        runMigration: runMigrationCalls, listLegacyInlayAssetIds, readLegacyInlayPayload,
        getTuple: () => tuple,
        setTuple: (value: ActivePersistentTuple) => { tuple = value },
        getDatabase: () => activeDatabase,
    }
}

async function packageFixture(includeAsset = true) {
    const entries: LosslessMigrationInputEntry[] = makeLosslessMigrationFixture()
    if (!includeAsset) entries.splice(entries.findIndex((entry) => entry.id === 'assets/photo.png'), 1)
    return encodeLosslessMigrationPackage(entries)
}

describe('lossless migration orchestrator', () => {
    test('returns already_migrated without creating a stage, source read, or revision lease', async () => {
        const target = fixture()
        target.setTuple({ revision: 4, dataGeneration: 'data-4', payloadGeneration: 'generated_4' })
        const createStage = vi.spyOn(target.stages, 'create')

        const result = await target.orchestrator.migrateLegacyInPlace()

        expect(result).toEqual({
            status: 'already_migrated',
            tuple: { revision: 4, dataGeneration: 'data-4', payloadGeneration: 'generated_4' },
        })
        expect(target.runMigration).toHaveBeenCalledOnce()
        expect(target.runMigration).toHaveBeenCalledWith('lossless-in-place', expect.any(Function))
        expect(createStage).not.toHaveBeenCalled()
        expect(target.store.acquireRevision).not.toHaveBeenCalled()
        expect(target.listLegacyInlayAssetIds).not.toHaveBeenCalled()
        expect(target.readLegacyInlayPayload).not.toHaveBeenCalled()
    })

    test('migrates current legacy database, assets, all inlay kinds, and cold payloads', async () => {
        const source = makeLosslessMigrationFixture()
        const rawInlays = new Map(source
            .filter((entry) => entry.kind === 'inlay')
            .map((entry) => [entry.id, entry] as const))
        const rawInlaySnapshots = new Map([...rawInlays].map(([id, entry]) => [id, entry.data.slice()]))
        const target = fixture({
            listLegacyInlayAssetIds: async () => [...rawInlays.keys()].reverse(),
            readLegacyInlayPayload: async (id) => {
                const entry = rawInlays.get(id)
                return entry?.kind === 'inlay' && entry.metadata.kind === 'inlay'
                    ? { data: entry.data, metadata: entry.metadata }
                    : null
            },
        })
        const legacyBlobs = target.blobs.open({ kind: 'legacy' })
        for (const entry of source.filter((entry) => entry.kind === 'asset')) {
            if (entry.kind !== 'asset') continue
            await legacyBlobs.put(entry.id, entry.data, writeMetadata(entry))
        }
        const legacyCold = target.cold.open({ kind: 'legacy' })
        for (const entry of source.filter((entry) => entry.kind === 'cold')) {
            if (entry.kind === 'cold') await legacyCold.write(entry.id, entry.data)
        }
        target.values.set('database/database.bin', new Uint8Array([91, 92]))
        const legacyValues = new Map([...target.values].map(([key, value]) => [key, value.slice()]))

        const result = await target.orchestrator.migrateLegacyInPlace()

        expect(result.status).toBe('migrated')
        if (result.status !== 'migrated') throw new Error('Expected migration result')
        expect(result.tuple).toEqual({ revision: 2, dataGeneration: 'data-2', payloadGeneration: 'import_1' })
        const migratedBlobs = target.blobs.open({ kind: 'generation', id: 'import_1' })
        for (const entry of source.filter((entry) => entry.kind === 'asset' || entry.kind === 'inlay')) {
            expect(await migratedBlobs.read(entry.id)).toEqual(entry.data)
            expect(await migratedBlobs.stat(entry.id)).toMatchObject(entry.metadata)
        }
        const migratedCold = target.cold.open({ kind: 'generation', id: 'import_1' })
        for (const entry of source.filter((entry) => entry.kind === 'cold')) {
            expect(await migratedCold.read(entry.id)).toEqual(entry.data)
        }
        for (const [key, value] of legacyValues) expect(target.values.get(key)).toEqual(value)
        for (const [id, entry] of rawInlays) expect(entry.data).toEqual(rawInlaySnapshots.get(id))
        expect(target.runMigration).toHaveBeenCalledTimes(1)
        expect(target.store.acquireRevision).toHaveBeenCalledWith(1)
        const lease = await target.store.acquireRevision.mock.results[0].value
        expect(lease.release).toHaveBeenCalledOnce()
        expect(target.publish).toHaveBeenCalledOnce()
        expect(target.getDatabase()).toMatchObject({ marker: 'old' })
    })

    test('skips a raw legacy inlay when the live legacy BlobStore already owns that ID', async () => {
        const target = fixture({
            listLegacyInlayAssetIds: async () => ['image-inlay'],
            readLegacyInlayPayload: async () => ({
                data: new Uint8Array([99]),
                metadata: {
                    kind: 'inlay', mime: 'image/png', name: 'raw', ext: 'png', inlayType: 'image',
                },
            }),
        })
        const source = makeLosslessMigrationFixture()
        const live = source.find((entry) => entry.kind === 'inlay' && entry.id === 'image-inlay')
        if (!live || live.kind !== 'inlay') throw new Error('Missing fixture inlay')
        const blobs = target.blobs.open({ kind: 'legacy' })
        await blobs.put(live.id, live.data, writeMetadata(live))
        for (const entry of source.filter((entry) => entry.kind === 'asset')) {
            if (entry.kind === 'asset') await blobs.put(entry.id, entry.data, writeMetadata(entry))
        }
        for (const entry of source.filter((entry) => entry.kind === 'inlay' && entry.id !== 'image-inlay')) {
            if (entry.kind === 'inlay') await blobs.put(entry.id, entry.data, writeMetadata(entry))
        }
        for (const entry of source.filter((entry) => entry.kind === 'cold')) {
            if (entry.kind === 'cold') await target.cold.open({ kind: 'legacy' }).write(entry.id, entry.data)
        }

        const result = await target.orchestrator.migrateLegacyInPlace()

        expect(result.status).toBe('migrated')
        expect(target.readLegacyInlayPayload).not.toHaveBeenCalled()
        expect(await target.blobs.open({ kind: 'generation', id: 'import_1' }).read('image-inlay')).toEqual(live.data)
    })

    test('preserves every legacy source after success and after a staging failure', async () => {
        const source = makeLosslessMigrationFixture()
        const rawInlays = new Map(source
            .filter((entry) => entry.kind === 'inlay')
            .map((entry) => [entry.id, entry] as const))
        const rawSnapshots = new Map([...rawInlays].map(([id, entry]) => [id, entry.data.slice()]))
        const target = fixture({
            listLegacyInlayAssetIds: async () => [...rawInlays.keys()],
            readLegacyInlayPayload: async (id) => {
                const entry = rawInlays.get(id)
                return entry?.metadata.kind === 'inlay'
                    ? { data: entry.data, metadata: entry.metadata }
                    : null
            },
            onInterruption: (point, entry) => {
                if (point === 'after-entry' && entry?.kind === 'inlay') throw new Error('staging failed')
            },
        })
        const blobs = target.blobs.open({ kind: 'legacy' })
        const cold = target.cold.open({ kind: 'legacy' })
        for (const entry of source) {
            if (entry.kind === 'asset') {
                await blobs.put(entry.id, entry.data, writeMetadata(entry))
            }
            if (entry.kind === 'cold') await cold.write(entry.id, entry.data)
        }
        target.values.set('database/database.bin', new Uint8Array([71, 72]))
        const beforeValues = new Map([...target.values].map(([key, value]) => [key, value.slice()]))

        await expect(target.orchestrator.migrateLegacyInPlace()).rejects.toThrow('staging failed')

        expect(target.getTuple()).toEqual({ revision: 1, dataGeneration: 'data-1', payloadGeneration: 'legacy' })
        for (const [key, value] of beforeValues) expect(target.values.get(key)).toEqual(value)
        for (const [id, entry] of rawInlays) expect(entry.data).toEqual(rawSnapshots.get(id))
        expect([...target.values.keys()].some((key) => key.includes('/import_1/'))).toBe(false)
        const lease = await target.store.acquireRevision.mock.results[0].value
        expect(lease.release).toHaveBeenCalledOnce()
    })

    test('owns reused legacy source scratch bytes before reading the next source', async () => {
        const scratch = new Uint8Array(1)
        const inlayIds = ['image-inlay', 'audio-inlay', 'video-inlay', 'signature-inlay']
        const expected = new Map(inlayIds.map((id, index) => [id, index + 10]))
        const target = fixture({
            listLegacyInlayAssetIds: async () => inlayIds,
            readLegacyInlayPayload: async (id) => {
                scratch[0] = expected.get(id) ?? 0
                const inlayType = id.split('-')[0] as 'image' | 'audio' | 'video' | 'signature'
                return {
                    data: scratch,
                    metadata: { kind: 'inlay', mime: 'application/octet-stream', name: id, ext: 'bin', inlayType },
                }
            },
        })
        const source = makeLosslessMigrationFixture()
        const blobs = target.blobs.open({ kind: 'legacy' })
        for (const entry of source.filter((entry) => entry.kind === 'asset')) {
            if (entry.kind === 'asset') await blobs.put(entry.id, entry.data, writeMetadata(entry))
        }
        for (const entry of source.filter((entry) => entry.kind === 'cold')) {
            if (entry.kind === 'cold') await target.cold.open({ kind: 'legacy' }).write(entry.id, entry.data)
        }

        const result = await target.orchestrator.migrateLegacyInPlace()

        expect(result.status).toBe('migrated')
        const migrated = target.blobs.open({ kind: 'generation', id: 'import_1' })
        for (const [id, value] of expected) expect(await migrated.read(id)).toEqual(new Uint8Array([value]))
    })

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

    test('boots a clean legacy store without an exclusive migration lock', async () => {
        const target = fixture()

        await target.orchestrator.recoverInterruptedMigrations()

        expect(target.runMigration).not.toHaveBeenCalled()
        expect(target.store.discardPreparedReplacement).not.toHaveBeenCalled()
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
