import { afterAll, describe, expect, it } from 'vitest'
import { Tiktoken } from '@dqbd/tiktoken'
import cl100kBase from '@dqbd/tiktoken/encoders/cl100k_base.json'
import o200kBase from '../../etc/o200k_base.json'
import corpus from '../../../benchmarks/tokenizer/native-tokenizer-corpus.json'
import { NATIVE_TOKENIZER_FINGERPRINTS } from './nativeTokenizer'

type TokenizerId = 'cl100k_base' | 'o200k_base'

type CorpusInput =
    | { kind: 'text'; value: string }
    | { kind: 'utf16-code-units'; codeUnits: number[] }
    | { kind: 'repeat'; value: string; count: number }

function resolveInput(input: CorpusInput): string {
    if (input.kind === 'text') return input.value
    if (input.kind === 'utf16-code-units') return String.fromCharCode(...input.codeUnits)
    return input.value.repeat(input.count)
}

function normalizeError(error: unknown) {
    const message = error instanceof Error ? error.message : String(error)
    const prefix = 'The text contains a special token that is not allowed: '
    if (!message.startsWith(prefix)) throw error
    return { code: 'disallowed_special_token', token: message.slice(prefix.length) }
}

const tokenizers: Record<TokenizerId, Tiktoken> = {
    cl100k_base: new Tiktoken(
        cl100kBase.bpe_ranks,
        cl100kBase.special_tokens,
        cl100kBase.pat_str,
    ),
    o200k_base: new Tiktoken(
        o200kBase.bpe_ranks,
        o200kBase.special_tokens,
        o200kBase.pat_str,
    ),
}

afterAll(() => {
    for (const tokenizer of Object.values(tokenizers)) tokenizer.free()
})

describe('native tokenizer compatibility corpus', () => {
    it.each(corpus.cases)('$name matches the JavaScript oracle', (entry) => {
        const encode = () =>
            tokenizers[entry.tokenizerId as TokenizerId].encode(resolveInput(entry.input as CorpusInput))

        if ('error' in entry) {
            try {
                encode()
                throw new Error('Expected the oracle to reject the input')
            } catch (error) {
                expect(normalizeError(error)).toEqual(entry.error)
            }
            return
        }

        expect(Array.from(encode())).toEqual(entry.ids)
    })

    it('covers both artifacts and every required compatibility class', () => {
        expect(NATIVE_TOKENIZER_FINGERPRINTS).toEqual({
            cl100k_base: corpus.artifacts.cl100k_base.fingerprint,
            o200k_base: corpus.artifacts.o200k_base.fingerprint,
        })
        expect(new Set(corpus.cases.map((entry) => entry.tokenizerId))).toEqual(
            new Set<TokenizerId>(['cl100k_base', 'o200k_base']),
        )
        expect(corpus.cases.map((entry) => entry.class)).toEqual(
            expect.arrayContaining([
                'empty',
                'ascii',
                'whitespace',
                'punctuation',
                'contractions',
                'numbers',
                'source-code',
                'json',
                'markdown',
                'url',
                'base64',
                'korean',
                'simplified-chinese',
                'traditional-chinese',
                'japanese',
                'arabic-rtl',
                'cyrillic',
                'devanagari',
                'unicode-normalization',
                'emoji',
                'control-characters',
                'utf16-boundary',
                'prompt-segment',
                'long-input',
                'allocation-boundary',
                'special-token-error',
            ]),
        )
    })
})
