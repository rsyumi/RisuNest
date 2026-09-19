export type BlobKind = 'asset' | 'inlay'
export type InlayBlobType = 'image' | 'video' | 'audio' | 'signature'
export type InlayEncodeFormat = 'webp' | 'png' | 'original'

export interface InlayEncodeOptions {
    format: InlayEncodeFormat
    quality: number
    maxDimension: number
    skipReencode: boolean
    /** Frames per second an animation is thinned down to, or 0 to keep its own rate. */
    animationMaxFps: number
    animationDecodeBytes?: number
}

export const MAX_INLAY_DIMENSION = 0xffff_ffff
export const MAX_INLAY_ANIMATION_FPS = 240

export const defaultInlayEncodeOptions: InlayEncodeOptions = {
    format: 'webp', quality: 85, maxDimension: 0, skipReencode: true, animationMaxFps: 0, animationDecodeBytes: 256 * 1024 * 1024,
}

export function normalizeInlayEncodeOptions(
    input: Partial<InlayEncodeOptions> | undefined,
): InlayEncodeOptions {
    const format = input?.format === 'png' || input?.format === 'original'
        ? input.format
        : defaultInlayEncodeOptions.format
    const quality = Math.min(100, Math.max(1, Math.round(
        Number.isFinite(input?.quality) ? input!.quality! : defaultInlayEncodeOptions.quality,
    )))
    const maxDimension = Math.min(MAX_INLAY_DIMENSION, Math.max(0, Math.round(
        Number.isFinite(input?.maxDimension) ? input!.maxDimension! : defaultInlayEncodeOptions.maxDimension,
    )))
    const skipReencode = typeof input?.skipReencode === 'boolean' ? input.skipReencode : defaultInlayEncodeOptions.skipReencode
    const animationMaxFps = Math.min(MAX_INLAY_ANIMATION_FPS, Math.max(0, Math.round(
        Number.isFinite(input?.animationMaxFps) ? input!.animationMaxFps! : defaultInlayEncodeOptions.animationMaxFps,
    )))
    const animationDecodeBytes = Math.min(512 * 1024 * 1024, Math.max(1, Math.floor(
        Number.isFinite(input?.animationDecodeBytes) ? input!.animationDecodeBytes! : defaultInlayEncodeOptions.animationDecodeBytes!,
    )))
    return { format, quality, maxDimension, skipReencode, animationMaxFps, animationDecodeBytes }
}

export interface AssetBlobMetadata {
    key: string
    kind: 'asset'
    size: number
    mime: string
    name: string
    ext: string
}

export interface InlayBlobMetadata {
    key: string
    kind: 'inlay'
    size: number
    mime: string
    name: string
    ext: string
    inlayType: InlayBlobType
    width?: number
    height?: number
    preservationReason?: 'animation-cost'
}

export type BlobMetadata = AssetBlobMetadata | InlayBlobMetadata
export type BlobWriteMetadata =
    | Omit<AssetBlobMetadata, 'key' | 'size'>
    | Omit<InlayBlobMetadata, 'key' | 'size'>

export interface BlobReadRange {
    start: number
    endExclusive: number
}

export interface BlobListQuery {
    kind?: BlobKind
}

export interface BlobStore {
    put(key: string, data: Uint8Array, metadata: BlobWriteMetadata): Promise<BlobMetadata>
    putNewInlayImage?(
        key: string,
        data: Uint8Array,
        input: { name: string, options?: InlayEncodeOptions },
    ): Promise<InlayBlobMetadata>
    read(key: string, range?: BlobReadRange): Promise<Uint8Array | null>
    stat(key: string): Promise<BlobMetadata | null>
    list(query?: BlobListQuery): Promise<BlobMetadata[]>
    remove(key: string): Promise<void>
    resolveUrl(key: string): Promise<string | null>
}

export interface BlobKeyValueBackend {
    write(key: string, value: Uint8Array): Promise<void>
    read(key: string): Promise<Uint8Array | null>
    keys(): Promise<string[]>
    remove(key: string): Promise<void>
    size?(key: string): Promise<number | null>
    readRange?(key: string, range: BlobReadRange): Promise<Uint8Array | null>
    resolveUrl?(key: string): Promise<string | null>
}

export interface BlobPhysicalKeyMapper {
    payload(key: string): string
    metadata(key: string): string
    metadataPrefix: string
}

