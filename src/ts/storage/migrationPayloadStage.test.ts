import { compressSync } from 'fflate'
import { describe, expect, test } from 'vitest'
import { createKeyValueRootedBlobStoreFactory } from './platformBlobStore'
import { createKeyValueColdPayloadStore, createRootedColdPayloadStoreFactory } from './platformColdPayloadStore'
import { createMigrationPayloadStageFactory } from './migrationPayloadStage'
import { createLosslessMigrationManifest, hashLosslessMigrationManifest } from './losslessMigrationPackage'

function backendFixture() {
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
    return { values, backend, blobs, cold }
}

const sha256 = async (data: Uint8Array) => Array.from(
    new Uint8Array(await crypto.subtle.digest('SHA-256', data as BufferSource)),
    (value) => value.toString(16).padStart(2, '0'),
).join('')

describe('migration payload stage', () => {
    test('verifies entries, seals last, and keeps only the inactive handle immutable', async () => {
        const fixture = backendFixture()
        const factory = createMigrationPayloadStageFactory({
            ...fixture,
            createGenerationId: () => 'stage_1',
        })
        const stage = factory.create('legacy')
        const asset = new Uint8Array([1, 2, 3])
        const cold = compressSync(new TextEncoder().encode(JSON.stringify([])))
        await stage.put({
            kind: 'asset', id: 'assets/a.png',
            metadata: { kind: 'asset', mime: 'image/png', name: 'a.png', ext: 'png' },
            size: asset.byteLength, sha256: await sha256(asset), data: asset,
        })
        await stage.put({
            kind: 'cold', id: 'cold-a', metadata: {}, size: cold.byteLength,
            sha256: await sha256(cold), data: cold,
        })
        const database = new Uint8Array([9])
        const manifest = createLosslessMigrationManifest([
            { kind: 'database', id: 'database.risudat', metadata: {}, size: 1, sha256: await sha256(database) },
            { kind: 'asset', id: 'assets/a.png', metadata: { kind: 'asset', mime: 'image/png', name: 'a.png', ext: 'png' }, size: asset.byteLength, sha256: await sha256(asset) },
            { kind: 'cold', id: 'cold-a', metadata: {}, size: cold.byteLength, sha256: await sha256(cold) },
        ])
        const manifestHash = await hashLosslessMigrationManifest(manifest)

        await stage.seal(manifest, manifestHash)
        await expect(stage.verifySeal()).resolves.toMatchObject({
            payloadGeneration: 'stage_1', previousPayloadGeneration: 'legacy', manifestHash,
        })
        await expect(stage.put({
            kind: 'cold', id: 'later', metadata: {}, size: cold.byteLength,
            sha256: await sha256(cold), data: cold,
        })).rejects.toThrow(/sealed/i)

        const activeCold = fixture.cold.open({ kind: 'generation', id: 'stage_1' })
        await activeCold.write('ordinary', new Uint8Array([7]))
        expect(await activeCold.read('ordinary')).toEqual(new Uint8Array([7]))
    })

    test('rejects undeclared staged values and cleans only an inactive exact root', async () => {
        const fixture = backendFixture()
        const factory = createMigrationPayloadStageFactory({ ...fixture, createGenerationId: () => 'stage_2' })
        const stage = factory.create('legacy')
        await fixture.cold.open({ kind: 'generation', id: 'stage_2' }).write('extra', new Uint8Array([1]))
        const database = new Uint8Array([9])
        const manifest = createLosslessMigrationManifest([{
            kind: 'database', id: 'database.risudat', metadata: {}, size: 1, sha256: await sha256(database),
        }])
        await expect(stage.seal(manifest, await hashLosslessMigrationManifest(manifest))).rejects.toThrow(/staged|declared/i)
        await expect(factory.removeGeneration('stage_2', 'stage_2')).rejects.toThrow(/active/i)
        await factory.removeGeneration('stage_2', 'legacy')
        expect([...fixture.values.keys()].some((key) => key.includes('/stage_2/'))).toBe(false)
    })

    test('rejects mutations through a handle reopened after the physical seal exists', async () => {
        const fixture = backendFixture()
        const factory = createMigrationPayloadStageFactory({ ...fixture, createGenerationId: () => 'sealed_1' })
        const stage = factory.create('legacy')
        const database = new Uint8Array([9])
        const manifest = createLosslessMigrationManifest([{
            kind: 'database', id: 'database.risudat', metadata: {}, size: 1, sha256: await sha256(database),
        }])
        const manifestHash = await hashLosslessMigrationManifest(manifest)
        await stage.seal(manifest, manifestHash)
        const reopened = factory.open('sealed_1', 'legacy')
        const late = new Uint8Array([1])

        await expect(reopened.put({
            kind: 'asset', id: 'assets/late.png',
            metadata: { kind: 'asset', mime: 'image/png', name: 'late.png', ext: 'png' },
            size: 1, sha256: await sha256(late), data: late,
        })).rejects.toThrow(/sealed/i)
        await expect(reopened.seal(manifest, manifestHash)).rejects.toThrow(/sealed/i)
        expect(await fixture.blobs.open({ kind: 'generation', id: 'sealed_1' }).read('assets/late.png')).toBeNull()
    })

    test('does not bypass a physical seal after its first seal read fails', async () => {
        const fixture = backendFixture()
        fixture.values.set(
            'blobstore/generations/sealed_retry/migration-manifest.json',
            new TextEncoder().encode('{}'),
        )
        let failSealRead = true
        const backend = {
            ...fixture.backend,
            async read(key: string) {
                if (failSealRead && key.endsWith('/migration-manifest.json')) {
                    failSealRead = false
                    throw new Error('transient seal read failure')
                }
                return fixture.backend.read(key)
            },
        }
        const factory = createMigrationPayloadStageFactory({
            backend,
            blobs: createKeyValueRootedBlobStoreFactory(backend),
            cold: fixture.cold,
        })
        const stage = factory.open('sealed_retry', 'legacy')
        const late = new Uint8Array([1])
        const entry = {
            kind: 'asset' as const, id: 'assets/late.png',
            metadata: { kind: 'asset' as const, mime: 'image/png', name: 'late.png', ext: 'png' },
            size: 1, sha256: await sha256(late), data: late,
        }

        await expect(stage.put(entry)).rejects.toThrow('transient seal read failure')
        await expect(stage.put(entry)).rejects.toThrow(/sealed/i)
        expect(await fixture.blobs.open({ kind: 'generation', id: 'sealed_retry' }).read(entry.id)).toBeNull()
    })
})
