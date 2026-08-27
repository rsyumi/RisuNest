import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'

import {
    projectPinnedCompatibilityDatabase,
    type AssetRepositoryOwnerManifestView,
} from './assetOwnerCompatibilityProjector'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import {
    encodeOwnerManifest,
    ownerManifestIdentity,
    type AssetTuple,
    type OwnerManifestEntry,
} from './ownerManifestCodec'
import type { AssetOwnerHead, AssetOwnerLocator } from './persistentDataStore'
import {
    ASSET_TUPLES_PER_OWNER,
    makeAssetManifestFixture,
} from './tests/assetManifestFixtures'
import { fixtureDatabase } from './tests/persistentDataFixtures'

type PresentAssetOwnerHead = Extract<AssetOwnerHead, { present: true }>

async function encodedManifest(
    tuples: readonly AssetTuple[],
    payloadHashes: readonly (Uint8Array | null)[],
): Promise<{ bytes: Uint8Array; head: Omit<PresentAssetOwnerHead, 'owner'> }> {
    const entries: OwnerManifestEntry[] = tuples.map((tuple, index) => ({
        tuple,
        payloadHash: payloadHashes[index],
    }))
    const bytes = encodeOwnerManifest(entries)
    return {
        bytes,
        head: {
            present: true,
            manifestHash: await ownerManifestIdentity(bytes),
            entryCount: entries.length,
        },
    }
}

