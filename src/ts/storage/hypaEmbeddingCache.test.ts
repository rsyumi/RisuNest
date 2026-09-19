import { describe, expect, it, vi } from 'vitest'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import {
    consistentEmbeddings,
    createLocalHypaEmbeddingCache,
    createNativeHypaEmbeddingCache,
    decodeHypaReadFrame,
    encodeHypaWriteFrame,
    resilientHypaEmbeddingCache,
    staleEmbeddingKeys,
    toVectorBuffer,
    type HypaCachedEmbedding,
    type HypaEmbeddingEntry,
} from './hypaEmbeddingCache'

function entry(key: string, values: number[]): HypaEmbeddingEntry {
    return {
        key,
        producer: 'hypa-v2',
        model: 'MiniLM',
        endpoint: null,
        preprocessVersion: 1,
        dimensions: values.length,
        vector: toVectorBuffer(values),
    }
}

/** Mirrors the native read frame so a fake invoke can answer one. */
function readFrame(entries: { key: string; values: number[] | null }[]): Uint8Array {
    const header = {
        entries: entries.map(({ key, values }) => ({
            key,
            dimensions: values?.length ?? 0,
            byteLength: (values?.length ?? 0) * 4,
        })),
    }
    const encoded = new TextEncoder().encode(JSON.stringify(header))
    const body = entries.flatMap(({ values }) => values ?? [])
    const vectors = new Uint8Array(Float32Array.from(body).buffer)
    const frame = new Uint8Array(4 + encoded.length + vectors.length)
    new DataView(frame.buffer).setUint32(0, encoded.length, true)
    frame.set(encoded, 4)
    frame.set(vectors, 4 + encoded.length)
    return frame
}

describe('hypa embedding frames', () => {
    it('carries vectors as Float32 little endian in header order', () => {
        const frame = encodeHypaWriteFrame([entry('a', [1, 2]), entry('b', [3])])
        const headerLength = new DataView(frame.buffer).getUint32(0, true)
        const header = JSON.parse(
            new TextDecoder().decode(frame.subarray(4, 4 + headerLength)),
        )
        expect(header.entries.map((item: { key: string }) => item.key)).toEqual(['a', 'b'])
        expect(header.entries[0].byteLength).toBe(8)
        const body = frame.subarray(4 + headerLength)
        expect(body.byteLength).toBe(12)
        expect(Array.from(new Float32Array(body.slice().buffer))).toEqual([1, 2, 3])
    })

    it('rejects a vector whose length contradicts its dimensions', () => {
        const broken = { ...entry('a', [1, 2]), dimensions: 3 }
        expect(() => encodeHypaWriteFrame([broken])).toThrow(RangeError)
    })

    it('decodes misses as absent rather than as empty vectors', () => {
        const decoded = decodeHypaReadFrame(
            readFrame([
                { key: 'a', values: [1, 2] },
                { key: 'missing', values: null },
            ]),
        )
        expect(decoded.has('missing')).toBe(false)
        expect(Array.from(decoded.get('a').vector)).toEqual([1, 2])
        expect(decoded.get('a').dimensions).toBe(2)
    })

    it('rejects a truncated frame', () => {
        const frame = readFrame([{ key: 'a', values: [1, 2] }])
        expect(() => decodeHypaReadFrame(frame.subarray(0, frame.length - 4))).toThrow(TypeError)
    })
})

