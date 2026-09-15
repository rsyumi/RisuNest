import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const state = vi.hoisted(() => ({
    database: { hypaModel: 'MiniLM', hypaCustomSettings: { url: '', key: '', model: '' } },
    embedded: [] as string[][],
    width: 4,
    cache: null as unknown,
}))

vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => state.database,
}))
vi.mock('src/ts/globalApi.svelte', () => ({
    globalFetch: vi.fn(async (_url: string, options: { body: { input: string[] } }) => {
        state.embedded.push([...options.body.input])
        return {
            ok: true,
            data: {
                data: options.body.input.map(() => ({
                    embedding: Array.from({ length: state.width }, () => 1),
                })),
            },
        }
    }),
}))
// Cut the module cycle that reaches back into this module through the app root.
vi.mock('src/ts/util', () => ({
    appendLastPath: (url: string, path: string) => `${url}/${path}`,
}))
vi.mock('../transformers', () => ({
    runEmbedding: vi.fn(async (inputs: string[]) => {
        state.embedded.push([...inputs])
        return inputs.map(() => new Float32Array(state.width).fill(1))
    }),
}))

vi.mock('src/ts/storage/hypaEmbeddingCache', async (importOriginal) => ({
    ...(await importOriginal<typeof import('src/ts/storage/hypaEmbeddingCache')>()),
    getHypaEmbeddingCache: () => state.cache,
}))

import { HypaProcesser } from './hypamemory'
import { HypaProcessorV2 } from './hypamemoryv2'
import type {
    HypaCachedEmbedding,
    HypaEmbeddingCache,
    HypaEmbeddingEntry,
} from 'src/ts/storage/hypaEmbeddingCache'

function recordingCache(seed: Map<string, HypaCachedEmbedding> = new Map()) {
    const stored = new Map(seed)
    const reads: string[][] = []
    const writes: HypaEmbeddingEntry[][] = []
    const cache: HypaEmbeddingCache = {
        async read(keys: string[]) {
            reads.push([...keys])
            return new Map([...stored].filter(([key]) => keys.includes(key)))
        },
        async write(entries: HypaEmbeddingEntry[]) {
            writes.push([...entries])
            for (const entry of entries) {
                stored.set(entry.key, {
                    vector: new Float32Array(entry.vector),
                    dimensions: entry.dimensions,
                })
            }
        },
    }
    return { reads, writes, stored, cache }
}

const texts = Array.from({ length: 40 }, (_, index) => `chunk ${index}`)

beforeEach(() => {
    state.embedded = []
    state.width = 4
    state.database = { hypaModel: 'MiniLM', hypaCustomSettings: { url: '', key: '', model: '' } }
})

afterEach(() => {
    state.cache = null
    vi.clearAllMocks()
})

describe('HypaProcesser.addText', () => {
    it('reads and writes the whole batch once instead of once per text', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        const processer = new HypaProcesser('MiniLM')
        await processer.addText(texts)

        expect(recorder.reads).toHaveLength(1)
        expect(recorder.reads[0]).toHaveLength(texts.length)
        expect(recorder.writes).toHaveLength(1)
        expect(recorder.writes[0]).toHaveLength(texts.length)
        expect(recorder.writes[0][0].producer).toBe('hypa-v1-text')
        expect(recorder.writes[0][0].model).toBe('MiniLM')
        expect(recorder.writes[0][0].endpoint).toBeNull()
        expect(processer.vectors).toHaveLength(texts.length)
    })

    it('embeds nothing on a second pass that the cache already answers', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        await new HypaProcesser('MiniLM').addText(texts)
        state.embedded = []
        const second = new HypaProcesser('MiniLM')
        await second.addText(texts)

        expect(state.embedded).toHaveLength(0)
        expect(second.vectors).toHaveLength(texts.length)
        expect(recorder.writes).toHaveLength(1)
    })

    it('recomputes cached vectors that disagree with the freshly computed width', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        await new HypaProcesser('MiniLM').addText(texts.slice(0, 20))
        state.width = 6
        state.embedded = []

        const processer = new HypaProcesser('MiniLM')
        await processer.addText(texts)

        expect(processer.vectors).toHaveLength(texts.length)
        for (const vector of processer.vectors) {
            expect(vector.embedding).toHaveLength(6)
        }
        expect(state.embedded.flat().sort()).toEqual([...texts].sort())
    })
})

describe('HypaProcessorV2.addTexts', () => {
    const items = texts.map((content, index) => ({ id: `item-${index}`, content }))

    it('reads and writes the whole batch once instead of once per chunk', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        const processor = new HypaProcessorV2<string>()
        await processor.addTexts(items)

        expect(recorder.reads).toHaveLength(1)
        expect(recorder.reads[0]).toHaveLength(items.length)
        expect(recorder.writes).toHaveLength(1)
        expect(recorder.writes[0]).toHaveLength(items.length)
        expect(recorder.writes[0][0].producer).toBe('hypa-v2')
        expect(processor.vectors.size).toBe(items.length)
    })

    it('answers a repeat round from the cache and keeps the caller identity', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        await new HypaProcessorV2<string>().addTexts(items)
        state.embedded = []
        const processor = new HypaProcessorV2<string>()
        await processor.addTexts(items)

        expect(state.embedded).toHaveLength(0)
        expect(processor.vectors.get('item-3').id).toBe('item-3')
        expect(processor.vectors.get('item-3').content).toBe('chunk 3')
    })

    it('recomputes cached vectors that disagree with the freshly computed width', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        await new HypaProcessorV2<string>().addTexts(items.slice(0, 20))
        state.width = 6
        state.embedded = []

        const processor = new HypaProcessorV2<string>()
        await processor.addTexts(items)

        expect(processor.vectors.size).toBe(items.length)
        for (const vector of processor.vectors.values()) {
            expect(vector.embedding).toHaveLength(6)
        }
    })

    it('separates the key from the v1 producer for the same text and model', async () => {
        const first = recordingCache()
        state.cache = first.cache
        await new HypaProcesser('MiniLM').addText([texts[0]])

        const second = recordingCache(first.stored)
        state.cache = second.cache
        state.embedded = []
        await new HypaProcessorV2<string>().addTexts([items[0]])

        expect(state.embedded.flat()).toEqual([texts[0]])
        expect(second.writes[0][0].key).not.toBe(first.writes[0][0].key)
    })
})

describe('embedding identity', () => {
    it('separates two custom servers that answer under one model name', async () => {
        const item = { id: 'item-0', content: texts[0] }
        const requested: string[] = []

        for (const url of ['https://one.example', 'https://two.example']) {
            state.database = {
                hypaModel: 'custom',
                hypaCustomSettings: { url, key: '', model: 'bge-m3' },
            }
            const recorder = recordingCache()
            state.cache = recorder.cache
            await new HypaProcessorV2<string>({ model: 'custom', customEmbeddingUrl: url })
                .addTexts([item])
                .catch(() => undefined)
            requested.push(recorder.reads[0][0])
            expect(recorder.writes[0][0].endpoint).toBe(url)
            expect(recorder.writes[0][0].model).toBe('custom:bge-m3')
        }

        expect(requested[0]).not.toBe(requested[1])
    })
})
