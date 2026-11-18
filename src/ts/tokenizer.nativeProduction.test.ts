import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import gemma4Reference from '../../tests/fixtures/tokenizer/gemma4.json'

const harness = vi.hoisted(() => ({
    nativeBatch: vi.fn(),
    nativeCountBatch: vi.fn(),
    useTokenizerCaching: false,
    aiModel: 'gpt-4',
    modelTokenizer: 1,
    modelInternalID: 'synthetic-count-model',
    customTokenizer: 'tik',
    googleClaudeTokenizing: false,
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
        customTokenizer: harness.customTokenizer,
        googleClaudeTokenizing: harness.googleClaudeTokenizing,
        useTokenizerCaching: harness.useTokenizerCaching,
    }),
}))

vi.mock('./process/files/inlays', () => ({ supportsInlayImage: () => false }))
// Like the parser, {{mainprompt}} reads the database the caller passes.
vi.mock('./parser/parser.svelte', async () => (await import('./parser/parser.testSupport')).parserStubModule({
    risuChatParser: (value: string, arg?: { db?: { mainPrompt?: string } }) =>
        value.replace('{{mainprompt}}', arg?.db?.mainPrompt ?? ''),
}))
vi.mock('./process/transformers', () => ({
    tokenizeTransformers: harness.transformersTokenizer,
}))
vi.mock('./globalApi.svelte', () => ({ globalFetch: vi.fn() }))
vi.mock('./model/modellist', () => ({
    getModelInfo: () => ({ tokenizer: harness.modelTokenizer, internalID: harness.modelInternalID }),
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

import { ChatTokenizer, encode, encodeWithTokenizer, strongBan, tokenizeAccurate } from './tokenizer'
import type { Database } from './storage/database.svelte'
import { pluginV2 } from './plugins/plugins.svelte'
import type { OpenAIChat } from './process/index.svelte'

describe('chat tokenizer native count batching', () => {
    beforeEach(() => {
        pluginV2.providerOptions.clear()
        harness.nativeCountBatch.mockReset()
        harness.useTokenizerCaching = false
        harness.aiModel = 'gpt-4'
        harness.modelTokenizer = 1
        harness.modelInternalID = 'synthetic-count-model'
        harness.googleClaudeTokenizing = false
        harness.transformersTokenizer.mockReset()
    })

    afterEach(() => vi.unstubAllGlobals())

    it('preserves arbitrary plugin tokenizer calls even with generation reuse enabled', async () => {
        harness.aiModel = 'custom'
        const tokenizerFunc = vi.fn().mockResolvedValueOnce([1, 2]).mockResolvedValueOnce([1, 2, 3])
        pluginV2.providerOptions.set('', { tokenizer: 'custom', tokenizerFunc })
        const tokenizer = new ChatTokenizer(2, 'noName', true)
        const chat = { role: 'user' as const, content: 'synthetic' }
        await expect(tokenizer.tokenizeChat(chat)).resolves.toBe(4)
        await expect(tokenizer.tokenizeChat(chat)).resolves.toBe(5)
        expect(tokenizerFunc).toHaveBeenCalledTimes(2)
    })

    it.each([false, true])('preserves exact generation counts across edits and tokenizer changes (global cache: %s)', async (globalCache) => {
        harness.useTokenizerCaching = globalCache
        const tokenizer = new ChatTokenizer(2, 'name', true)
        const chat: OpenAIChat = { role: 'user', content: '첫 번째 답변 🙂', name: '사용자' }
        const check = async (tokenizerId: string) => {
            const expected = (await encodeWithTokenizer(chat.content, tokenizerId)).length + 2
                + (chat.name ? (await encodeWithTokenizer(chat.name, tokenizerId)).length + 1 : 0)
            expect(await tokenizer.tokenizeChat(chat)).toBe(expected)
            expect(await tokenizer.tokenizeChat(chat)).toBe(expected)
        }
        await check('tik')
        chat.content = 'A changed answer with more words, punctuation and Unicode: 한글 🙂.'
        await check('tik')
        chat.name = 'A longer speaker name'
        await check('tik')
        delete chat.name
        await check('tik')
        harness.modelTokenizer = 2
        await check('o200k_base')
        harness.aiModel = 'custom'
        pluginV2.providerOptions.set('', { tokenizer: 'cl100k_base' })
        await check('tik')
        pluginV2.providerOptions.set('', { tokenizer: 'o200k_base' })
        await check('o200k_base')
    })

    it('recomputes multimodal and requested thought adjustments around cached text counts', async () => {
        const tokenizer = new ChatTokenizer(2, 'noName', true)
        const chat: OpenAIChat = { role: 'assistant', content: 'synthetic response' }
        const text = (await encodeWithTokenizer(chat.content, 'tik')).length + 2
        expect(await tokenizer.tokenizeChat(chat)).toBe(text)
        chat.multimodals = [{ type: 'image', base64: 'synthetic' }]
        expect(await tokenizer.tokenizeChat(chat)).toBe(text + 2)
        chat.multimodals.push({type: 'image', base64: 'synthetic-second'})
        expect(await tokenizer.tokenizeChat(chat)).toBe(text + 4)
        delete chat.multimodals
        chat.thoughts = ['first thought', '두 번째 생각']
        const thoughts = (await Promise.all(chat.thoughts.map(async thought =>
            (await encodeWithTokenizer(thought, 'tik')).length + 1))).reduce((total, count) => total + count, 0)
        expect(await tokenizer.tokenizeChat(chat, {countThoughts: true})).toBe(text + thoughts)
        expect(await tokenizer.tokenizeChat(chat)).toBe(text)
    })

    it('preserves remote tokenizer routing changes when generation reuse is enabled', async () => {
        harness.aiModel = 'google-test'
        harness.modelTokenizer = 10
        harness.googleClaudeTokenizing = true
        const fetchCount = vi.fn().mockResolvedValueOnce(new Response(JSON.stringify({totalTokens: 7})))
            .mockResolvedValueOnce(new Response(JSON.stringify({totalTokens: 11})))
        vi.stubGlobal('fetch', fetchCount)
        const tokenizer = new ChatTokenizer(2, 'noName', true)
        const chat: OpenAIChat = {role: 'user', content: 'synthetic remote count'}
        expect(await tokenizer.tokenizeChat(chat)).toBe(9)
        harness.modelInternalID = 'synthetic-count-model-other'
        expect(await tokenizer.tokenizeChat(chat)).toBe(13)
    })

    it('keeps count reuse opt-in and never reuses a failed count', async () => {
        harness.aiModel = 'hf:::synthetic/model'
        harness.modelTokenizer = 12
        harness.transformersTokenizer.mockResolvedValue([1])
        const chat = { role: 'user' as const, content: 'synthetic' }
        const ordinary = new ChatTokenizer(0, 'noName')
        await ordinary.tokenizeChat(chat)
        await ordinary.tokenizeChat(chat)
        expect(harness.transformersTokenizer).toHaveBeenCalledTimes(2)
        const cached = new ChatTokenizer(0, 'noName', true)
        harness.transformersTokenizer.mockRejectedValueOnce(new Error('synthetic tokenizer failure'))
        await expect(cached.tokenizeChat(chat)).rejects.toThrow('synthetic tokenizer failure')
        await expect(cached.tokenizeChat(chat)).resolves.toBe(1)
        expect(harness.transformersTokenizer).toHaveBeenCalledTimes(4)
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

describe('encodeWithTokenizer', () => {
    it('encodes with o200k_base when it is named', async () => {
        const { Tiktoken } = await import('@dqbd/tiktoken')
        const o200k = await import('src/etc/o200k_base.json')
        const expected = new Tiktoken(o200k.bpe_ranks, o200k.special_tokens, o200k.pat_str)
        const text = '합성 테스트 문장입니다. Synthetic sentence.'
        try {
            expect([...await encodeWithTokenizer(text, 'o200k_base')]).toEqual([...expected.encode(text)])
        } finally {
            expected.free()
        }
    })

    it('encodes with the loaded tokenizer while another one loads', async () => {
        const text = '합성 테스트 문장입니다. Synthetic sentence.'
        const cl100k = [...await encodeWithTokenizer(text, 'tik')]
        const o200k = [...await encodeWithTokenizer(text, 'o200k_base')]
        await encodeWithTokenizer(text, 'tik')
        const [loading, loaded] = await Promise.all([
            encodeWithTokenizer(text, 'o200k_base'),
            encodeWithTokenizer(text, 'tik'),
        ])
        expect([[...loading], [...loaded]]).toEqual([o200k, cl100k])
    })
})

describe('tokenizeAccurate', () => {
    it('counts preset macros from the database it is given', async () => {
        const preset = {
            aiModel: 'gpt-4', currentPluginProvider: '', customTokenizer: 'tik', mainPrompt: 'Synthetic edited preset prompt.',
        } as Database
        const expected = (await encodeWithTokenizer(preset.mainPrompt, 'tik')).length
        expect(expected).toBeGreaterThan(0)
        expect(await tokenizeAccurate('{{mainprompt}}', true, preset)).toBe(expected)
    })
})

describe('Gemma and Llama tokenizers', () => {
    const turnText = '<|turn>user\nHello<turn|>'
    const turnIds = [105, 2364, 107, 9259, 106]

    beforeEach(() => {
        harness.aiModel = 'gemini-2.5-flash'
        harness.modelTokenizer = 10
        harness.customTokenizer = 'gemma'
        harness.googleClaudeTokenizing = false
        harness.useTokenizerCaching = false
        pluginV2.providerOptions.clear()
        vi.stubGlobal('fetch', vi.fn(async (input: string) => {
            if (input.startsWith('https://generativelanguage.googleapis.com/')) {
                return new Response('', { status: 503 })
            }
            if (!input.startsWith('/token/') || input.includes('..')) {
                throw new Error(`Unexpected tokenizer asset: ${input}`)
            }
            const bytes = await readFile(resolve('public', input.slice(1)))
            return new Response(bytes)
        }))
    })

    afterEach(() => {
        vi.unstubAllGlobals()
        harness.aiModel = 'gpt-4'
        harness.modelTokenizer = 1
        harness.customTokenizer = 'tik'
        harness.googleClaudeTokenizing = false
        pluginV2.providerOptions.clear()
    })

    it.each(gemma4Reference.cases)('matches the Gemma 4 reference IDs for $name', async ({ text, ids }) => {
        expect([...await encode(text)]).toEqual(ids)
        expect([...await encodeWithTokenizer(text, 'gemma')]).toEqual(ids)
    })

    it.each(['openrouter', 'reverse_proxy', 'custom'])(
        'uses Gemma 4 when %s selects gemma',
        async (aiModel) => {
            harness.aiModel = aiModel
            pluginV2.providerOptions.set('', { tokenizer: 'gemma' })
            expect([...await encode(turnText)]).toEqual(turnIds)
        },
    )

    it('uses Gemma 4 for the built-in Gemma tokenizer', async () => {
        harness.modelTokenizer = 9
        expect([...await encode(turnText)]).toEqual(turnIds)
    })

    it('uses the same Gemma 4 IDs when the Google token count API fails', async () => {
        harness.googleClaudeTokenizing = true
        expect([...await encode(turnText)]).toEqual(turnIds)
    })

    it('keeps a successful Google token count when the API is enabled', async () => {
        harness.googleClaudeTokenizing = true
        vi.stubGlobal('fetch', vi.fn(async () => Response.json({ totalTokens: 7 })))
        expect((await encode('API count')).length).toBe(7)
    })

    it.each(['openrouter', 'reverse_proxy', 'custom'])(
        'uses Llama 3 IDs when %s selects llama3',
        async (aiModel) => {
            harness.aiModel = aiModel
            harness.customTokenizer = 'llama3'
            pluginV2.providerOptions.set('', { tokenizer: 'llama3' })
            const expected = [...await encodeWithTokenizer('Hello, world!', 'llama3')]
            expect([...await encodeWithTokenizer('Hello, world!', 'llama')]).not.toEqual(expected)
            expect([...await encode('Hello, world!')]).toEqual(expected)
        },
    )
})