describe('native hypa embedding cache', () => {
    it.each([false, true])('preserves every vector in smaller low-spec batches (Android: %s)', async (android) => {
        setRuntimePerformanceProfile('low-spec')
        try {
            const stored = new Map<string, HypaCachedEmbedding>()
            const sizes: number[] = []
            const invoke = vi.fn(async (command: string, args: any) => {
                if (command === 'pds_write_hypa_embeddings') {
                    const decoded = decodeHypaReadFrame(android ? Buffer.from(args.payload, 'base64') : args)
                    sizes.push(decoded.size)
                    for (const [key, value] of decoded) stored.set(key, value)
                    return
                }
                sizes.push(args.keys.length)
                return readFrame(args.keys.map((key: string) => ({ key, values: Array.from(stored.get(key)!.vector) })))
            })
            const cache = createNativeHypaEmbeddingCache(invoke as never, android)
            const entries = Array.from({ length: 137 }, (_, i) => entry(`key-${i}`, [i, -i]))
            await cache.write(entries)
            const hits = await cache.read(entries.map((item) => item.key))
            expect(sizes).toEqual([64, 64, 9, 64, 64, 9])
            expect([...hits.keys()]).toEqual(entries.map((item) => item.key))
            for (let i = 0; i < entries.length; i++) expect(hits.get(`key-${i}`)!.vector).toEqual(new Float32Array([i, -i]))
        } finally {
            setRuntimePerformanceProfile('normal')
        }
    })

    it('reads a whole batch in one call', async () => {
        const invoke = vi.fn(async (_command: string, _args: unknown) => readFrame([
            { key: 'a', values: [1, 2] },
            { key: 'b', values: null },
            { key: 'c', values: [3, 4] },
        ]).buffer)
        const cache = createNativeHypaEmbeddingCache(invoke as never, false)

        const hits = await cache.read(['a', 'b', 'c'])
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(invoke.mock.calls[0]).toEqual(['pds_read_hypa_embeddings', { keys: ['a', 'b', 'c'] }])
        expect([...hits.keys()]).toEqual(['a', 'c'])
    })

    it('accepts a number array response where raw IPC responses are unavailable', async () => {
        const invoke = vi.fn(async (_command: string, _args: unknown) =>
            Array.from(readFrame([{ key: 'a', values: [5] }])),
        )
        const cache = createNativeHypaEmbeddingCache(invoke as never, false)
        expect(Array.from((await cache.read(['a'])).get('a').vector)).toEqual([5])
    })

    it('writes a whole batch in one call as a raw body', async () => {
        const invoke = vi.fn(async (_command: string, _args: unknown) => undefined)
        const cache = createNativeHypaEmbeddingCache(invoke as never, false)

        await cache.write([entry('a', [1, 2]), entry('b', [3, 4])])
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(invoke.mock.calls[0][0]).toBe('pds_write_hypa_embeddings')
        expect(invoke.mock.calls[0][1]).toBeInstanceOf(Uint8Array)
    })

    it('sends the same frame as base64 on Android', async () => {
        const invoke = vi.fn(async (_command: string, _args: unknown) => undefined)
        const cache = createNativeHypaEmbeddingCache(invoke as never, true)

        await cache.write([entry('a', [1, 2])])
        const payload = (invoke.mock.calls[0][1] as { payload: string }).payload
        expect(Buffer.from(payload, 'base64')).toEqual(
            Buffer.from(encodeHypaWriteFrame([entry('a', [1, 2])])),
        )
    })

    it('splits an oversized batch instead of calling once per key', async () => {
        const keys = Array.from({ length: 2500 }, (_, index) => `key-${index}`)
        const invoke = vi.fn(async (_command: string, args: { keys: string[] }) =>
            readFrame(args.keys.map((key) => ({ key, values: [1] }))).buffer,
        )
        const cache = createNativeHypaEmbeddingCache(invoke as never, false)

        expect((await cache.read(keys)).size).toBe(2500)
        expect(invoke).toHaveBeenCalledTimes(3)
    })
})

describe('local hypa embedding cache', () => {
    it('round trips vectors under the normalized key', async () => {
        const store = new Map<string, unknown>()
        const forage = {
            async getItem(key: string) {
                return store.get(key) ?? null
            },
            async setItem(key: string, value: unknown) {
                store.set(key, value)
                return value
            },
        } as unknown as LocalForage
        const cache = createLocalHypaEmbeddingCache(forage)

        await cache.write([entry('a', [1, 2])])
        const hits = await cache.read(['a', 'missing'])
        expect(hits.size).toBe(1)
        expect(Array.from(hits.get('a').vector)).toEqual([1, 2])
    })

    it('treats a stored value from another key scheme as a miss', async () => {
        const forage = {
            async getItem() {
                return { content: 'text', embedding: [1, 2] }
            },
            async setItem(_key: string, value: unknown) {
                return value
            },
        } as unknown as LocalForage
        expect((await createLocalHypaEmbeddingCache(forage).read(['a'])).size).toBe(0)
    })
})

describe('dimension agreement', () => {
    const hits = (widths: number[]): Map<string, HypaCachedEmbedding> =>
        new Map(
            widths.map((width, index) => [
                `key-${index}`,
                { vector: new Float32Array(width), dimensions: width },
            ]),
        )

    it('keeps hits that agree and drops every hit when they do not', () => {
        expect(consistentEmbeddings(hits([3, 3])).size).toBe(2)
        expect(consistentEmbeddings(hits([3, 4])).size).toBe(0)
    })

    it('names the hits that disagree with a freshly computed width', () => {
        expect(staleEmbeddingKeys(hits([3, 3]), 3)).toEqual([])
        expect(staleEmbeddingKeys(hits([3, 3]), 4)).toEqual(['key-0', 'key-1'])
        expect(staleEmbeddingKeys(hits([3]), 0)).toEqual([])
    })
})

describe('cache availability', () => {
    const unavailable = {
        async read(): Promise<never> {
            throw new Error('device store is unavailable')
        },
        async write(): Promise<never> {
            throw new Error('device store is unavailable')
        },
    }

    it('answers an unavailable store as a full miss instead of failing the round', async () => {
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        const cache = resilientHypaEmbeddingCache(unavailable)

        expect((await cache.read(['a', 'b'])).size).toBe(0)
        await expect(cache.write([entry('a', [1, 2])])).resolves.toBeUndefined()
        expect(warn).toHaveBeenCalledTimes(2)
        warn.mockRestore()
    })

    it('passes a working store straight through', async () => {
        const cache = resilientHypaEmbeddingCache(
            createNativeHypaEmbeddingCache(
                (async () => readFrame([{ key: 'a', values: [1] }]).buffer) as never,
                false,
            ),
        )
        expect((await cache.read(['a'])).size).toBe(1)
    })
})
