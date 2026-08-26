import { describe, expect, it, vi } from 'vitest'
import {
    NATIVE_TOKENIZER_MIN_BATCH_ITEMS,
    resolveProductionNativeTokenizerId,
    tryNativeTokenizerIdsBatch,
    type ProductionNativeTokenizerContext,
} from './nativeTokenizerProduction'
import { NATIVE_TOKENIZER_FINGERPRINTS } from './nativeTokenizer'

const cl100kContext: ProductionNativeTokenizerContext = {
    isTauri: true,
    aiModel: 'gpt-4',
    customTokenizer: 'tik',
    modelTokenizerId: 'cl100k_base',
}

describe('production native tokenizer eligibility', () => {
    it('keeps small batches and Web on the existing JavaScript route', async () => {
        const invoke = vi.fn()

        await expect(
            tryNativeTokenizerIdsBatch(
                Array.from({ length: NATIVE_TOKENIZER_MIN_BATCH_ITEMS - 1 }, (_, index) => `${index}`),
                cl100kContext,
                invoke,
            ),
        ).resolves.toBeNull()
        await expect(
            tryNativeTokenizerIdsBatch(
                Array.from({ length: NATIVE_TOKENIZER_MIN_BATCH_ITEMS }, (_, index) => `${index}`),
                { ...cl100kContext, isTauri: false },
                invoke,
            ),
        ).resolves.toBeNull()
        expect(invoke).not.toHaveBeenCalled()
    })

    it.each([
        [{ ...cl100kContext, aiModel: 'openrouter', customTokenizer: 'tik' }, 'o200k_base'],
        [{ ...cl100kContext, aiModel: 'reverse_proxy', customTokenizer: 'tik' }, 'o200k_base'],
        [
            {
                ...cl100kContext,
                aiModel: 'custom',
                modelTokenizerId: null,
                pluginTokenizer: 'cl100k_base',
            },
            'cl100k_base',
        ],
        [
            {
                ...cl100kContext,
                aiModel: 'custom',
                modelTokenizerId: null,
                pluginTokenizer: 'o200k_base',
            },
            'o200k_base',
        ],
    ] as const)('resolves the exact supported route from %o', (context, expected) => {
        expect(resolveProductionNativeTokenizerId(context)).toBe(expected)
    })

    it.each([
        { ...cl100kContext, aiModel: 'openrouter', customTokenizer: 'mistral' },
        { ...cl100kContext, aiModel: 'reverse_proxy', customTokenizer: 'llama' },
        {
            ...cl100kContext,
            aiModel: 'custom',
            modelTokenizerId: null,
            pluginTokenizer: 'custom',
        },
        { ...cl100kContext, modelTokenizerId: null },
    ] as const)('keeps unsupported route %o in JavaScript', (context) => {
        expect(resolveProductionNativeTokenizerId(context)).toBeNull()
    })

    it('invokes one ordered IDs batch at the measured threshold', async () => {
        const texts = Array.from(
            { length: NATIVE_TOKENIZER_MIN_BATCH_ITEMS },
            (_, index) => `segment-${index}`,
        )
        const ids = texts.map((_, index) => [index, index + 1])
        const invoke = vi.fn(async () => ({
            mode: 'ids',
            artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
            ids,
        }))

        await expect(tryNativeTokenizerIdsBatch(texts, cl100kContext, invoke)).resolves.toEqual(ids)
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(invoke).toHaveBeenCalledWith('tokenize_batch', {
            request: {
                tokenizer_id: 'cl100k_base',
                artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
                mode: 'ids',
                texts,
            },
        })
    })

    it('propagates native failures so the production caller can preserve JavaScript errors', async () => {
        const failure = new Error('native boundary failed')
        const invoke = vi.fn(async () => {
            throw failure
        })

        await expect(
            tryNativeTokenizerIdsBatch(
                Array.from({ length: NATIVE_TOKENIZER_MIN_BATCH_ITEMS }, (_, index) => `${index}`),
                cl100kContext,
                invoke,
            ),
        ).rejects.toBe(failure)
    })
})