describe('pinned asset-owner compatibility projector', () => {
    it('reconstructs exact legacy arrays from occurrence heads at one pinned revision', async () => {
        const database = structuredClone(fixtureDatabase)
        const duplicate: [string, string, string] = [
            '중복\\Name',
            'Assets\\Mixed/Path.BIN',
            'OddExt',
        ]
        const moduleManifestTuples: [string, string, string][] = [
            duplicate,
            ['second', 'https://example.invalid/remote', 'REMOTE'],
            duplicate,
        ]
        const moduleTuples = [
            [...moduleManifestTuples[0], 'first-tail', { rank: 1 }],
            [...moduleManifestTuples[1], 'second-tail', { rank: 2 }],
            [...moduleManifestTuples[2], 'third-tail', { rank: 3 }],
        ] as unknown as [string, string, string][]
        const personaTuples: [string, string, string][] = [
            ['persona', 'assets/persona.bin', ''],
            ['persona', 'assets/persona.bin', ''],
        ]
        const characterTuples: [string, string, string][] = [
            ['zero', 'assets/zero.bin', 'BIN'],
            ['missing', 'assets/missing.bin', 'unknown'],
        ]
        database.modules = [
            { id: 'duplicate', name: 'Absent', description: '' },
            { id: 'duplicate', name: 'Empty', description: '', assets: [] },
            { id: 'duplicate', name: 'Ordered', description: '', assets: moduleTuples },
        ]
        database.personas = [
            {
                name: 'No ID and absent assets',
                personaPrompt: '',
                icon: '',
                embeddedModule: { id: '', name: 'Absent', description: '' },
            },
            {
                name: 'No ID and duplicate assets',
                personaPrompt: '',
                icon: '',
                embeddedModule: {
                    id: '',
                    name: 'Present',
                    description: '',
                    assets: personaTuples,
                },
            },
        ]
        database.characters[0].additionalAssets = characterTuples

        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'asset-owner-projector-exact',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const root = (await store.readRoot()).value
        const character = (await store.readCharacter(database.characters[0].chaId))!.value
        const manifests = new Map<string, Uint8Array>()
        const heads: AssetOwnerHead[] = []
        const addPresentHead = async (
            owner: AssetOwnerLocator,
            tuples: readonly AssetTuple[],
            payloadHashes: readonly (Uint8Array | null)[],
        ) => {
            const manifest = await encodedManifest(tuples, payloadHashes)
            manifests.set(manifest.head.manifestHash!, manifest.bytes)
            heads.push({ owner, ...manifest.head } as AssetOwnerHead)
        }
        heads.push({
            owner: { kind: 'root-module-assets', index: 0 },
            present: false,
            manifestHash: null,
            entryCount: 0,
        })
        await addPresentHead(
            { kind: 'root-module-assets', index: 1 },
            [],
            [],
        )
        await addPresentHead(
            { kind: 'root-module-assets', index: 2 },
            moduleManifestTuples,
            [new Uint8Array(32).fill(1), null, new Uint8Array(32).fill(1)],
        )
        heads.push({
            owner: { kind: 'persona-embedded-module-assets', index: 0 },
            present: false,
            manifestHash: null,
            entryCount: 0,
        })
        await addPresentHead(
            { kind: 'persona-embedded-module-assets', index: 1 },
            personaTuples,
            [new Uint8Array(32).fill(2), new Uint8Array(32).fill(2)],
        )
        await addPresentHead(
            {
                kind: 'character-additional-assets',
                characterId: database.characters[0].chaId,
            },
            characterTuples,
            [new Uint8Array(32), null],
        )
        const shadowed = await store.commit({
            expectedRevision: imported.revision,
            root,
            character,
            assetOwnerHeads: heads,
        })
        const lease = await store.acquireRevision(shadowed.revision)
        const changedRoot = structuredClone(root)
        changedRoot.modules[2].assets = [['new', 'assets/new.bin', 'bin']]
        await store.commit({
            expectedRevision: shadowed.revision,
            root: changedRoot,
        })
        const readOwnerManifest = vi.fn(async (hash: string) => manifests.get(hash)?.slice() ?? null)
        const repository: AssetRepositoryOwnerManifestView = { readOwnerManifest }

        const projected = await projectPinnedCompatibilityDatabase(lease, repository)

        expect(projected).toEqual(database)
        expect(Object.prototype.hasOwnProperty.call(projected.modules[0], 'assets')).toBe(false)
        expect(Object.prototype.hasOwnProperty.call(projected.modules[1], 'assets')).toBe(true)
        expect(projected.modules[1].assets).toEqual([])
        expect(projected.modules[2].assets).toEqual(moduleTuples)
        expect(projected.personas[1].embeddedModule!.assets).toEqual(personaTuples)
        expect(projected.characters[0].additionalAssets).toEqual(characterTuples)
        expect(readOwnerManifest).toHaveBeenCalledTimes(4)
        await lease.release()
    })

    it('rejects a manifest that does not exactly match the pinned legacy parent', async () => {
        const database = structuredClone(fixtureDatabase)
        database.modules = [{
            id: 'module',
            name: 'Module',
            description: '',
            assets: [['legacy', 'assets/legacy.bin', 'BIN']],
        }]
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'asset-owner-projector-reject',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const root = (await store.readRoot()).value
        const mismatched = await encodedManifest(
            [['different', 'assets/different.bin', 'bin']],
            [null],
        )
        const head: AssetOwnerHead = {
            owner: { kind: 'root-module-assets', index: 0 },
            ...mismatched.head,
        } as AssetOwnerHead
        const shadowed = await store.commit({
            expectedRevision: imported.revision,
            root,
            assetOwnerHeads: [head],
        })
        const lease = await store.acquireRevision(shadowed.revision)

        await expect(projectPinnedCompatibilityDatabase(lease, {
            readOwnerManifest: async () => mismatched.bytes,
        })).rejects.toThrow('does not match pinned legacy tuples')
        await lease.release()
    })

    it('rejects trailing fields for character additional assets', async () => {
        const database = structuredClone(fixtureDatabase)
        database.modules = []
        database.personas = []
        database.characters[0].additionalAssets = [[
            'character',
            'assets/character.bin',
            'bin',
            'unsupported-tail',
        ]] as unknown as [string, string, string][]
        const manifest = await encodedManifest(
            [['character', 'assets/character.bin', 'bin']],
            [null],
        )
        const store = new IndexedDbPersistentDataStore(
            'asset-owner-projector-character-tail',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const character = (await store.readCharacter(database.characters[0].chaId))!.value
        const committed = await store.commit({
            expectedRevision: imported.revision,
            character,
            assetOwnerHeads: [{
                owner: {
                    kind: 'character-additional-assets',
                    characterId: database.characters[0].chaId,
                },
                ...manifest.head,
            } as AssetOwnerHead],
        })
        const lease = await store.acquireRevision(committed.revision)

        await expect(projectPinnedCompatibilityDatabase(lease, {
            readOwnerManifest: async () => manifest.bytes,
        })).rejects.toThrow('does not match pinned legacy tuples')
        await lease.release()
    })

    it('bounds expected-scale projection to one head query and one manifest decode', async () => {
        const database = makeAssetManifestFixture()
        database.personas = []
        database.characters = []
        const tuples = database.modules[0].assets!
        const manifest = await encodedManifest(
            tuples,
            Array.from({ length: tuples.length }, () => null),
        )
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'asset-owner-projector-scale',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const root = (await store.readRoot()).value
        const shadowed = await store.commit({
            expectedRevision: imported.revision,
            root,
            assetOwnerHeads: [{
                owner: { kind: 'root-module-assets', index: 0 },
                ...manifest.head,
            }],
        })
        const lease = await store.acquireRevision(shadowed.revision)
        const readOwnerHead = vi.spyOn(lease, 'readAssetOwnerHead')
        const readOwnerManifest = vi.fn(async () => manifest.bytes.slice())

        const start = performance.now()
        const projected = await projectPinnedCompatibilityDatabase(lease, {
            readOwnerManifest,
        })
        const projectionMs = performance.now() - start

        expect(projected.modules[0].assets).toEqual(tuples)
        expect(projected.modules[0].assets).toHaveLength(ASSET_TUPLES_PER_OWNER)
        expect(readOwnerHead).toHaveBeenCalledTimes(1)
        expect(readOwnerManifest).toHaveBeenCalledTimes(1)
        console.info('owner-head-projector-measurement', JSON.stringify({
            tuples: ASSET_TUPLES_PER_OWNER,
            canonicalBytes: manifest.bytes.byteLength,
            headQueries: readOwnerHead.mock.calls.length,
            manifestReads: readOwnerManifest.mock.calls.length,
            projectionMs,
        }))
        await lease.release()
    })
})
