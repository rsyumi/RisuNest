import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import type { BlobStore } from './blobStore'
import { createImmutablePayloadCas, type ImmutablePayloadCas } from './payloadCas'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import type {
    AssetAlias,
    PersistentDataStore,
    PersistentRevisionReader,
} from './persistentDataStore'
import { createAssetRepository, createAssetRepositoryBlobStore } from './assetRepository'
import { fixtureDatabase } from './tests/persistentDataFixtures'

describe('AssetRepository BlobStore facade', () => {
    it('prepares the immutable object before committing one exact alias', async () => {
        const events: string[] = []
        let alias: AssetAlias | null = null
        const store = {
            readRoot: vi.fn(async () => ({ revision: 7, value: {} })),
            readAssetAlias: vi.fn(async () => alias === null ? null : { revision: 8, value: alias }),
            commitAssetAlias: vi.fn(async (value: AssetAlias, expectedRevision: number) => {
                events.push(`alias:${expectedRevision}`)
                alias = structuredClone(value)
                return { revision: expectedRevision + 1 }
            }),
        } as unknown as PersistentDataStore
        const cas = {
            prepare: vi.fn(async (data: Uint8Array) => {
                events.push('object')
                return {
                    contentHash: '88'.repeat(32),
                    byteSize: data.byteLength,
                    physicalKey: `assets-v2/objects/88/${'88'.repeat(31)}`,
                    deduplicated: false,
                }
            }),
        } as unknown as ImmutablePayloadCas
        const legacy = {
            put: vi.fn(),
        } as unknown as BlobStore
        const facade = createAssetRepositoryBlobStore({
            store,
            cas,
            legacy,
            legacyFallback: true,
            removal: 'disabled',
        })
        const input = new Uint8Array([1, 2, 3])

        await expect(facade.put('assets/exact.bin', input, {
            kind: 'asset',
            mime: 'application/X-Exact',
            name: 'Exact Name',
            ext: 'BIN',
        })).resolves.toEqual({
            key: 'assets/exact.bin',
            kind: 'asset',
            size: 3,
            mime: 'application/X-Exact',
            name: 'Exact Name',
            ext: 'BIN',
        })

        expect(events).toEqual(['object', 'alias:7'])
        expect(alias).toEqual({
            key: 'assets/exact.bin',
            objectHash: '88'.repeat(32),
            kind: 'asset',
            size: 3,
            mime: 'application/X-Exact',
            name: 'Exact Name',
            ext: 'BIN',
        })
        expect(legacy.put).not.toHaveBeenCalled()
    })

    it('reads and stats current and pinned aliases directly without enumerating objects', async () => {
        const key = 'assets/revision.bin'
        const zeroKey = 'assets/zero.bin'
        const missingPayloadKey = 'assets/missing.bin'
        const currentAlias: AssetAlias = {
            key,
            objectHash: 'aa'.repeat(32),
            kind: 'asset',
            size: 3,
            mime: 'application/current',
            name: 'Current',
            ext: 'current',
        }
        const pinnedAlias: AssetAlias = {
            ...currentAlias,
            objectHash: 'bb'.repeat(32),
            size: 2,
            mime: 'application/pinned',
            name: 'Pinned',
            ext: 'pinned',
        }
        const zeroAlias: AssetAlias = {
            ...currentAlias,
            key: zeroKey,
            objectHash: 'cc'.repeat(32),
            size: 0,
        }
        const missingPayloadAlias: AssetAlias = {
            ...currentAlias,
            key: missingPayloadKey,
            objectHash: null,
            size: 0,
        }
        const reader = (
            revision: number,
            aliases: Record<string, AssetAlias>,
        ) => ({
            revision,
            readAssetAlias: vi.fn(async (logicalKey: string) => {
                const value = aliases[logicalKey]
                return value ? { revision, value: structuredClone(value) } : null
            }),
        }) as unknown as PersistentRevisionReader
        const casBytes = new Map([
            [currentAlias.objectHash!, new Uint8Array([1, 2, 3])],
            [pinnedAlias.objectHash!, new Uint8Array([4, 5])],
            [zeroAlias.objectHash!, new Uint8Array()],
        ])
        const cas = {
            readObject: vi.fn(async (hash: string) => casBytes.get(hash)?.slice() ?? null),
            statObject: vi.fn(async (hash: string) => casBytes.get(hash)?.byteLength ?? null),
        } as unknown as ImmutablePayloadCas
        const legacy = {
            read: vi.fn(async () => { throw new Error('legacy read must not run') }),
            stat: vi.fn(async () => { throw new Error('legacy stat must not run') }),
        } as unknown as BlobStore
        const current = createAssetRepository({
            reader: reader(9, {
                [key]: currentAlias,
                [zeroKey]: zeroAlias,
                [missingPayloadKey]: missingPayloadAlias,
            }),
            cas,
            legacy,
            legacyFallback: false,
        })
        const pinned = createAssetRepository({
            reader: reader(8, { [key]: pinnedAlias }),
            cas,
            legacy,
            legacyFallback: false,
        })

        await expect(current.read(key, { start: 1, endExclusive: 3 })).resolves.toEqual({
            revision: 9,
            value: {
                alias: currentAlias,
                data: new Uint8Array([2, 3]),
                source: 'cas',
            },
        })
        await expect(pinned.read(key)).resolves.toEqual({
            revision: 8,
            value: {
                alias: pinnedAlias,
                data: new Uint8Array([4, 5]),
                source: 'cas',
            },
        })
        await expect(current.stat(zeroKey)).resolves.toEqual({
            revision: 9,
            value: { alias: zeroAlias, objectSize: 0, source: 'cas' },
        })
        await expect(current.stat(missingPayloadKey)).resolves.toEqual({
            revision: 9,
            value: { alias: missingPayloadAlias, objectSize: null, source: 'missing' },
        })
        await expect(current.stat('assets/no-alias.bin')).resolves.toBeNull()
        expect(legacy.read).not.toHaveBeenCalled()
        expect(legacy.stat).not.toHaveBeenCalled()
    })

    it('uses only explicit verified legacy fallback and preserves missing-alias semantics', async () => {
        const validKey = 'assets/legacy-empty.bin'
        const corruptKey = 'assets/legacy-corrupt.bin'
        const nullHashKey = 'assets/legacy-null-hash.bin'
        const aliases: Record<string, AssetAlias> = Object.fromEntries(
            [validKey, corruptKey].map((key) => [key, {
                key,
                objectHash: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
                kind: 'asset' as const,
                size: 0,
                mime: 'application/octet-stream',
                name: key,
                ext: 'bin',
            }]),
        )
        aliases[nullHashKey] = {
            ...aliases[validKey],
            key: nullHashKey,
            objectHash: null,
        }
        aliases[corruptKey] = { ...aliases[corruptKey], size: 1 }
        const reader = {
            readAssetAlias: vi.fn(async (key: string) => {
                const value = aliases[key]
                return value ? { revision: 4, value } : null
            }),
        } as unknown as PersistentDataStore
        const cas = {
            readObject: vi.fn(async () => null),
            statObject: vi.fn(async () => null),
        } as unknown as ImmutablePayloadCas
        const legacy = {
            read: vi.fn(async (key: string) => {
                if (key === validKey) return new Uint8Array()
                if (key === corruptKey) return new Uint8Array([9])
                if (key === nullHashKey) return new Uint8Array([7])
                if (key === 'assets/legacy-only.bin') return new Uint8Array([1])
                return null
            }),
            stat: vi.fn(async (key: string) => {
                if (key === validKey) return { ...aliases[validKey], size: 0 }
                if (key === corruptKey) return { ...aliases[corruptKey], size: 2 }
                return null
            }),
        } as unknown as BlobStore
        const repository = createAssetRepository({
            reader,
            cas,
            legacy,
            legacyFallback: true,
        })
        const noFallback = createAssetRepository({
            reader,
            cas,
            legacy,
            legacyFallback: false,
        })

        await expect(repository.read(validKey)).resolves.toEqual({
            revision: 4,
            value: { alias: aliases[validKey], data: new Uint8Array(), source: 'legacy' },
        })
        await expect(repository.read(corruptKey)).rejects.toThrow('hash mismatch')
        await expect(repository.read(nullHashKey)).rejects.toThrow('size mismatch')
        await expect(repository.stat(validKey)).resolves.toEqual({
            revision: 4,
            value: { alias: aliases[validKey], objectSize: 0, source: 'legacy' },
        })
        await expect(repository.stat(corruptKey)).rejects.toThrow('size mismatch')
        await expect(repository.read('assets/legacy-only.bin')).resolves.toBeNull()
        await expect(noFallback.read(validKey)).resolves.toEqual({
            revision: 4,
            value: { alias: aliases[validKey], data: null, source: 'missing' },
        })
    })

    it('keeps old alias bytes readable through a real IndexedDB revision lease', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'asset-repository-pinned-bytes',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
        const objects = new Map<string, Uint8Array>()
        const cas = createImmutablePayloadCas({
            async putIfAbsent(key, data) {
                if (objects.has(key)) return false
                objects.set(key, data.slice())
                return true
            },
            async read(key) {
                return objects.get(key)?.slice() ?? null
            },
            async stat(key) {
                return objects.get(key)?.byteLength ?? null
            },
        })
        const oldBytes = new Uint8Array([1, 2])
        const newBytes = new Uint8Array([3, 4, 5])
        const oldObject = await cas.prepare(oldBytes)
        const oldAlias: AssetAlias = {
            key: 'assets/lease.bin',
            objectHash: oldObject.contentHash,
            kind: 'asset',
            size: oldBytes.byteLength,
            mime: 'application/old',
            name: 'Old',
            ext: 'old',
        }
        const first = await store.commitAssetAlias(oldAlias, imported.revision)
        const lease = await store.acquireRevision(first.revision)
        const newObject = await cas.prepare(newBytes)
        const newAlias: AssetAlias = {
            ...oldAlias,
            objectHash: newObject.contentHash,
            size: newBytes.byteLength,
            mime: 'application/new',
            name: 'New',
            ext: 'new',
        }
        await store.commitAssetAlias(newAlias, first.revision)
        const legacy = {
            read: vi.fn(async () => null),
            stat: vi.fn(async () => null),
        } as unknown as BlobStore
        const current = createAssetRepository({
            reader: store,
            cas,
            legacy,
            legacyFallback: false,
        })
        const pinned = createAssetRepository({
            reader: lease,
            cas,
            legacy,
            legacyFallback: false,
        })

        expect((await current.read(oldAlias.key))?.value).toEqual({
            alias: newAlias,
            data: newBytes,
            source: 'cas',
        })
        expect((await pinned.read(oldAlias.key))?.value).toEqual({
            alias: oldAlias,
            data: oldBytes,
            source: 'cas',
        })
        await lease.release()
    })

    it('leaves only an unreferenced object when the alias commit fails', async () => {
        const objects = new Map<string, Uint8Array>()
        const cas = createImmutablePayloadCas({
            async putIfAbsent(key, data) {
                objects.set(key, data.slice())
                return true
            },
            async read(key) {
                return objects.get(key)?.slice() ?? null
            },
            async stat(key) {
                return objects.get(key)?.byteLength ?? null
            },
        })
        const aliasError = new Error('injected alias commit failure')
        const store = {
            readRoot: vi.fn(async () => ({ revision: 3, value: {} })),
            commitAssetAlias: vi.fn(async () => { throw aliasError }),
        } as unknown as PersistentDataStore
        const legacy = { put: vi.fn() } as unknown as BlobStore
        const facade = createAssetRepositoryBlobStore({
            store,
            cas,
            legacy,
            legacyFallback: false,
            removal: 'disabled',
        })

        await expect(facade.put('assets/fails.bin', new Uint8Array([6]), {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'Fails',
            ext: 'bin',
        })).rejects.toBe(aliasError)

        expect(objects.size).toBe(1)
        expect(store.commitAssetAlias).toHaveBeenCalledWith(expect.objectContaining({
            key: 'assets/fails.bin',
            size: 1,
        }), 3)
        expect(legacy.put).not.toHaveBeenCalled()
    })
})
