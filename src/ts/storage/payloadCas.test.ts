import { describe, expect, test, vi } from 'vitest'
import {
    createBlobKeyValuePayloadBackend,
    createImmutablePayloadCas,
    createProcessLocalPayloadKeyLock,
    createShadowCopyBlobStore,
    type ImmutablePayloadBackend,
} from './payloadCas'
import { createKeyValueBlobStore, type BlobKeyValueBackend } from './blobStore'

class MemoryImmutablePayloadBackend implements ImmutablePayloadBackend {
    readonly values = new Map<string, Uint8Array>()

    async putIfAbsent(key: string, data: Uint8Array): Promise<boolean> {
        if (this.values.has(key)) return false
        this.values.set(key, data.slice())
        return true
    }

    async read(key: string): Promise<Uint8Array | null> {
        return this.values.get(key)?.slice() ?? null
    }

    async readRange(
        key: string,
        range: { start: number; endExclusive: number },
    ): Promise<Uint8Array | null> {
        return this.values.get(key)?.slice(range.start, range.endExclusive) ?? null
    }

    async stat(key: string): Promise<number | null> {
        return this.values.get(key)?.byteLength ?? null
    }
}

function createLegacyBackend(): BlobKeyValueBackend {
    const values = new Map<string, Uint8Array>()
    return {
        async write(key, value) {
            values.set(key, value.slice())
        },
        async read(key) {
            return values.get(key)?.slice() ?? null
        },
        async keys() {
            return [...values.keys()]
        },
        async remove(key) {
            values.delete(key)
        },
        async size(key) {
            return values.get(key)?.byteLength ?? null
        },
    }
}

