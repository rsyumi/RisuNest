import { invoke } from '@tauri-apps/api/core'
import { nativeAssetByteSender, type NativeByteInvoke } from './nativeAssetByteTransport'

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
    PayloadSizeMismatchError,
    type ImmutablePayloadCas,
    type PreparedImmutablePayload,
} from './payloadCas'
import { createTauriCasObjectUrl, getNativeMediaEndpoint } from './platformBlobStore'
import {
    consumeBoundedNativeMediaOutput,
    invokeWithBoundedNativeMediaInput,
} from './nativeMediaIpc'

type InvokeCommand = NativeByteInvoke

export const NATIVE_CAS_IPC_CHUNK_BYTES = 64 * 1024
/** The largest part one native CAS read returns; native enforces the same value. */
export const NATIVE_CAS_READ_PART_BYTES = 1024 * 1024

// Keep upload admission aligned with CasUploadPool in asset_repository/commands.rs.
const MAX_CAS_UPLOAD_BYTES = 512 * 1024 * 1024
const MAX_CONCURRENT_CAS_UPLOADS = 4
const MAX_TOTAL_CAS_UPLOAD_BYTES = 1024 * 1024 * 1024

export interface NativeCasUploadBudgetOptions {
    maxUploads?: number
    capacityBytes?: number
}

export interface NativeCasUploadBudget {
    run<T>(byteSize: number, upload: () => Promise<T>): Promise<T>
}

/** Shares native upload slots and byte reservations between ownership sessions, in arrival order. */
export function createNativeCasUploadBudget(options: NativeCasUploadBudgetOptions = {}): NativeCasUploadBudget {
    const maxUploads = options.maxUploads ?? MAX_CONCURRENT_CAS_UPLOADS
    const capacity = options.capacityBytes ?? MAX_TOTAL_CAS_UPLOAD_BYTES
    if (!Number.isSafeInteger(maxUploads) || maxUploads < 1 || maxUploads > MAX_CONCURRENT_CAS_UPLOADS
        || !Number.isSafeInteger(capacity) || capacity < 1 || capacity > MAX_TOTAL_CAS_UPLOAD_BYTES) {
        throw new RangeError('Native CAS upload budget exceeds the native limits')
    }
    let active = 0
    let reservedBytes = 0
    const waiting: { byteSize: number, admit: () => void }[] = []
    const admitWaiting = () => {
        while (waiting.length > 0 && active < maxUploads && waiting[0].byteSize <= capacity - reservedBytes) {
            const next = waiting.shift()!
            active++
            reservedBytes += next.byteSize
            next.admit()
        }
    }
    return {
        async run(byteSize, upload) {
            if (!Number.isSafeInteger(byteSize) || byteSize < 0 || byteSize > Math.min(capacity, MAX_CAS_UPLOAD_BYTES)) {
                throw new RangeError('Native CAS upload size is outside the supported range')
            }
            await new Promise<void>(admit => {
                waiting.push({ byteSize, admit })
                admitWaiting()
            })
            try {
                return await upload()
            } finally {
                active--
                reservedBytes -= byteSize
                admitWaiting()
            }
        },
    }
}

const documentUploadBudget = createNativeCasUploadBudget()

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

