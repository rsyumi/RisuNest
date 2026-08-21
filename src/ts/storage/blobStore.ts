export type BlobKind = 'asset' | 'inlay'
export type InlayBlobType = 'image' | 'video' | 'audio' | 'signature'

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
    legacyAssetPrefix: string
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
        legacyAssetPrefix: 'assets/',
    }

    let initialized: Promise<void> | undefined
    const initialize = () => initialized ??= (async () => {
        const allKeys = await backend.keys()
        const keySet = new Set(allKeys)
        for (const key of allKeys.filter((value) => value.startsWith(keys.legacyAssetPrefix))) {
            const logicalKey = key.slice(keys.legacyAssetPrefix.length - 'assets/'.length)
            const metadataKey = keys.metadata(logicalKey)
            if (keySet.has(metadataKey)) continue
            const payloadSize = backend.size ? await backend.size(key) : (await backend.read(key))?.byteLength ?? null
            if (payloadSize === null) continue
            const ext = normalizeBlobExtension(logicalKey.split('.').pop() ?? '')
            const metadata: BlobMetadata = {
                key: logicalKey,
                kind: 'asset',
                size: payloadSize,
                mime: inferBlobMime(undefined, ext),
                name: logicalKey.split('/').pop() ?? logicalKey,
                ext,
            }
            await backend.write(metadataKey, new TextEncoder().encode(JSON.stringify(metadata)))
        }
    })()

    async function stat(key: string): Promise<BlobMetadata | null> {
        await initialize()
        const metadata = parseMetadata(await backend.read(keys.metadata(key)))
        if (!metadata) return null
        const allKeys = await backend.keys()
        return allKeys.includes(keys.payload(key)) ? metadata : null
    }

    return {
        async put(key, data, input) {
            await initialize()
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
            await initialize()
            if (range) validateBlobReadRange(range)
            const payloadKey = keys.payload(key)
            if (range && backend.readRange) return backend.readRange(payloadKey, range)
            const data = await backend.read(payloadKey)
            if (!data) {
                const metadata = parseMetadata(await backend.read(keys.metadata(key)))
                if (metadata?.size === 0 && (await backend.keys()).includes(payloadKey)) return new Uint8Array()
                return null
            }
            if (!range) return data
            return data.slice(Math.min(range.start, data.byteLength), Math.min(range.endExclusive, data.byteLength))
        },
        stat,
        async list(query) {
            await initialize()
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
            await initialize()
            await backend.remove(keys.payload(key))
            await backend.remove(keys.metadata(key))
        },
        async resolveUrl(key) {
            await initialize()
            if (!await stat(key)) return null
            return backend.resolveUrl?.(keys.payload(key)) ?? null
        },
    }
}