describe('immutable payload CAS', () => {
    test('stores exact bytes under the lowercase SHA-256 shard key', async () => {
        const backend = new MemoryImmutablePayloadBackend()
        const cas = createImmutablePayloadCas(backend)

        const prepared = await cas.prepare(new TextEncoder().encode('abc'))

        expect(prepared).toEqual({
            contentHash: 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad',
            byteSize: 3,
            physicalKey: 'assets-v2/objects/ba/7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad',
            deduplicated: false,
        })
        expect(backend.values.get(prepared.physicalKey)).toEqual(
            new TextEncoder().encode('abc'),
        )
    })

    test('rejects an existing key whose bytes do not match its claimed hash', async () => {
        const backend = new MemoryImmutablePayloadBackend()
        const cas = createImmutablePayloadCas(backend)
        const first = await cas.prepare(new TextEncoder().encode('abc'))
        backend.values.set(first.physicalKey, new TextEncoder().encode('abd'))

        await expect(cas.prepare(new TextEncoder().encode('abc'))).rejects.toThrow(
            'collision or corruption',
        )
        expect(backend.values.get(first.physicalKey)).toEqual(new TextEncoder().encode('abd'))
    })

    test('preserves zero-byte payloads while deduplicating their physical object', async () => {
        const backend = new MemoryImmutablePayloadBackend()
        const cas = createImmutablePayloadCas(backend)

        const first = await cas.prepare(new Uint8Array())
        const second = await cas.prepare(new Uint8Array())

        expect(first.contentHash).toBe(
            'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
        )
        expect(first.byteSize).toBe(0)
        expect(first.deduplicated).toBe(false)
        expect(second).toEqual({ ...first, deduplicated: true })
        expect(backend.values).toHaveLength(1)
    })

    test('shadow copies before legacy writes without changing read or delete authority', async () => {
        const authoritative = createKeyValueBlobStore(
            createLegacyBackend(),
            { kind: 'legacy' },
        )
        const backend = new MemoryImmutablePayloadBackend()
        const cas = createImmutablePayloadCas(backend)
        const observations: { contentHash: string; deduplicated: boolean }[] = []
        const store = createShadowCopyBlobStore(authoritative, cas, (prepared) => {
            observations.push({
                contentHash: prepared.contentHash,
                deduplicated: prepared.deduplicated,
            })
        })
        const bytes = new Uint8Array([4, 2])

        await store.put('assets/first.png', bytes, {
            kind: 'asset',
            mime: 'image/png',
            name: 'first',
            ext: 'png',
        })
        await store.put('assets/second.bin', bytes, {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'second',
            ext: 'bin',
        })
        await store.remove('assets/first.png')

        expect(await store.read('assets/first.png')).toBeNull()
        expect(await store.read('assets/second.bin')).toEqual(bytes)
        expect(await store.stat('assets/second.bin')).toMatchObject({
            key: 'assets/second.bin',
            name: 'second',
            ext: 'bin',
            mime: 'application/octet-stream',
        })
        expect(backend.values).toHaveLength(1)
        expect(observations).toEqual([
            {
                contentHash: 'b7586d310e5efb1b7d10a917ba5af403adbf54f4f77fe7fdcb4880a95dac7e7e',
                deduplicated: false,
            },
            {
                contentHash: 'b7586d310e5efb1b7d10a917ba5af403adbf54f4f77fe7fdcb4880a95dac7e7e',
                deduplicated: true,
            },
        ])
    })

    test('shadow and authoritative writes retain the same input snapshot', async () => {
        const authoritative = createKeyValueBlobStore(
            createLegacyBackend(),
            { kind: 'legacy' },
        )
        const backend = new MemoryImmutablePayloadBackend()
        const store = createShadowCopyBlobStore(
            authoritative,
            createImmutablePayloadCas(backend),
        )
        const input = new Uint8Array([1])

        const write = store.put('assets/snapshot.bin', input, {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'snapshot',
            ext: 'bin',
        })
        input[0] = 9
        await write

        expect(await store.read('assets/snapshot.bin')).toEqual(
            new Uint8Array([1]),
        )
        expect([...backend.values.values()]).toEqual([new Uint8Array([1])])
    })

    test('direct read and stat validate one exact hash path without listing objects', async () => {
        const backend = new MemoryImmutablePayloadBackend()
        const cas = createImmutablePayloadCas(backend)
        const prepared = await cas.prepare(new TextEncoder().encode('lookup'))
        const missing =
            '0000000000000000000000000000000000000000000000000000000000000000'

        await expect(cas.statObject(prepared.contentHash)).resolves.toBe(6)
        await expect(cas.readObject(prepared.contentHash)).resolves.toEqual(
            new TextEncoder().encode('lookup'),
        )
        await expect(cas.statObject(missing)).resolves.toBeNull()
        await expect(cas.readObject(missing)).resolves.toBeNull()
        await expect(cas.statObject('../objects')).rejects.toThrow(
            '64 lowercase hexadecimal',
        )
    })

    test('reads a validated object range without materializing the complete payload', async () => {
        const backend = new MemoryImmutablePayloadBackend()
        const cas = createImmutablePayloadCas(backend)
        const prepared = await cas.prepare(new Uint8Array([0, 1, 2, 3, 4, 5]))
        const fullRead = vi.spyOn(backend, 'read').mockRejectedValue(
            new Error('complete payload read is forbidden'),
        )

        await expect(cas.readObjectRange(prepared.contentHash, {
            start: 2,
            endExclusive: 5,
        })).resolves.toEqual(new Uint8Array([2, 3, 4]))
        expect(fullRead).not.toHaveBeenCalled()
        await expect(cas.readObjectRange(prepared.contentHash, {
            start: 5,
            endExclusive: 4,
        })).rejects.toThrow('ascending order')
    })

    test('BlobStore backends publish immutable keys without enumerating their tree', async () => {
        const values = new Map<string, Uint8Array>()
        let writes = 0
        const blobBackend: BlobKeyValueBackend = {
            async write(key, value) {
                writes += 1
                values.set(key, value.slice())
            },
            async read(key) {
                return values.get(key)?.slice() ?? null
            },
            async size(key) {
                return values.get(key)?.byteLength ?? null
            },
            async keys() {
                throw new Error('object enumeration is forbidden')
            },
            async remove() {},
        }
        const lock = createProcessLocalPayloadKeyLock()
        const cas = createImmutablePayloadCas(
            createBlobKeyValuePayloadBackend(blobBackend, lock),
        )

        const first = await cas.prepare(new Uint8Array([7]))
        const second = await cas.prepare(new Uint8Array([7]))

        expect(first.deduplicated).toBe(false)
        expect(second.deduplicated).toBe(true)
        expect(writes).toBe(1)
        expect(values).toHaveLength(1)
    })

    test('a shared process lock coordinates concurrent prepares across adapter callers', async () => {
        const values = new Map<string, Uint8Array>()
        let writes = 0
        const blobBackend: BlobKeyValueBackend = {
            async write(key, value) {
                writes += 1
                values.set(key, value.slice())
            },
            async read(key) {
                return values.get(key)?.slice() ?? null
            },
            async size(key) {
                const size = values.get(key)?.byteLength ?? null
                await Promise.resolve()
                return size
            },
            async keys() {
                throw new Error('object enumeration is forbidden')
            },
            async remove() {},
        }
        const lock = createProcessLocalPayloadKeyLock()
        const firstCas = createImmutablePayloadCas(
            createBlobKeyValuePayloadBackend(blobBackend, lock),
        )
        const secondCas = createImmutablePayloadCas(
            createBlobKeyValuePayloadBackend(blobBackend, lock),
        )

        const prepared = await Promise.all([
            firstCas.prepare(new Uint8Array([8, 9])),
            secondCas.prepare(new Uint8Array([8, 9])),
        ])

        expect(lock.scope).toBe('process-local')
        expect(prepared.filter((result) => !result.deduplicated)).toHaveLength(1)
        expect(writes).toBe(1)
        expect(values).toHaveLength(1)
    })
})