const mimeByExtension: Record<string, string> = {
    avif: 'image/avif', gif: 'image/gif', jpeg: 'image/jpeg', jpg: 'image/jpeg', png: 'image/png', webp: 'image/webp',
    flac: 'audio/flac', mp3: 'audio/mpeg', ogg: 'audio/ogg', wav: 'audio/wav',
    mkv: 'video/x-matroska', mp4: 'video/mp4', webm: 'video/webm', json: 'application/json',
}

export function normalizeBlobExtension(ext: string): string {
    return ext.replace(/^\.+/, '').toLowerCase()
}

export function inferBlobMime(mime: string | undefined, ext: string): string {
    const normalizedMime = mime?.trim()
    return normalizedMime || mimeByExtension[normalizeBlobExtension(ext)] || 'application/octet-stream'
}

export function validateBlobReadRange(range: BlobReadRange): void {
    if (!Number.isFinite(range.start) || !Number.isInteger(range.start) || range.start < 0
        || !Number.isFinite(range.endExclusive) || !Number.isInteger(range.endExclusive)
        || range.endExclusive < range.start) {
        throw new RangeError('Blob range must contain nonnegative integer bounds in ascending order')
    }
}

function parseMetadata(value: Uint8Array | null): BlobMetadata | null {
    if (!value) return null
    try {
        const parsed = JSON.parse(new TextDecoder().decode(value)) as BlobMetadata
        if (!parsed || typeof parsed.key !== 'string' || (parsed.kind !== 'asset' && parsed.kind !== 'inlay')) return null
        return parsed
    } catch {
        return null
    }
}

export function createKeyValueBlobStore(
    backend: BlobKeyValueBackend,
    mapper: BlobPhysicalKeyMapper | { kind: 'legacy' },
): BlobStore {
    const keys = 'payload' in mapper ? mapper : {
        payload: (key: string) => key.startsWith('assets/') ? key : `blobstore/inlays/${Buffer.from(key).toString('hex')}.bin`,
        metadata: (key: string) => `blobstore/metadata/${Buffer.from(key).toString('hex')}.json`,
        metadataPrefix: 'blobstore/metadata/',
    }

    async function payloadExists(payloadKey: string): Promise<boolean> {
        if (backend.size) return await backend.size(payloadKey) !== null
        return (await backend.keys()).includes(payloadKey)
    }

    async function stat(key: string): Promise<BlobMetadata | null> {
        const metadata = parseMetadata(await backend.read(keys.metadata(key)))
        if (!metadata) return null
        return await payloadExists(keys.payload(key)) ? metadata : null
    }

    return {
        async put(key, data, input) {
            const ext = normalizeBlobExtension(input.ext)
            const metadata = {
                ...input,
                key,
                size: data.byteLength,
                ext,
                mime: inferBlobMime(input.mime, ext),
            } as BlobMetadata
            await backend.write(keys.payload(key), data)
            await backend.write(keys.metadata(key), new TextEncoder().encode(JSON.stringify(metadata)))
            return metadata
        },
        async read(key, range) {
            if (range) validateBlobReadRange(range)
            // Reading the payload is itself the liveness check, so no separate probe is needed.
            const metadata = parseMetadata(await backend.read(keys.metadata(key)))
            if (!metadata) return null
            const payloadKey = keys.payload(key)
            if (range && backend.readRange) return backend.readRange(payloadKey, range)
            const data = await backend.read(payloadKey)
            if (!data) {
                if (metadata.size === 0 && await payloadExists(payloadKey)) return new Uint8Array()
                return null
            }
            if (!range) return data
            return data.slice(Math.min(range.start, data.byteLength), Math.min(range.endExclusive, data.byteLength))
        },
        stat,
        async list(query) {
            const allKeys = await backend.keys()
            const keySet = new Set(allKeys)
            const results: BlobMetadata[] = []
            for (const metadataKey of allKeys.filter((key) => key.startsWith(keys.metadataPrefix))) {
                const metadata = parseMetadata(await backend.read(metadataKey))
                if (!metadata || (query?.kind && metadata.kind !== query.kind)) continue
                if (keySet.has(keys.payload(metadata.key))) results.push(metadata)
            }
            return results.sort((left, right) => left.key.localeCompare(right.key))
        },
        async remove(key) {
                await backend.remove(keys.payload(key))
            await backend.remove(keys.metadata(key))
        },
        async resolveUrl(key) {
                if (!await stat(key)) return null
            return backend.resolveUrl?.(keys.payload(key)) ?? null
        },
    }
}
