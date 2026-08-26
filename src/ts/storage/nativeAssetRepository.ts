import { invoke } from '@tauri-apps/api/core'

import type { InlayBlobMetadata } from './blobStore'
import { validateBlobReadRange } from './blobStore'
import type { AssetObjectUrlResolver, NewInlayImageEncoder } from './assetRepository'
import {
    objectPhysicalKey,
    type ImmutablePayloadCas,
    type PreparedImmutablePayload,
} from './payloadCas'
import { createTauriCasObjectUrl } from './platformBlobStore'

type InvokeCommand = (command: string, args?: Record<string, unknown>) => Promise<unknown>

function bytes(value: unknown, context: string): Uint8Array {
    if (value instanceof Uint8Array) return value.slice()
    if (!Array.isArray(value) || !value.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
        throw new TypeError(`${context} must return byte values`)
    }
    return Uint8Array.from(value)
}

function safeSize(value: unknown, context: string): number {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
        throw new TypeError(`${context} must return a nonnegative safe integer`)
    }
    return value
}

export function createNativeImmutablePayloadCas(
    invokeCommand: InvokeCommand = invoke,
): ImmutablePayloadCas {
    return {
        async prepare(data) {
            const result = await invokeCommand('asset_cas_prepare', {
                data: Array.from(data),
            }) as PreparedImmutablePayload
            safeSize(result.byteSize, 'Native CAS prepare')
            if (result.physicalKey !== objectPhysicalKey(result.contentHash)) {
                throw new TypeError('Native CAS prepare returned an invalid object identity')
            }
            if (typeof result.deduplicated !== 'boolean') {
                throw new TypeError('Native CAS prepare returned an invalid deduplication result')
            }
            return result
        },
        async readObject(contentHash) {
            const result = await invokeCommand('asset_cas_read_object', { contentHash })
            return result === null ? null : bytes(result, 'Native CAS read')
        },
        async readObjectRange(contentHash, range) {
            validateBlobReadRange(range)
            const result = await invokeCommand('asset_cas_read_object_range', {
                contentHash,
                start: range.start,
                endExclusive: range.endExclusive,
            })
            return result === null ? null : bytes(result, 'Native CAS range read')
        },
        async statObject(contentHash) {
            const result = await invokeCommand('asset_cas_stat_object', { contentHash })
            return result === null ? null : safeSize(result, 'Native CAS stat')
        },
    }
}

export function createNativeAssetObjectUrlResolver(): AssetObjectUrlResolver {
    return {
        async resolveObjectUrl(input) {
            return createTauriCasObjectUrl(input)
        },
    }
}

interface NativeEncodedInlayImage {
    data: unknown
    metadata: InlayBlobMetadata
}

export function createNativeNewInlayImageEncoder(
    invokeCommand: InvokeCommand = invoke,
): NewInlayImageEncoder {
    return {
        async encodeNewInlayImage(key, data, input) {
            const result = await invokeCommand(
                'native_media_encode_inlay_image',
                { id: key, data: Array.from(data), name: input.name },
            ) as NativeEncodedInlayImage
            const metadata = result.metadata
            if (
                metadata.kind !== 'inlay'
                || metadata.key !== key
                || metadata.mime !== 'image/webp'
                || metadata.ext !== 'webp'
                || metadata.inlayType !== 'image'
                || !Number.isSafeInteger(metadata.width)
                || !Number.isSafeInteger(metadata.height)
            ) {
                throw new TypeError('Native Inlay encoder returned invalid metadata')
            }
            const encoded = bytes(result.data, 'Native Inlay encoder')
            if (metadata.size !== encoded.byteLength) {
                throw new TypeError('Native Inlay encoder size does not match its bytes')
            }
            return {
                data: encoded,
                metadata: {
                    kind: 'inlay',
                    mime: metadata.mime,
                    name: metadata.name,
                    ext: metadata.ext,
                    inlayType: metadata.inlayType,
                    width: metadata.width,
                    height: metadata.height,
                },
            }
        },
    }
}
