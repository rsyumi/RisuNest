import { invoke } from '@tauri-apps/api/core'

import {
    defaultInlayEncodeOptions,
    normalizeInlayEncodeOptions,
    type InlayBlobMetadata,
    type InlayEncodeOptions,
} from './blobStore'
import { validateBlobReadRange } from './blobStore'
import type {
    AssetObjectUrlResolver,
    DurableAssetWriteSessionFactory,
    NewInlayImageEncoder,
    RemoteAssetReader,
} from './assetRepository'
import {
    objectPhysicalKey,
    type ImmutablePayloadCas,
    type PreparedImmutablePayload,
} from './payloadCas'
import { createTauriCasObjectUrl, getNativeMediaEndpoint } from './platformBlobStore'
import {
    consumeBoundedNativeMediaOutput,
    invokeWithBoundedNativeMediaInput,
} from './nativeMediaIpc'

type InvokeCommand = (command: string, args?: Record<string, unknown>) => Promise<unknown>

export const NATIVE_CAS_IPC_CHUNK_BYTES = 64 * 1024

export type NativeCasJobKind =
    | 'direct-asset-or-inlay-write'
    | 'local-backup-restore'
    | 'lossless-import'
    | 'card-or-module-content-import'
    | 'official-publication-or-export-preparation'
    | 'cold-migration'
    | 'cold-direct-write'

export type NativeCasObjectRole = 'direct-object' | 'owner-manifest'
export type NativeCasReleaseOutcome = 'committed' | 'aborted'

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

function pinSessionId(value: unknown, context: string): string {
    if (typeof value !== 'string' || value.length === 0 || value.length > 64) {
        throw new TypeError(`${context} must return a bounded session ID`)
    }
    return value
}

function preparedPayload(value: unknown, context: string): PreparedImmutablePayload {
    const result = value as PreparedImmutablePayload
    safeSize(result.byteSize, context)
    if (result.physicalKey !== objectPhysicalKey(result.contentHash)) {
        throw new TypeError(`${context} returned an invalid object identity`)
    }
    if (typeof result.deduplicated !== 'boolean') {
        throw new TypeError(`${context} returned an invalid deduplication result`)
    }
    return result
}

export async function beginCasJob(
    kind: NativeCasJobKind,
    invokeCommand: InvokeCommand = invoke,
): Promise<string> {
    return pinSessionId(
        await invokeCommand('asset_cas_job_begin', { kind }),
        'Native CAS job begin',
    )
}

export async function prepareCasObject(
    sessionId: string,
    data: Uint8Array,
    role: NativeCasObjectRole,
    invokeCommand: InvokeCommand = invoke,
): Promise<PreparedImmutablePayload> {
    pinSessionId(sessionId, 'Native CAS job prepare')
    if (data.byteLength <= NATIVE_CAS_IPC_CHUNK_BYTES) {
        return preparedPayload(await invokeCommand('asset_cas_job_prepare', {
            sessionId,
            data: Array.from(data),
            role,
        }), 'Native CAS job prepare')
    }
    const uploadId = crypto.randomUUID()
    try {
        const opened = await invokeCommand('asset_cas_job_upload_open', {
            uploadId,
            sessionId,
            role,
            totalBytes: data.byteLength,
        }) as { capacity?: unknown }
        if (opened.capacity !== NATIVE_CAS_IPC_CHUNK_BYTES) {
            throw new TypeError('Native CAS upload returned an invalid chunk capacity')
        }
        for (let offset = 0; offset < data.byteLength;) {
            const end = Math.min(offset + NATIVE_CAS_IPC_CHUNK_BYTES, data.byteLength)
            const acknowledged = safeSize(await invokeCommand('asset_cas_job_upload_chunk', {
                uploadId,
                offset,
                data: Array.from(data.subarray(offset, end)),
            }), 'Native CAS upload chunk')
            if (acknowledged !== end) {
                throw new Error('Native CAS upload returned an invalid chunk acknowledgement')
            }
            offset = end
        }
        return preparedPayload(
            await invokeCommand('asset_cas_job_upload_finish', { uploadId }),
            'Native CAS upload finish',
        )
    } finally {
        await invokeCommand('asset_cas_job_upload_cancel', { uploadId }).catch(() => undefined)
    }
}

