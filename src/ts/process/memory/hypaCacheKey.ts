import type { HypaModel } from "./hypamemory"

/** Separates the key schemes that share one embedding cache. */
export type HypaCacheProducer = 'hypa-v1-text' | 'hypa-v2' | 'hypa-v3-group'

/** Raise this when the text handed to the embedding model changes. */
export const HYPA_PREPROCESS_VERSION = 1

export interface HypaEmbeddingIdentity {
    model: string
    endpoint: string
}

export interface HypaCacheKeyInput {
    producer: HypaCacheProducer
    content: string
    identity: HypaEmbeddingIdentity
    contextSuffix?: string
    preprocessVersion?: number
}

const encoder = new TextEncoder()

/** The custom server name and URL only identify the model when it is in use. */
export function hypaEmbeddingIdentity(
    model: HypaModel | string,
    customEmbeddingUrl?: string,
    customModel?: string,
): HypaEmbeddingIdentity {
    if (model !== 'custom') {
        return { model, endpoint: '' }
    }
    const name = customModel?.trim() || ''
    return {
        model: name ? `custom:${name}` : 'custom',
        endpoint: customEmbeddingUrl?.trim() || '',
    }
}

function joinWithSeparators(parts: string[]): Uint8Array {
    const encoded = parts.map((part) => encoder.encode(part))
    const total = encoded.reduce((sum, part) => sum + part.length, 0) + encoded.length - 1
    const bytes = new Uint8Array(total)
    let offset = 0
    for (let index = 0; index < encoded.length; index++) {
        if (index > 0) {
            bytes[offset] = 0
            offset += 1
        }
        bytes.set(encoded[index], offset)
        offset += encoded[index].length
    }
    return bytes
}

function toHex(digest: ArrayBuffer): string {
    const bytes = new Uint8Array(digest)
    let hex = ''
    for (let index = 0; index < bytes.length; index++) {
        hex += bytes[index].toString(16).padStart(2, '0')
    }
    return hex
}

export async function hypaCacheKey(input: HypaCacheKeyInput): Promise<string> {
    const bytes = joinWithSeparators([
        input.producer,
        input.content.normalize('NFC'),
        input.identity.model,
        input.identity.endpoint,
        String(input.preprocessVersion ?? HYPA_PREPROCESS_VERSION),
        input.contextSuffix ?? '',
    ])
    return toHex(await crypto.subtle.digest('SHA-256', bytes as unknown as ArrayBuffer))
}

export async function hypaCacheKeys(inputs: HypaCacheKeyInput[]): Promise<string[]> {
    return await Promise.all(inputs.map((input) => hypaCacheKey(input)))
}
