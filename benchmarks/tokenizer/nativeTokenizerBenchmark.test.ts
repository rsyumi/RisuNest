import { afterEach, describe, expect, it } from 'vitest'
import { createNativeTokenizerBenchmarkSeam, installNativeTokenizerBenchmarkSeam } from './nativeTokenizerBenchmark'
import { NATIVE_TOKENIZER_FINGERPRINTS } from '../../src/ts/tokenizer/nativeTokenizer'

const seams: Array<{ dispose(): void }> = []

afterEach(() => {
    for (const seam of seams.splice(0)) seam.dispose()
})

describe('native tokenizer benchmark WebView seam', () => {
    it.each([
        ['cl100k_base', 34, 10],
        ['o200k_base', 34, 4],
    ] as const)(
        'proves the %s JavaScript implementation against the literal corpus',
        (tokenizerId, idCases, errorCases) => {
            const seam = createNativeTokenizerBenchmarkSeam()
            seams.push(seam)

            const initialized = seam.initialize(tokenizerId)

            expect(initialized.durationMs).toBeGreaterThanOrEqual(0)
            expect(seam.verifyJavaScriptCorpus(tokenizerId)).toEqual({
                tokenizerId,
                idCases,
                errorCases,
                passed: true,
            })
        },
    )

    it('warms both implementations and measures equivalent typed-array ID results', async () => {
        const invokeCalls: Array<{ command: string; args: Record<string, unknown> }> = []
        const seam = createNativeTokenizerBenchmarkSeam(async (command, args) => {
            invokeCalls.push({ command, args })
            return {
                mode: 'ids',
                artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
                ids: [[15339], [], [15339]],
            }
        })
        seams.push(seam)
        seam.initialize('cl100k_base')
        const request = {
            tokenizerId: 'cl100k_base' as const,
            mode: 'ids' as const,
            texts: ['hello', '', 'hello'],
        }

        const javascriptWarm = await seam.warm('javascript', request)
        const nativeWarm = await seam.warm('native', request)
        const javascript = await seam.measure('javascript', request, 2)
        const native = await seam.measure('native', request, 2)

        expect(nativeWarm).toEqual(javascriptWarm)
        expect(native.result).toEqual(javascript.result)
        expect(native.result).toMatchObject({
            mode: 'ids',
            segmentCount: 3,
            totalIds: 2,
            typedArraySegments: 3,
        })
        expect(javascript.durationsMs).toHaveLength(2)
        expect(native.durationsMs).toHaveLength(2)
        expect(invokeCalls).toHaveLength(3)
        expect(invokeCalls[0]).toMatchObject({ command: 'tokenize_batch' })
    })

    it('installs only the explicit benchmark surface on its WebView target', () => {
        const target: Record<string, unknown> = {}

        const seam = installNativeTokenizerBenchmarkSeam(target)
        seams.push(seam)

        expect(target.__RISUNEST_TOKENIZER_BENCHMARK__).toBe(seam)
        expect(Object.keys(target)).toEqual(['__RISUNEST_TOKENIZER_BENCHMARK__'])
    })
})
