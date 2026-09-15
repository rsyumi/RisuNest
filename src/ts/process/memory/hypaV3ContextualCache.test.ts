import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const state = vi.hoisted(() => ({
    database: { hypaModel: 'voyageContext3', hypaCustomSettings: { url: '', key: '', model: '' } },
    groups: [] as string[][][],
    width: 4,
    cache: null as unknown,
}))

vi.mock('../transformers', () => ({ runEmbedding: vi.fn() }))
// The app graph runs store effects on import that a loaded database would feed.
vi.mock('src/ts/parser/parser.svelte', async () => (await import('../tests/sendChatTestHarness')).parserModule())

vi.mock('./contextualEmbedding', () => ({
    isContextModel: (model: string) => model === 'voyageContext3',
    getContextProvider: () => ({
        modelId: 'voyage-context-3',
        async embedDocumentGroups(groups: string[][]) {
            state.groups.push(groups.map((group) => [...group]))
            return groups.map((group) =>
                group.map(() => new Float32Array(state.width).fill(1)),
            )
        },
        async embedQueries(queries: string[]) {
            return queries.map(() => new Float32Array(state.width))
        },
        getCacheKeySuffix: (contextTexts?: string[]) =>
            `|voyageContext3${contextTexts?.length > 1 ? `|ctx:${contextTexts.join('')}` : ''}`,
    }),
}))

vi.mock('src/ts/storage/hypaEmbeddingCache', async (importOriginal) => ({
    ...(await importOriginal<typeof import('src/ts/storage/hypaEmbeddingCache')>()),
    getHypaEmbeddingCache: () => state.cache,
}))

import { DBState } from 'src/ts/stores.svelte'
import { setDatabaseLite } from 'src/ts/storage/database.svelte'
import { HypaProcesserEx } from './hypav3'
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

function summary(text: string) {
    return { text, chatMemos: new Set<string>(), isImportant: false }
}

function chunksOf(groups: string[][]) {
    return groups.flatMap((texts, index) => {
        const owner = summary(`summary ${index}`)
        return texts.map((text) => ({ text, summary: owner }))
    })
}

const groups = [
    ['alpha one', 'alpha two', 'alpha three'],
    ['beta one', 'beta two'],
]

beforeEach(() => {
    state.groups = []
    state.width = 4
    setDatabaseLite(state.database as never)
    expect(DBState.db.hypaModel).toBe('voyageContext3')
})

afterEach(() => {
    state.cache = null
    vi.clearAllMocks()
})

describe('HypaProcesserEx contextual chunks', () => {
    it('reads and writes every chunk in one call', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache

        const processer = new HypaProcesserEx('voyageContext3')
        await processer.addSummaryChunks(chunksOf(groups))

        expect(recorder.reads).toHaveLength(1)
        expect(recorder.reads[0]).toHaveLength(5)
        expect(recorder.writes).toHaveLength(1)
        expect(recorder.writes[0]).toHaveLength(5)
        expect(recorder.writes[0][0].producer).toBe('hypa-v3-group')
        expect(processer.summaryChunkVectors).toHaveLength(5)
        expect(new Set(recorder.reads[0]).size).toBe(5)
    })

    it('embeds nothing when every group is already cached', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache
        await new HypaProcesserEx('voyageContext3').addSummaryChunks(chunksOf(groups))
        state.groups = []

        const processer = new HypaProcesserEx('voyageContext3')
        await processer.addSummaryChunks(chunksOf(groups))

        expect(state.groups).toHaveLength(0)
        expect(processer.summaryChunkVectors).toHaveLength(5)
    })

    it('re-embeds a whole group when one of its chunks is missing', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache
        await new HypaProcesserEx('voyageContext3').addSummaryChunks(chunksOf(groups))

        // Drop one chunk of the first group, leaving its siblings cached.
        const firstGroupKeys = recorder.writes[0].slice(0, 3).map((entry) => entry.key)
        recorder.stored.delete(firstGroupKeys[1])
        state.groups = []

        await new HypaProcesserEx('voyageContext3').addSummaryChunks(chunksOf(groups))

        expect(state.groups).toEqual([[groups[0]]])
    })

    it('recomputes cached groups that disagree with the freshly computed width', async () => {
        const recorder = recordingCache()
        state.cache = recorder.cache
        await new HypaProcesserEx('voyageContext3').addSummaryChunks(chunksOf(groups))

        // The first group must be recomputed, which changes the vector width.
        recorder.stored.delete(recorder.writes[0][0].key)
        state.width = 6
        state.groups = []

        const processer = new HypaProcesserEx('voyageContext3')
        await processer.addSummaryChunks(chunksOf(groups))

        expect(state.groups.flat()).toEqual([groups[0], groups[1]])
        for (const entry of processer.summaryChunkVectors) {
            expect(entry.vector.embedding).toHaveLength(6)
        }
    })

    it('separates the chunk key from the group it was embedded with', async () => {
        const first = recordingCache()
        state.cache = first.cache
        await new HypaProcesserEx('voyageContext3').addSummaryChunks(chunksOf(groups))

        const second = recordingCache(first.stored)
        state.cache = second.cache
        state.groups = []
        await new HypaProcesserEx('voyageContext3').addSummaryChunks(
            chunksOf([[groups[0][0], 'gamma one', 'gamma two'], groups[1]]),
        )

        expect(state.groups).toEqual([[[groups[0][0], 'gamma one', 'gamma two']]])
    })
})