async function readCasObjectRange(
    invokeCommand: InvokeCommand,
    contentHash: string,
    start: number,
    endExclusive: number,
): Promise<Uint8Array | null> {
    const chunks: Uint8Array[] = []
    let total = 0
    for (let offset = start; offset < endExclusive;) {
        const end = Math.min(offset + NATIVE_CAS_IPC_CHUNK_BYTES, endExclusive)
        const requestedLength = end - offset
        const result = await invokeCommand('asset_cas_read_object_range', {
            contentHash,
            start: offset,
            endExclusive: end,
        })
        if (result === null) return null
        const chunk = bytes(result, 'Native CAS range read')
        if (chunk.byteLength > end - offset) {
            throw new TypeError('Native CAS range read exceeded its requested length')
        }
        chunks.push(chunk)
        total += chunk.byteLength
        offset += chunk.byteLength
        if (chunk.byteLength < requestedLength) break
    }
    const combined = new Uint8Array(total)
    let writeOffset = 0
    for (const chunk of chunks) {
        combined.set(chunk, writeOffset)
        writeOffset += chunk.byteLength
    }
    return combined
}

export async function pinExistingCasObject(
    sessionId: string,
    contentHash: string,
    byteSize: number,
    role: NativeCasObjectRole,
    invokeCommand: InvokeCommand = invoke,
): Promise<void> {
    pinSessionId(sessionId, 'Native CAS existing-object pin')
    objectPhysicalKey(contentHash)
    safeSize(byteSize, 'Native CAS existing-object pin')
    await invokeCommand('asset_cas_job_pin_existing', {
        sessionId,
        contentHash,
        byteSize,
        role,
    })
}

export async function sealCasJob(
    sessionId: string,
    invokeCommand: InvokeCommand = invoke,
): Promise<void> {
    pinSessionId(sessionId, 'Native CAS job seal')
    await invokeCommand('asset_cas_job_seal', { sessionId })
}

export async function finalizeContentCasJob(
    sessionId: string,
    ownerManifest: Uint8Array,
    invokeCommand: InvokeCommand = invoke,
): Promise<PreparedImmutablePayload> {
    pinSessionId(sessionId, 'Native content CAS finalizer')
    return preparedPayload(await invokeCommand('asset_cas_job_finalize_content', {
        sessionId,
        ownerManifest: Array.from(ownerManifest),
    }), 'Native content CAS finalizer')
}

export async function sealPreparedContentCasJob(
    sessionId: string,
    invokeCommand: InvokeCommand = invoke,
): Promise<void> {
    pinSessionId(sessionId, 'Native prepared content CAS seal')
    await invokeCommand('asset_cas_job_seal_prepared_content', { sessionId })
}

export async function releaseCasJob(
    sessionId: string,
    outcome: NativeCasReleaseOutcome,
    invokeCommand: InvokeCommand = invoke,
): Promise<void> {
    pinSessionId(sessionId, 'Native CAS job release')
    await invokeCommand('asset_cas_job_release', { sessionId, outcome })
}

export function createNativeDurableAssetWriteSessionFactory(
    invokeCommand: InvokeCommand = invoke,
): DurableAssetWriteSessionFactory {
    return createNativeDurableCasJobSessionFactory(
        'direct-asset-or-inlay-write',
        invokeCommand,
    )
}

export function createNativeDurableCasJobSessionFactory(
    kind: NativeCasJobKind,
    invokeCommand: InvokeCommand = invoke,
): DurableAssetWriteSessionFactory {
    return {
        async begin() {
            const sessionId = await beginCasJob(kind, invokeCommand)
            return {
                prepare: (data, role = 'direct-object') => prepareCasObject(
                    sessionId,
                    data,
                    role,
                    invokeCommand,
                ),
                seal: () => sealCasJob(sessionId, invokeCommand),
                release: (outcome) => releaseCasJob(sessionId, outcome, invokeCommand),
            }
        },
    }
}

