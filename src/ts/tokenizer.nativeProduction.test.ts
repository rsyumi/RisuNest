import { beforeEach, describe, expect, it, vi } from 'vitest'

const harness = vi.hoisted(() => ({
    nativeBatch: vi.fn(),
    nativeCountBatch: vi.fn(),
    useTokenizerCaching: false,
    aiModel: 'gpt-4',
    modelTokenizer: 1,
    transformersTokenizer: vi.fn(),
}))

vi.mock('./tokenizer/nativeTokenizerProduction', () => ({
    tryNativeTokenizerIdsBatch: harness.nativeBatch,
    tryNativeTokenizerCountBatch: harness.nativeCountBatch,
}))

vi.mock('./storage/database.svelte', () => ({
    getCurrentCharacter: vi.fn(),
    getDatabase: () => ({
        aiModel: harness.aiModel,
        currentPluginProvider: '',
        customTokenizer: 'tik',
        googleClaudeTokenizing: false,
        useTokenizerCaching: harness.useTokenizerCaching,
    }),
}))

vi.mock('./process/files/inlays', () => ({ supportsInlayImage: () => false }))
vi.mock('./parser/parser.svelte', () => ({
    risuChatParser: (text: string) => text,
}))
vi.mock('./process/transformers', () => ({
    tokenizeTransformers: harness.transformersTokenizer,
}))
vi.mock('./globalApi.svelte', () => ({ globalFetch: vi.fn() }))
vi.mock('./model/modellist', () => ({
    getModelInfo: () => ({ tokenizer: harness.modelTokenizer }),
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

import { ChatTokenizer, encode, strongBan } from './tokenizer'

describe('chat tokenizer native count batching', () => {
    beforeEach(() => {
        harness.nativeCountBatch.mockReset()
        harness.useTokenizerCaching = false
        harness.aiModel = 'gpt-4'
        harness.modelTokenizer = 1
        harness.transformersTokenizer.mockReset()
    })

    it.each([
        'Xenova/opt-350m',
        'Xenova/tiny-random-mistral',
        'Xenova/gpt2-large-conversational',
        'synthetic/custom-model',
    ])(
        'tokenizes the WebLLM model %s without the removed GGUF server',
        async (model) => {
            harness.aiModel = `hf:::${model}`
            harness.modelTokenizer = 12
            harness.transformersTokenizer.mockResolvedValue([101, 42, 102])

            await expect(encode('synthetic prompt')).resolves.toEqual([
                101, 42, 102,
            ])
            expect(
                harness.transformersTokenizer,
            ).toHaveBeenCalledExactlyOnceWith('synthetic prompt', model)
        },
    )

    it('rejects a non-Hugging-Face local tokenizer instead of loading it as a model', async () => {
        harness.aiModel = 'local_C:/synthetic/model.gguf'
        harness.modelTokenizer = 12

        await expect(encode('synthetic prompt')).rejects.toThrow(
            'Local tokenization requires a Hugging Face model (hf:::).',
        )
        expect(harness.transformersTokenizer).not.toHaveBeenCalled()
    })

    it('counts a large homogeneous prompt batch with one native invocation', async () => {
        const chats = Array.from({ length: 100 }, (_, index) => ({
            role: 'user' as const,
            content: `content-${index}`,
            name: `name-${index}`,
        }))
        harness.nativeCountBatch.mockImplementation(async (candidate) => {
            const texts = candidate.buildTexts()
            expect(candidate.itemCount).toBe(200)
            expect(texts).toHaveLength(200)
            return texts.map((text: string) => text.startsWith('name-') ? 2 : 5)
        })

        const result = await new ChatTokenizer(3, 'name').tokenizeChats(chats)

        expect(result).toBe(1_100)
        expect(harness.nativeCountBatch).toHaveBeenCalledTimes(1)
        expect(harness.nativeCountBatch.mock.calls[0][1]).toEqual({
            isTauri: expect.any(Boolean),
            aiModel: 'gpt-4',
            customTokenizer: 'tik',
            modelTokenizerId: 'cl100k_base',
            pluginTokenizer: undefined,
        })
    })

    it('keeps tokenizer-cache users on the existing JavaScript path', async () => {
        harness.useTokenizerCaching = true
        harness.nativeCountBatch.mockResolvedValue(Array.from({ length: 100 }, () => 99))
        const chats = Array.from({ length: 100 }, () => ({
            role: 'user' as const,
            content: '',
        }))

        const result = await new ChatTokenizer(3, 'name').tokenizeChats(chats)

        expect(result).toBe(300)
        expect(harness.nativeCountBatch).not.toHaveBeenCalled()
    })

    it.each(['name', 'noName'] as const)(
        'preserves mixed chat adjustments in %s mode',
        async (useName) => {
            const chats = Array.from({ length: 100 }, (_, index) => ({
                role: index % 2 === 0 ? 'user' as const : 'assistant' as const,
                content: `mixed-content-${index}`,
                name: index % 3 === 0 ? `speaker-${index}` : undefined,
                thoughts: index === 0 ? ['hidden thought', 'another hidden thought'] : undefined,
                multimodals: index === 1
                    ? [
                        { type: 'image' as const, base64: 'image-a' },
                        { type: 'audio' as const, base64: 'audio-b' },
                    ]
                    : undefined,
            }))
            const tokenizer = new ChatTokenizer(3, useName)
            harness.nativeCountBatch.mockResolvedValueOnce(null)
            const expected = await tokenizer.tokenizeChats(chats)
            harness.nativeCountBatch.mockImplementationOnce(async (candidate) => {
                const texts = candidate.buildTexts()
                expect(candidate.itemCount).toBe(texts.length)
                expect(texts.some((text: string) => text.includes('hidden thought'))).toBe(false)
                if(useName === 'noName'){
                    expect(texts.some((text: string) => text.startsWith('speaker-'))).toBe(false)
                }
                return await Promise.all(texts.map(async (text: string) => (await encode(text)).length))
            })

            await expect(tokenizer.tokenizeChats(chats)).resolves.toBe(expected)
        },
    )

    it.each([
        ['null result', null],
        ['rejected invocation', new Error('native count failed')],
    ] as const)('preserves the JavaScript result after a %s', async (_name, nativeResult) => {
        const chats = Array.from({ length: 100 }, (_, index) => ({
            role: 'user' as const,
            content: `fallback-content-${index}`,
            name: index % 4 === 0 ? `fallback-name-${index}` : undefined,
        }))
        const tokenizer = new ChatTokenizer(3, 'name')
        harness.nativeCountBatch.mockResolvedValueOnce(null)
        const expected = await tokenizer.tokenizeChats(chats)
        if(nativeResult instanceof Error){
            harness.nativeCountBatch.mockRejectedValueOnce(nativeResult)
        }
        else {
            harness.nativeCountBatch.mockResolvedValueOnce(nativeResult)
        }

        await expect(tokenizer.tokenizeChats(chats)).resolves.toBe(expected)
    })

    it('preserves the JavaScript special-token result after native rejection', async () => {
        const chats = Array.from({ length: 100 }, (_, index) => ({
            role: 'user' as const,
            content: index === 0 ? '<|ENDOFTEXT|>' : `safe-content-${index}`,
        }))
        const tokenizer = new ChatTokenizer(3, 'name')
        harness.nativeCountBatch.mockResolvedValueOnce(null)
        const expected = await tokenizer.tokenizeChats(chats)
        harness.nativeCountBatch.mockRejectedValueOnce(new Error('native count failed'))

        await expect(tokenizer.tokenizeChats(chats)).resolves.toBe(expected)
    })
})

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
