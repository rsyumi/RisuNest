import { invoke } from '@tauri-apps/api/core'

export type NativeTokenizerId = 'cl100k_base' | 'o200k_base'
export type NativeTokenizeMode = 'count' | 'ids'

export const NATIVE_TOKENIZER_FINGERPRINTS = {
    cl100k_base:
        'cl100k_base:dqbd-1.0.22:49a4e05dea02c8fafbd50cc4725c4aab8f39386c0afedef118e0dfebc2fe523a:tiktoken-rs-0.12.0:contract-1',
    o200k_base:
        'o200k_base:dqbd-1.0.22:a2c363f80642c0f07d916716b3940ff030121f3b5cef72b2c5bd1d4f64c14fb8:tiktoken-rs-0.12.0:contract-1',
} as const satisfies Record<NativeTokenizerId, string>

// General routing stays disabled. Measured production seams opt in explicitly.
export const NATIVE_TOKENIZER_PRODUCTION_ENABLED = false

export type ResolvedTokenizerRoute =
    | {
          kind: 'native-tiktoken'
          tokenizerId: NativeTokenizerId
          fingerprint: string
      }
    | {
          kind: 'existing'
          tokenizerId: string
      }

export type NativeTokenizeBatchResponse =
    | { mode: 'count'; artifact_fingerprint: string; counts: number[] }
    | { mode: 'ids'; artifact_fingerprint: string; ids: number[][] }

export type NativeTokenizerInvoke = (
    command: string,
    args: Record<string, unknown>,
) => Promise<unknown>

export class NativeTokenizerBoundaryError extends Error {
    constructor(
        readonly code: string,
        message: string,
    ) {
        super(message)
        this.name = 'NativeTokenizerBoundaryError'
    }
}

function isNativeTokenizerId(tokenizerId: string): tokenizerId is NativeTokenizerId {
    return tokenizerId === 'cl100k_base' || tokenizerId === 'o200k_base'
}

export function resolveNativeTokenizerRoute(
    tokenizerId: string,
    isTauri: boolean,
    candidateEnabled = NATIVE_TOKENIZER_PRODUCTION_ENABLED,
): ResolvedTokenizerRoute {
    if (!candidateEnabled || !isTauri || !isNativeTokenizerId(tokenizerId)) {
        return { kind: 'existing', tokenizerId }
    }
    return {
        kind: 'native-tiktoken',
        tokenizerId,
        fingerprint: NATIVE_TOKENIZER_FINGERPRINTS[tokenizerId],
    }
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

export async function invokeNativeTokenizerBatch(
    route: Extract<ResolvedTokenizerRoute, { kind: 'native-tiktoken' }>,
    texts: string[],
    mode: NativeTokenizeMode,
    invokeCommand: NativeTokenizerInvoke = invoke,
): Promise<NativeTokenizeBatchResponse> {
    const response = (await invokeCommand('tokenize_batch', {
        request: {
            tokenizer_id: route.tokenizerId,
            artifact_fingerprint: route.fingerprint,
            mode,
            texts: texts.map(normalizeUtf16),
        },
    })) as NativeTokenizeBatchResponse

    if (response?.artifact_fingerprint !== route.fingerprint) {
        throw new NativeTokenizerBoundaryError(
            'artifact_fingerprint_mismatch',
            'The native tokenizer returned a different artifact fingerprint.',
        )
    }
    if (response.mode !== mode) {
        throw new NativeTokenizerBoundaryError(
            'response_mode_mismatch',
            'The native tokenizer returned a different response mode.',
        )
    }
    const responseLength =
        response.mode === 'count' ? response.counts?.length : response.ids?.length
    if (responseLength !== texts.length) {
        throw new NativeTokenizerBoundaryError(
            'response_length_mismatch',
            'The native tokenizer returned a different number of batch items.',
        )
    }
    return response
}