export function createNativeImmutablePayloadCas(
    invokeCommand: InvokeCommand = invoke,
): ImmutablePayloadCas {
    return {
        async prepare(data) {
            void data
            throw new Error('Native CAS writes require a durable ownership session')
        },
        async readObject(contentHash) {
            const sizeValue = await invokeCommand('asset_cas_stat_object', { contentHash })
            if (sizeValue === null) return null
            const size = safeSize(sizeValue, 'Native CAS stat')
            if (size === 0) {
                const result = await invokeCommand('asset_cas_read_object_range', {
                    contentHash,
                    start: 0,
                    endExclusive: 0,
                })
                return result === null ? null : bytes(result, 'Native CAS range read')
            }
            const result = await readCasObjectRange(invokeCommand, contentHash, 0, size)
            if (result !== null && result.byteLength !== size) {
                throw new Error('Native CAS object changed while it was being read')
            }
            return result
        },
        async readObjectRange(contentHash, range) {
            validateBlobReadRange(range)
            if (range.start === range.endExclusive) {
                const result = await invokeCommand('asset_cas_read_object_range', {
                    contentHash,
                    start: range.start,
                    endExclusive: range.endExclusive,
                })
                return result === null ? null : bytes(result, 'Native CAS range read')
            }
            return readCasObjectRange(
                invokeCommand,
                contentHash,
                range.start,
                range.endExclusive,
            )
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
            return createTauriCasObjectUrl(input, await getNativeMediaEndpoint())
        },
    }
}

export function createNativeRemoteAssetReader(
    invokeCommand: InvokeCommand = invoke,
): RemoteAssetReader {
    return {
        async statObject(contentHash) {
            objectPhysicalKey(contentHash)
            const result = await invokeCommand('asset_remote_stat_object', {
                contentHash,
            })
            return result === null
                ? null
                : safeSize(result, 'Native remote asset stat')
        },
        async readObject(contentHash, range) {
            objectPhysicalKey(contentHash)
            if (range) validateBlobReadRange(range)
            const result = await invokeCommand('asset_remote_read_object', {
                contentHash,
                start: range?.start ?? null,
                endExclusive: range?.endExclusive ?? null,
            })
            return result === null
                ? null
                : bytes(result, 'Native remote asset read')
        },
    }
}
interface NativeEncodedInlayImage {
    data?: unknown
    outputId?: unknown
    outputSize?: unknown
    metadata: InlayBlobMetadata
}

function requestedNativeInlayOutput(options: InlayEncodeOptions): { mime: string, ext: string } | null {
    if (options.format === 'webp') return { mime: 'image/webp', ext: 'webp' }
    if (options.format === 'png') return { mime: 'image/png', ext: 'png' }
    return null
}

export function createNativeNewInlayImageEncoder(
    invokeCommand: InvokeCommand = invoke,
): NewInlayImageEncoder {
    return {
        async encodeNewInlayImage(key, data, input) {
            const options = normalizeInlayEncodeOptions(input.options ?? defaultInlayEncodeOptions)
            const requested = requestedNativeInlayOutput(options)
            const result = await invokeWithBoundedNativeMediaInput<NativeEncodedInlayImage>(
                invokeCommand,
                {
                    data,
                    directCommand: 'native_media_encode_inlay_image',
                    streamedFinishCommand: 'native_media_encode_inlay_finish',
                    args: {
                        id: key,
                        name: input.name,
                        ...(input.options === undefined ? {} : { options }),
                    },
                },
            )
            const encoded = await consumeBoundedNativeMediaOutput(
                invokeCommand,
                result,
                (value) => {
                    const metadata = value as InlayBlobMetadata
                    if (
                        typeof metadata !== 'object'
                        || metadata === null
                        || metadata.kind !== 'inlay'
                        || metadata.key !== key
                        || typeof metadata.mime !== 'string'
                        || metadata.mime.length === 0
                        || typeof metadata.ext !== 'string'
                        || metadata.inlayType !== 'image'
                        || (metadata.width !== undefined && !Number.isSafeInteger(metadata.width))
                        || (metadata.height !== undefined && !Number.isSafeInteger(metadata.height))
                    ) {
                        throw new TypeError('Native Inlay encoder returned invalid metadata')
                    }
                    return metadata
                },
            )
            if (encoded.metadata.size !== encoded.data.byteLength) {
                throw new TypeError('Native Inlay encoder size does not match its bytes')
            }
            // Either the format that was asked for, or the source kept exactly as it was.
            const asRequested = requested !== null
                && encoded.metadata.mime === requested.mime
                && encoded.metadata.ext === requested.ext
            if (!asRequested && encoded.data.byteLength !== data.byteLength) {
                throw new TypeError('Native Inlay encoder returned neither the requested format nor the original image')
            }
            return {
                data: encoded.data,
                metadata: {
                    kind: 'inlay',
                    mime: encoded.metadata.mime,
                    name: encoded.metadata.name,
                    ext: encoded.metadata.ext,
                    inlayType: encoded.metadata.inlayType,
                    width: encoded.metadata.width,
                    height: encoded.metadata.height,
                },
            }
        },
    }
}
