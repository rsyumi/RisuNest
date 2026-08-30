import { invoke } from '@tauri-apps/api/core'

import type { InlayBlobMetadata } from './blobStore'
import { validateBlobReadRange } from './blobStore'
import type {
    AssetObjectUrlResolver,
    DurableAssetWriteSessionFactory,
    NewInlayImageEncoder,
} from './assetRepository'
import {
    objectPhysicalKey,
    type ImmutablePayloadCas,
    type PreparedImmutablePayload,
} from './payloadCas'
import { createTauriCasObjectUrl } from './platformBlobStore'

type InvokeCommand = (command: string, args?: Record<string, unknown>) => Promise<unknown>

export type NativeCasJobKind =
    | 'direct-asset-or-inlay-write'
    | 'local-backup-restore'
    | 'lossless-import'
    | 'card-or-module-content-import'
    | 'official-publication-or-export-preparation'
    | 'peer-clone'
    | 'android-clone'
    | 'logical-delta-target'
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
