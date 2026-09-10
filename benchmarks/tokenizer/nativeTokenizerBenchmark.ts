import { Tiktoken } from '@dqbd/tiktoken'
import cl100kBase from '@dqbd/tiktoken/encoders/cl100k_base.json'
import o200kBase from '../../src/etc/o200k_base.json'
import corpus from './native-tokenizer-corpus.json'
import {
    invokeNativeTokenizerBatch,
    resolveNativeTokenizerRoute,
    type NativeTokenizeMode,
    type NativeTokenizerId,
} from '../../src/ts/tokenizer/nativeTokenizer'

type BenchmarkImplementation = 'javascript' | 'native'
type BenchmarkInvoke = (command: string, args: Record<string, unknown>) => Promise<unknown>

export type TokenizerBenchmarkRequest = {
    tokenizerId: NativeTokenizerId
    mode: NativeTokenizeMode
    texts: string[]
}

type BenchmarkOutput = number[] | Uint32Array[]

type CorpusInput =
    | { kind: 'text'; value: string }
    | { kind: 'utf16-code-units'; codeUnits: number[] }
    | { kind: 'repeat'; value: string; count: number }

function resolveCorpusInput(input: CorpusInput): string {
    if (input.kind === 'text') return input.value
    if (input.kind === 'utf16-code-units') {
        return normalizeUtf16(String.fromCharCode(...input.codeUnits))
    }
    return input.value.repeat(input.count)
}

function normalizeUtf16(text: string): string {
    let normalized = ''
    for (let index = 0; index < text.length; index++) {
        const codeUnit = text.charCodeAt(index)
        if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
            const next = text.charCodeAt(index + 1)
            if (next >= 0xdc00 && next <= 0xdfff) {
                normalized += text[index] + text[index + 1]
                index++
            } else {
                normalized += '\ufffd'
            }
        } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
            normalized += '\ufffd'
        } else {
            normalized += text[index]
        }
    }
    return normalized
}

function normalizeSpecialTokenError(error: unknown) {
    const message = error instanceof Error ? error.message : String(error)
    const prefix = 'The text contains a special token that is not allowed: '
    if (!message.startsWith(prefix)) throw error
    return { code: 'disallowed_special_token', token: message.slice(prefix.length) }
}

function createTokenizer(tokenizerId: NativeTokenizerId): Tiktoken {
    const artifact = tokenizerId === 'cl100k_base' ? cl100kBase : o200kBase
    return new Tiktoken(artifact.bpe_ranks, artifact.special_tokens, artifact.pat_str)
}

function checksumValue(checksum: number, value: number): number {
    return Math.imul(checksum ^ value, 16_777_619) >>> 0
}

function summarizeOutput(mode: NativeTokenizeMode, output: BenchmarkOutput) {
    let checksum = 2_166_136_261
    let totalIds = 0
    let typedArraySegments = 0

    if (mode === 'count') {
        for (const count of output as number[]) {
            totalIds += count
            checksum = checksumValue(checksum, count)
        }
    } else {
        for (const ids of output as Uint32Array[]) {
            if (ids instanceof Uint32Array) typedArraySegments++
            totalIds += ids.length
            checksum = checksumValue(checksum, ids.length)
            for (const id of ids) checksum = checksumValue(checksum, id)
        }
    }

    return {
        mode,
        segmentCount: output.length,
        totalIds,
        typedArraySegments,
        checksum,
    }
}

export function createNativeTokenizerBenchmarkSeam(invokeCommand?: BenchmarkInvoke) {
    let selectedTokenizerId: NativeTokenizerId | null = null
    let tokenizer: Tiktoken | null = null

    function initialize(tokenizerId: NativeTokenizerId) {
        tokenizer?.free()
        const startedAt = performance.now()
        tokenizer = createTokenizer(tokenizerId)
        selectedTokenizerId = tokenizerId
        return { durationMs: performance.now() - startedAt }
    }

    function requireTokenizer(tokenizerId: NativeTokenizerId) {
        if (!tokenizer || selectedTokenizerId !== tokenizerId) {
            throw new Error(`Tokenizer ${tokenizerId} is not initialized`)
        }
        return tokenizer
    }

    function verifyJavaScriptCorpus(tokenizerId: NativeTokenizerId) {
        const selected = requireTokenizer(tokenizerId)
        const entries = corpus.cases.filter((entry) => entry.tokenizerId === tokenizerId)
        let idCases = 0
        let errorCases = 0

        for (const entry of entries) {
            const input = resolveCorpusInput(entry.input as CorpusInput)
            if ('ids' in entry) {
                const actual = selected.encode(input)
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
                selected.encode(input)
                throw new Error(`${entry.name} should reject its special token`)
            } catch (error) {
                const actual = normalizeSpecialTokenError(error)
                if (actual.code !== entry.error.code || actual.token !== entry.error.token) {
                    throw new Error(`${entry.name} error differs from the literal corpus`)
                }
            }
            errorCases++
        }

        return { tokenizerId, idCases, errorCases, passed: true as const }
    }

    async function runJavaScript(request: TokenizerBenchmarkRequest): Promise<BenchmarkOutput> {
        const selected = requireTokenizer(request.tokenizerId)
        const ids = request.texts.map((text) => selected.encode(text))
        if (request.mode === 'count') return ids.map((value) => value.length)
        return ids
    }

    async function runNative(request: TokenizerBenchmarkRequest): Promise<BenchmarkOutput> {
        const route = resolveNativeTokenizerRoute(request.tokenizerId, true, true)
        if (route.kind !== 'native-tiktoken') throw new Error('Native candidate route was not selected')
        const response = invokeCommand
            ? await invokeNativeTokenizerBatch(
                  route,
                  request.texts,
                  request.mode,
                  invokeCommand,
              )
            : await invokeNativeTokenizerBatch(route, request.texts, request.mode)
        if (response.mode === 'count') return response.counts
        return response.ids.map((ids) => Uint32Array.from(ids))
    }

    function run(implementation: BenchmarkImplementation, request: TokenizerBenchmarkRequest) {
        return implementation === 'javascript' ? runJavaScript(request) : runNative(request)
    }

    async function warm(implementation: BenchmarkImplementation, request: TokenizerBenchmarkRequest) {
        return summarizeOutput(request.mode, await run(implementation, request))
    }

    async function measure(
        implementation: BenchmarkImplementation,
        request: TokenizerBenchmarkRequest,
        samples: number,
    ) {
        const durationsMs: number[] = []
        let output: BenchmarkOutput | null = null
        for (let sample = 0; sample < samples; sample++) {
            const startedAt = performance.now()
            output = await run(implementation, request)
            durationsMs.push(performance.now() - startedAt)
        }
        if (!output) throw new Error('Tokenizer benchmark requires at least one sample')
        return { durationsMs, result: summarizeOutput(request.mode, output) }
    }

    function dispose() {
        tokenizer?.free()
        tokenizer = null
        selectedTokenizerId = null
    }

    return { initialize, verifyJavaScriptCorpus, warm, measure, dispose }
}

export function installNativeTokenizerBenchmarkSeam(target?: Record<string, unknown>) {
    const seam = createNativeTokenizerBenchmarkSeam()
    const benchmarkTarget = target ?? (window as unknown as Record<string, unknown>)
    benchmarkTarget.__RISUNEST_TOKENIZER_BENCHMARK__ = seam
    return seam
}
