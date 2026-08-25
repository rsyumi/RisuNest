import type { InlayBlobType, InlayBlobMetadata } from 'src/ts/storage/blobStore'
import { getInlayAssetBlob, getInlayAssetMetadata, getInlayAssetRenderUrl } from './inlays'

export interface InlayRenderSource {
    url: string
    mime: string
    type: InlayBlobType
    name: string
    size: number
    width?: number
    height?: number
    objectUrl: boolean
}

const nativeThumbnailMimes = new Set([
    'image/gif',
    'image/jpeg',
    'image/png',
    'image/webp',
])

export function getNativeInlayThumbnailSize(
    metadata: Pick<InlayBlobMetadata, 'inlayType' | 'mime'>,
    native: boolean,
): 256 | undefined {
    return native && metadata.inlayType === 'image' && nativeThumbnailMimes.has(metadata.mime.toLowerCase())
        ? 256
        : undefined
}

function escapeHtmlAttribute(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
}

export function renderInlaySourceMarkup(source: InlayRenderSource): string {
    const url = escapeHtmlAttribute(source.url)
    const mime = escapeHtmlAttribute(source.mime)
    switch (source.type) {
        case 'image':
            return `<img src="${url}"/>`
        case 'video':
            return `<video controls><source src="${url}" type="${mime}"></video>`
        case 'audio':
            return `<audio controls><source src="${url}" type="${mime}"></audio>`
        default:
            return ''
    }
}

function sourceFromMetadata(metadata: InlayBlobMetadata, url: string, objectUrl: boolean): InlayRenderSource {
    return {
        url,
        mime: metadata.mime,
        type: metadata.inlayType,
        name: metadata.name,
        size: metadata.size,
        ...(metadata.width === undefined ? {} : { width: metadata.width }),
        ...(metadata.height === undefined ? {} : { height: metadata.height }),
        objectUrl,
    }
}

export async function getInlayRenderSource(
    id: string,
    native: boolean,
    thumbnailSize?: 128 | 256 | 512,
    knownMetadata?: InlayBlobMetadata,
): Promise<InlayRenderSource | null> {
    if (native) {
        const metadata = knownMetadata
            ?? await getInlayAssetMetadata(id, { migrateLegacy: false })
        if (!metadata) return null
        const url = await getInlayAssetRenderUrl(id, thumbnailSize)
        return url ? sourceFromMetadata(metadata, url, false) : null
    }

    const asset = await getInlayAssetBlob(id)
    if (!asset) return null
    return {
        url: URL.createObjectURL(asset.data),
        mime: asset.data.type || knownMetadata?.mime || 'application/octet-stream',
        type: asset.type,
        name: asset.name,
        size: asset.data.size,
        ...(asset.width === undefined ? {} : { width: asset.width }),
        ...(asset.height === undefined ? {} : { height: asset.height }),
        objectUrl: true,
    }
}

export async function getInlayRenderSources(
    ids: Iterable<string>,
    native: boolean,
): Promise<Map<string, InlayRenderSource | null>> {
    const uniqueIds = [...new Set(ids)]
    const sources = await Promise.all(uniqueIds.map(async (id) => [
        id,
        await getInlayRenderSource(id, native),
    ] as const))
    return new Map(sources)
}
