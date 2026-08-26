import { describe, expect, it, vi } from 'vitest'
import {
    NATIVE_TOKENIZER_MAX_AGGREGATE_INPUT_BYTES,
    NATIVE_TOKENIZER_MAX_BATCH_ITEMS,
    NATIVE_TOKENIZER_MIN_BATCH_ITEMS,
    resolveProductionNativeTokenizerId,
    tryNativeTokenizerCountBatch,
    tryNativeTokenizerIdsBatch,
    type NativeTokenizerIdsBatchCandidate,
    type ProductionNativeTokenizerContext,
} from './nativeTokenizerProduction'
import { NATIVE_TOKENIZER_FINGERPRINTS } from './nativeTokenizer'

const cl100kContext: ProductionNativeTokenizerContext = {
    isTauri: true,
    aiModel: 'gpt-4',
    customTokenizer: 'tik',
    modelTokenizerId: 'cl100k_base',
}

function candidate(texts: string[]): NativeTokenizerIdsBatchCandidate {
    return {
        itemCount: texts.length,
        aggregateInputBytes: () =>
            texts.reduce((total, text) => total + new TextEncoder().encode(text).byteLength, 0),
        buildTexts: () => texts,
    }
}

describe('production native tokenizer eligibility', () => {
    it('keeps small batches and Web on the existing JavaScript route', async () => {
        const invoke = vi.fn()
        const smallMeasure = vi.fn(() => 0)
        const smallBuild = vi.fn(() => [] as string[])
        const webMeasure = vi.fn(() => 0)
        const webBuild = vi.fn(() => [] as string[])

        await expect(
            tryNativeTokenizerIdsBatch(
                {
                    itemCount: NATIVE_TOKENIZER_MIN_BATCH_ITEMS - 1,
                    aggregateInputBytes: smallMeasure,
                    buildTexts: smallBuild,
                },
                cl100kContext,
                invoke,
            ),
        ).resolves.toBeNull()
        await expect(
            tryNativeTokenizerIdsBatch(
                {
                    itemCount: NATIVE_TOKENIZER_MIN_BATCH_ITEMS,
                    aggregateInputBytes: webMeasure,
                    buildTexts: webBuild,
                },
                { ...cl100kContext, isTauri: false },
                invoke,
            ),
        ).resolves.toBeNull()
        expect(invoke).not.toHaveBeenCalled()
        expect(smallMeasure).not.toHaveBeenCalled()
        expect(smallBuild).not.toHaveBeenCalled()
        expect(webMeasure).not.toHaveBeenCalled()
        expect(webBuild).not.toHaveBeenCalled()
    })

    it('rejects unsupported and oversized candidates before building a batch or invoking IPC', async () => {
        const invoke = vi.fn()
        const unsupportedMeasure = vi.fn(() => 0)
        const unsupportedBuild = vi.fn(() => [] as string[])
        const tooManyMeasure = vi.fn(() => 0)
        const tooManyBuild = vi.fn(() => [] as string[])
        const oversizedMeasure = vi.fn(() => NATIVE_TOKENIZER_MAX_AGGREGATE_INPUT_BYTES + 1)
        const oversizedBuild = vi.fn(() => [] as string[])

        await expect(
            tryNativeTokenizerIdsBatch(
                {
                    itemCount: NATIVE_TOKENIZER_MIN_BATCH_ITEMS,
                    aggregateInputBytes: unsupportedMeasure,
                    buildTexts: unsupportedBuild,
                },
                { ...cl100kContext, modelTokenizerId: null },
                invoke,
            ),
        ).resolves.toBeNull()
        await expect(
            tryNativeTokenizerIdsBatch(
                {
                    itemCount: NATIVE_TOKENIZER_MAX_BATCH_ITEMS + 1,
                    aggregateInputBytes: tooManyMeasure,
                    buildTexts: tooManyBuild,
                },
                cl100kContext,
                invoke,
            ),
        ).resolves.toBeNull()
        await expect(
            tryNativeTokenizerIdsBatch(
                {
                    itemCount: NATIVE_TOKENIZER_MIN_BATCH_ITEMS,
                    aggregateInputBytes: oversizedMeasure,
                    buildTexts: oversizedBuild,
                },
                cl100kContext,
                invoke,
            ),
        ).resolves.toBeNull()

        expect(invoke).not.toHaveBeenCalled()
        expect(unsupportedMeasure).not.toHaveBeenCalled()
        expect(unsupportedBuild).not.toHaveBeenCalled()
        expect(tooManyMeasure).not.toHaveBeenCalled()
        expect(tooManyBuild).not.toHaveBeenCalled()
        expect(oversizedMeasure).toHaveBeenCalledTimes(1)
        expect(oversizedBuild).not.toHaveBeenCalled()
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

        await expect(tryNativeTokenizerIdsBatch(candidate(texts), cl100kContext, invoke)).resolves.toEqual(ids)
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

    it('invokes one ordered count batch at the measured threshold', async () => {
        const texts = Array.from(
            { length: NATIVE_TOKENIZER_MIN_BATCH_ITEMS },
            (_, index) => `count-segment-${index}`,
        )
        const counts = texts.map((_, index) => index + 1)
        const invoke = vi.fn(async () => ({
            mode: 'count',
            artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
            counts,
        }))

        await expect(
            tryNativeTokenizerCountBatch(candidate(texts), cl100kContext, invoke),
        ).resolves.toEqual(counts)
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(invoke).toHaveBeenCalledWith('tokenize_batch', {
            request: {
                tokenizer_id: 'cl100k_base',
                artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
                mode: 'count',
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
                candidate(
                    Array.from(
                        { length: NATIVE_TOKENIZER_MIN_BATCH_ITEMS },
                        (_, index) => `${index}`,
                    ),
                ),
                cl100kContext,
                invoke,
            ),
        ).rejects.toBe(failure)
    })
})
