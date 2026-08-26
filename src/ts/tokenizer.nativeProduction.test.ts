import { beforeEach, describe, expect, it, vi } from 'vitest'

const harness = vi.hoisted(() => ({
    nativeBatch: vi.fn(),
}))

vi.mock('./tokenizer/nativeTokenizerProduction', () => ({
    tryNativeTokenizerIdsBatch: harness.nativeBatch,
}))

vi.mock('./storage/database.svelte', () => ({
    getCurrentCharacter: vi.fn(),
    getDatabase: () => ({
        aiModel: 'gpt-4',
        currentPluginProvider: '',
        customTokenizer: 'tik',
        googleClaudeTokenizing: false,
        useTokenizerCaching: false,
    }),
}))

vi.mock('./process/files/inlays', () => ({ supportsInlayImage: () => false }))
vi.mock('./parser/parser.svelte', () => ({ risuChatParser: (text: string) => text }))
vi.mock('./process/models/local', () => ({ tokenizeGGUFModel: vi.fn() }))
vi.mock('./globalApi.svelte', () => ({ globalFetch: vi.fn() }))
vi.mock('./model/modellist', () => ({
    getModelInfo: () => ({ tokenizer: 1 }),
    LLMTokenizer: {
        Unknown: 0,
        tiktokenCl100kBase: 1,
        tiktokenO200Base: 2,
        NovelList: 7,
        Claude: 6,
        NovelAI: 5,
        Mistral: 3,
        Llama: 4,
        Local: 12,
        GoogleCloud: 10,
        Gemma: 9,
        DeepSeek: 13,
        DeepSeekV4: 14,
        GLM4: 15,
        GLM5: 16,
        Cohere: 11,
    },
}))
vi.mock('./plugins/plugins.svelte', () => ({
    pluginV2: { providerOptions: new Map() },
}))

import { strongBan } from './tokenizer'

describe('strong ban native batch routing', () => {
    beforeEach(() => {
        localStorage.clear()
        harness.nativeBatch.mockReset()
    })

    it('uses one large native IDs batch and applies every ordered result', async () => {
        harness.nativeBatch.mockImplementation(async (candidate) => {
            const texts = candidate.buildTexts()
            expect(candidate.itemCount).toBe(texts.length)
            expect(candidate.aggregateInputBytes()).toBe(
                texts.reduce(
                    (total: number, text: string) =>
                        total + new TextEncoder().encode(text).byteLength,
                    0,
                ),
            )
            return texts.map((_: string, index: number) => [10_000 + index])
        })
        const bias = { 42: -5 }

        const result = await strongBan('target', bias)

        expect(harness.nativeBatch).toHaveBeenCalledTimes(1)
        const [candidate, context] = harness.nativeBatch.mock.calls[0]
        const texts = candidate.buildTexts()
        expect(candidate.itemCount).toBeGreaterThanOrEqual(100)
        expect(context).toEqual({
            isTauri: expect.any(Boolean),
            aiModel: 'gpt-4',
            customTokenizer: 'tik',
            modelTokenizerId: 'cl100k_base',
            pluginTokenizer: undefined,
        })
        const repeatedFirstInput = texts.findIndex(
            (text: string, index: number) => index > 0 && text === texts[0],
        )
        expect(repeatedFirstInput).toBeGreaterThan(0)
        expect(Object.keys(result)).toHaveLength(1 + texts.length - repeatedFirstInput)
        expect(result[10_000 + repeatedFirstInput]).toBe(-100)
        expect(result[42]).toBe(-5)
    })

    it('falls back to the JavaScript result when the native boundary fails', async () => {
        harness.nativeBatch.mockResolvedValueOnce(null)
        const expected = await strongBan('fallback-target', { 42: -5 })
        localStorage.clear()
        harness.nativeBatch.mockRejectedValueOnce(new Error('native boundary failed'))

        const actual = await strongBan('fallback-target', { 42: -5 })

        expect(actual).toEqual(expected)
    })

    it('preserves the JavaScript error and partial bias mutation on native failure', async () => {
        const input = '<|ENDOFTEXT|>'
        const baselineBias: Record<number, number> = {}
        harness.nativeBatch.mockResolvedValueOnce(null)
        let baselineError: unknown
        try {
            await strongBan(input, baselineBias)
        } catch (error) {
            baselineError = error
        }
        expect(Object.keys(baselineBias).length).toBeGreaterThan(0)
        localStorage.clear()
        const fallbackBias: Record<number, number> = {}
        harness.nativeBatch.mockRejectedValueOnce(new Error('native boundary failed'))
        let fallbackError: unknown
        try {
            await strongBan(input, fallbackBias)
        } catch (error) {
            fallbackError = error
        }

        expect(fallbackError).toEqual(baselineError)
        expect(fallbackBias).toEqual(baselineBias)
    })
})
