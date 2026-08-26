import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'

import { Tiktoken } from '@dqbd/tiktoken'
import cl100kBase from '@dqbd/tiktoken/encoders/cl100k_base.json' with { type: 'json' }
import o200kBase from '../../src/etc/o200k_base.json' with { type: 'json' }

const CORPUS_URL = new URL('./native-tokenizer-corpus.json', import.meta.url)

const ORACLE_ARTIFACTS = {
    cl100k_base: {
        data: cl100kBase,
        url: new URL('../../node_modules/@dqbd/tiktoken/encoders/cl100k_base.json', import.meta.url),
    },
    o200k_base: {
        data: o200kBase,
        url: new URL('../../src/etc/o200k_base.json', import.meta.url),
    },
}

export const REQUIRED_COMPATIBILITY_CLASSES = [
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
]

export async function loadTokenizerCorpus() {
    return JSON.parse(await readFile(CORPUS_URL, 'utf8'))
}

export function resolveCorpusInput(input) {
    if (input.kind === 'text') return input.value
    if (input.kind === 'utf16-code-units') return String.fromCharCode(...input.codeUnits)
    if (input.kind === 'repeat') return input.value.repeat(input.count)
    throw new Error(`Unsupported corpus input kind: ${input.kind}`)
}

function normalizeOracleError(error) {
    const message = error instanceof Error ? error.message : String(error)
    const prefix = 'The text contains a special token that is not allowed: '
    if (!message.startsWith(prefix)) throw error
    return { code: 'disallowed_special_token', token: message.slice(prefix.length) }
}

function createOracle(tokenizerId) {
    const artifact = ORACLE_ARTIFACTS[tokenizerId]
    if (!artifact) throw new Error(`Unsupported tokenizer ID: ${tokenizerId}`)
    return new Tiktoken(artifact.data.bpe_ranks, artifact.data.special_tokens, artifact.data.pat_str)
}

export async function verifyArtifactHashes(corpus) {
    const hashes = {}
    for (const [tokenizerId, artifact] of Object.entries(ORACLE_ARTIFACTS)) {
        const actual = createHash('sha256').update(await readFile(artifact.url)).digest('hex')
        const expected = corpus.artifacts[tokenizerId]?.sha256
        if (actual !== expected) {
            throw new Error(`${tokenizerId} artifact hash differs: expected ${expected}, received ${actual}`)
        }
        hashes[tokenizerId] = actual
    }
    return hashes
}

export async function verifyTokenizerCorpus(corpus) {
    const result = {}
    for (const tokenizerId of Object.keys(ORACLE_ARTIFACTS)) {
        const tokenizer = createOracle(tokenizerId)
        let idCases = 0
        let errorCases = 0
        try {
            for (const entry of corpus.cases.filter((item) => item.tokenizerId === tokenizerId)) {
                const input = resolveCorpusInput(entry.input)
                if ('ids' in entry) {
                    const actual = Array.from(tokenizer.encode(input))
                    if (actual.length !== entry.ids.length) {
                        throw new Error(`${entry.name} token count differs from the literal corpus`)
                    }
                    for (let index = 0; index < actual.length; index++) {
                        if (actual[index] !== entry.ids[index]) {
                            throw new Error(`${entry.name} token ID differs at index ${index}`)
                        }
                    }
                    idCases++
                    continue
                }

                try {
                    tokenizer.encode(input)
                    throw new Error(`${entry.name} should reject its special token`)
                } catch (error) {
                    const actual = normalizeOracleError(error)
                    if (actual.code !== entry.error.code || actual.token !== entry.error.token) {
                        throw new Error(`${entry.name} error differs from the literal corpus`)
                    }
                }
                errorCases++
            }
        } finally {
            tokenizer.free()
        }
        result[tokenizerId] = { idCases, errorCases }
    }
    return result
}

export function createOracleBenchmarkFixtures(corpus, tokenizerId) {
    const successful = corpus.cases.filter(
        (entry) => entry.tokenizerId === tokenizerId && 'ids' in entry,
    )
    const shortSeed = successful.find((entry) => entry.name.endsWith(':synthetic-chat-segment'))
    const longPrompt = successful.find((entry) => entry.name.endsWith(':long-prompt-over-32-kib'))
    const realistic = successful.filter((entry) => entry.class === 'prompt-segment')
    if (!shortSeed || !longPrompt || realistic.length !== 6) {
        throw new Error(`${tokenizerId} corpus is missing benchmark fixture inputs`)
    }

    const shortText = resolveCorpusInput(shortSeed.input)
    return [
        ...[1, 10, 100, 1000].map((count) => ({
            name: `short-segments-${count}`,
            texts: Array.from({ length: count }, (_, index) => `${shortText}\nsegment:${index}`),
        })),
        { name: 'prompt-over-32-kib', texts: [resolveCorpusInput(longPrompt.input)] },
        {
            name: 'realistic-prompt-segments',
            texts: realistic.map((entry) => resolveCorpusInput(entry.input)),
        },
    ]
}

export function createOracleEncoder(tokenizerId) {
    return createOracle(tokenizerId)
}

export function percentile(samples, fraction) {
    if (samples.length === 0) throw new Error('percentile requires at least one sample')
    const sorted = [...samples].sort((left, right) => left - right)
    return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)]
}
