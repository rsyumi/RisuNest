import { describe, expect, it, vi } from 'vitest'
import {
    NATIVE_TOKENIZER_FINGERPRINTS,
    NATIVE_TOKENIZER_PRODUCTION_ENABLED,
    invokeNativeTokenizerBatch,
    resolveNativeTokenizerRoute,
} from './nativeTokenizer'

describe('native tokenizer candidate routing', () => {
    it('keeps general routing disabled outside an explicitly measured production seam', () => {
        expect(NATIVE_TOKENIZER_PRODUCTION_ENABLED).toBe(false)
        expect(resolveNativeTokenizerRoute('cl100k_base', true)).toEqual({
            kind: 'existing',
            tokenizerId: 'cl100k_base',
        })
    })

    it.each(['cl100k_base', 'o200k_base'] as const)(
        'selects the stable %s artifact only for an explicit Tauri candidate run',
        (tokenizerId) => {
            expect(resolveNativeTokenizerRoute(tokenizerId, true, true)).toEqual({
                kind: 'native-tiktoken',
                tokenizerId,
                fingerprint: NATIVE_TOKENIZER_FINGERPRINTS[tokenizerId],
            })
        },
    )

    it.each([
        ['cl100k_base', false],
        ['o200k_base', false],
        ['google-remote', true],
        ['plugin-custom', true],
        ['gguf', true],
        ['sentencepiece', true],
    ])('preserves the existing %s route when Tauri eligibility is %s', (tokenizerId, isTauri) => {
        expect(resolveNativeTokenizerRoute(tokenizerId, isTauri as boolean, true)).toEqual({
            kind: 'existing',
            tokenizerId,
        })
    })
})

describe('native tokenizer batch boundary', () => {
    it('normalizes JavaScript surrogate boundaries and invokes once for an ordered IDs batch', async () => {
        const invoke = vi.fn(async (_command: string, args: Record<string, unknown>) => {
            const request = args.request as { artifact_fingerprint: string }
            return {
                mode: 'ids',
                artifact_fingerprint: request.artifact_fingerprint,
                ids: [[15339], [5809], [15339]],
            }
        })
        const route = resolveNativeTokenizerRoute('cl100k_base', true, true)
        if (route.kind !== 'native-tiktoken') throw new Error('candidate route was not selected')

        await expect(
            invokeNativeTokenizerBatch(route, ['hello', '\ud800', 'hello'], 'ids', invoke),
        ).resolves.toEqual({
            mode: 'ids',
            artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
            ids: [[15339], [5809], [15339]],
        })
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(invoke).toHaveBeenCalledWith('tokenize_batch', {
            request: {
                tokenizer_id: 'cl100k_base',
                artifact_fingerprint: NATIVE_TOKENIZER_FINGERPRINTS.cl100k_base,
                mode: 'ids',
                texts: ['hello', '�', 'hello'],
            },
        })
    })

    it('rejects a native result from a different artifact contract', async () => {
        const route = resolveNativeTokenizerRoute('o200k_base', true, true)
        if (route.kind !== 'native-tiktoken') throw new Error('candidate route was not selected')
        const invoke = vi.fn(async () => ({
            mode: 'count',
            artifact_fingerprint: 'wrong-artifact',
            counts: [1],
        }))

        await expect(invokeNativeTokenizerBatch(route, ['hello'], 'count', invoke)).rejects.toMatchObject({
            code: 'artifact_fingerprint_mismatch',
        })
    })

    it('rejects a response whose mode differs from the request', async () => {
        const route = resolveNativeTokenizerRoute('cl100k_base', true, true)
        if (route.kind !== 'native-tiktoken') throw new Error('candidate route was not selected')
        const invoke = vi.fn(async () => ({
            mode: 'ids',
            artifact_fingerprint: route.fingerprint,
            ids: [[15339]],
        }))

        await expect(invokeNativeTokenizerBatch(route, ['hello'], 'count', invoke)).rejects.toMatchObject({
            code: 'response_mode_mismatch',
        })
    })

    it('rejects a response that drops a batch item', async () => {
        const route = resolveNativeTokenizerRoute('o200k_base', true, true)
        if (route.kind !== 'native-tiktoken') throw new Error('candidate route was not selected')
        const invoke = vi.fn(async () => ({
            mode: 'count',
            artifact_fingerprint: route.fingerprint,
            counts: [1],
        }))

        await expect(
            invokeNativeTokenizerBatch(route, ['hello', 'world'], 'count', invoke),
        ).rejects.toMatchObject({ code: 'response_length_mismatch' })
    })
})