function byteValues(value: unknown, context: string): Uint8Array | number[] {
    if (value instanceof ArrayBuffer) return new Uint8Array(value)
    if (value instanceof Uint8Array) return value
    if (!Array.isArray(value) || !value.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)) {
        throw new TypeError(`${context} must return byte values`)
    }
    return value
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
    budget: NativeCasUploadBudget = documentUploadBudget,
): Promise<PreparedImmutablePayload> {
    pinSessionId(sessionId, 'Native CAS job prepare')
    if (data.byteLength <= NATIVE_CAS_IPC_CHUNK_BYTES) {
        return preparedPayload(await invokeCommand('asset_cas_job_prepare', {
            sessionId,
            data: Array.from(data),
            role,
        }), 'Native CAS job prepare')
    }
    return budget.run(data.byteLength, async () => {
        const uploadId = crypto.randomUUID()
        const sender = await nativeAssetByteSender(invokeCommand)
        try {
            const opened = await invokeCommand('asset_cas_job_upload_open', {
                uploadId,
                sessionId,
                role,
                totalBytes: data.byteLength,
                binary: sender.binary,
            }) as { capacity?: unknown }
            if (opened.capacity !== sender.capacity) {
                throw new TypeError('Native CAS upload returned an invalid chunk capacity')
            }
            for (let offset = 0; offset < data.byteLength;) {
                const end = Math.min(offset + sender.capacity, data.byteLength)
                const acknowledged = safeSize(await sender.append('asset_cas_job_upload_chunk',
                    uploadId, offset, data.subarray(offset, end)), 'Native CAS upload chunk')
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
    })
}

/** A native CAS read whose response did not arrive within the stall limit. */
export class NativeCasReadStallError extends Error {
    constructor() {
        super('Native CAS read did not complete')
        this.name = 'NativeCasReadStallError'
    }
}

export interface NativeCasReadBudgetOptions {
    capacityBytes?: number
    minimumWeightBytes?: number
    stallMs?: number
}

export interface NativeCasReadBudget {
    run(length: number, read: () => Promise<unknown>): Promise<unknown>
}

/**
 * Bounds the read responses a document has in flight. An invoke settles only
 * after the page has received its body, so holding weight until then also
 * covers bodies still waiting for delivery. Waiters are served in order.
 */
export function createNativeCasReadBudget(options: NativeCasReadBudgetOptions = {}): NativeCasReadBudget {
    const capacity = options.capacityBytes ?? 8 * 1024 * 1024
    const minimumWeight = options.minimumWeightBytes ?? 256 * 1024
    const stallMs = options.stallMs ?? 30_000
    let available = capacity
    const waiting: { weight: number, admit: () => void }[] = []
    const admitWaiting = () => {
        while (waiting.length > 0 && waiting[0].weight <= available) {
            const next = waiting.shift()!
            available -= next.weight
            next.admit()
        }
    }
    return {
        async run(length, read) {
            const weight = Math.min(capacity, Math.max(length, minimumWeight))
            await new Promise<void>((admit) => {
                waiting.push({ weight, admit })
                admitWaiting()
            })
            let released = false
            const release = () => {
                if (released) return
                released = true
                available += weight
                admitWaiting()
            }
            const invoked = new Promise<unknown>((resolve) => resolve(read()))
            invoked.then(release, release)
            let timer: ReturnType<typeof setTimeout> | undefined
            // Android leaves an invoke unsettled when its channel fetch fails. The
            // read fails without a retry, and a late result is discarded.
            const stalled = new Promise<never>((_, reject) => {
                timer = setTimeout(() => {
                    release()
                    reject(new NativeCasReadStallError())
                }, stallMs)
            })
            try {
                return await Promise.race([invoked, stalled])
            } finally {
                clearTimeout(timer)
            }
        },
    }
}

const documentReadBudget = createNativeCasReadBudget()

function expectedObjectSize(value: number): number {
    if (!Number.isSafeInteger(value) || value < 0) {
        throw new TypeError('Expected object size must be a nonnegative safe integer')
    }
    return value
}

async function readCasPart(
    invokeCommand: InvokeCommand,
    budget: NativeCasReadBudget,
    contentHash: string,
    start: number,
    endExclusive: number,
    expectedSize: number,
): Promise<Uint8Array | null> {
    let result: unknown
    try {
        result = await budget.run(endExclusive - start, () => invokeCommand('asset_cas_read_bytes', {
            contentHash, start, endExclusive, expectedSize,
        }))
    } catch (error) {
        if (error === 'cas-object-missing') return null
        if (error === 'cas-object-size-mismatch') throw new PayloadSizeMismatchError()
        throw error
    }
    const values = byteValues(result, 'Native CAS read')
    if (values.length !== endExclusive - start) {
        throw new TypeError('Native CAS read returned an invalid length')
    }
    return values instanceof Uint8Array ? values : Uint8Array.from(values)
}

/** Reads `[start, end)` clamped to an object of `expectedSize` bytes, one part at a time. */
async function readCasObjectRange(
    invokeCommand: InvokeCommand,
    budget: NativeCasReadBudget,
    contentHash: string,
    requestedStart: number,
    requestedEnd: number,
    expectedSize: number,
): Promise<Uint8Array | null> {
    const start = Math.min(requestedStart, expectedSize)
    const end = Math.min(requestedEnd, expectedSize)
    const firstEnd = Math.min(end, start + NATIVE_CAS_READ_PART_BYTES)
    // An empty range still makes one call, which reports absence.
    const first = await readCasPart(invokeCommand, budget, contentHash, start, firstEnd, expectedSize)
    if (first === null || firstEnd === end) return first
    // The first part has confirmed the object and its size before the result is reserved.
    const combined = new Uint8Array(end - start)
    combined.set(first)
    for (let offset = firstEnd; offset < end;) {
        const partEnd = Math.min(end, offset + NATIVE_CAS_READ_PART_BYTES)
        const part = await readCasPart(invokeCommand, budget, contentHash, offset, partEnd, expectedSize)
        if (part === null) return null
        combined.set(part, offset - start)
        offset = partEnd
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
    budget: NativeCasReadBudget = documentReadBudget,
): ImmutablePayloadCas {
    const objectSize = async (contentHash: string, expectedSize: number | undefined) => {
        if (expectedSize !== undefined) return expectedObjectSize(expectedSize)
        const size = await invokeCommand('asset_cas_stat_object', { contentHash })
        return size === null ? null : safeSize(size, 'Native CAS stat')
    }
    return {
        async prepare(data) {
            void data
            throw new Error('Native CAS writes require a durable ownership session')
        },
        async readObject(contentHash, expectedSize) {
            const size = await objectSize(contentHash, expectedSize)
            if (size === null) return null
            return readCasObjectRange(invokeCommand, budget, contentHash, 0, size, size)
        },
        async readObjectRange(contentHash, range, expectedSize) {
            validateBlobReadRange(range)
            const size = await objectSize(contentHash, expectedSize)
            if (size === null) return null
            return readCasObjectRange(
                invokeCommand,
                budget,
                contentHash,
                range.start,
                range.endExclusive,
                size,
            )
        },
        async statObject(contentHash) {
            const result = await invokeCommand('asset_cas_stat_object', { contentHash })
            return result === null ? null : safeSize(result, 'Native CAS stat')
        },
        statObjects: hashes => statNativeObjects(invokeCommand, 'asset_cas_stat_objects', hashes),
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
    budget: NativeCasReadBudget = documentReadBudget,
): RemoteAssetReader {
    return {
        statObjects: hashes => statNativeObjects(invokeCommand, 'asset_remote_stat_objects', hashes),
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
            const hydrated = await invokeCommand('asset_remote_hydrate_object', { contentHash })
            if (hydrated === null) return null
            const size = safeSize(hydrated, 'Native remote asset hydration')
            return readCasObjectRange(
                invokeCommand,
                budget,
                contentHash,
                range?.start ?? 0,
                range?.endExclusive ?? size,
                size,
            )
        },
    }
}

async function statNativeObjects(invokeCommand: InvokeCommand, command: string, hashes: readonly string[]): Promise<(number | null)[]> {
    const sizes: (number | null)[] = []
    for (let start = 0; start < hashes.length; start += 512) {
        const contentHashes = hashes.slice(start, start + 512)
        contentHashes.forEach(objectPhysicalKey)
        const result = await invokeCommand(command, { contentHashes })
        if (!Array.isArray(result) || result.length !== contentHashes.length) throw new Error('Native asset batch stat response mismatch')
        sizes.push(...result.map(size => size === null ? null : safeSize(size, 'Native asset batch stat')))
    }
    return sizes
}
interface NativeEncodedInlayImage {
    data?: unknown
    outputId?: unknown
    outputSize?: unknown
    sourceKept?: unknown
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
                data,
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
                    preservationReason: encoded.metadata.preservationReason,
                },
            }
        },
    }
}
