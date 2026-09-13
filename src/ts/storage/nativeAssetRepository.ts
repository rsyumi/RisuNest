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

type InvokeCommand = (command: string, args?: Record<string, unknown>) => Promise<unknown>

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
    return preparedPayload(await invokeCommand('asset_cas_job_prepare', {
        sessionId,
        data: Array.from(data),
        role,
    }), 'Native CAS job prepare')
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
    data: unknown
    metadata: InlayBlobMetadata
}

function sourceInlayMime(data: Uint8Array): { mime: string, ext: string } | null {
    if (data.byteLength >= 8 && data.slice(0, 8).every((byte, index) => byte === [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a][index])) {
        return { mime: 'image/png', ext: 'png' }
    }
    if (data.byteLength >= 3 && data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff) return { mime: 'image/jpeg', ext: 'jpg' }
    if (data.byteLength >= 12 && new TextDecoder().decode(data.subarray(0, 4)) === 'RIFF'
        && new TextDecoder().decode(data.subarray(8, 12)) === 'WEBP') return { mime: 'image/webp', ext: 'webp' }
    return null
}

function expectedNativeInlayOutput(options: InlayEncodeOptions, source: Uint8Array): { mime: string, ext: string } | null {
    if (options.format === 'webp') return { mime: 'image/webp', ext: 'webp' }
    if (options.format === 'png') return { mime: 'image/png', ext: 'png' }
    return sourceInlayMime(source)
}

export function createNativeNewInlayImageEncoder(
    invokeCommand: InvokeCommand = invoke,
): NewInlayImageEncoder {
    return {
        async encodeNewInlayImage(key, data, input) {
            const options = normalizeInlayEncodeOptions(input.options ?? defaultInlayEncodeOptions)
            const result = await invokeCommand(
                'native_media_encode_inlay_image',
                {
                    id: key,
                    data: Array.from(data),
                    name: input.name,
                    ...(input.options === undefined ? {} : { options }),
                },
            ) as NativeEncodedInlayImage
            const metadata = result.metadata
            const expected = expectedNativeInlayOutput(options, data)
            if (
                metadata.kind !== 'inlay'
                || metadata.key !== key
                || !expected
                || metadata.mime !== expected.mime
                || metadata.ext !== expected.ext
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
