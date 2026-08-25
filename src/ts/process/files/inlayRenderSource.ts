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

interface DeferredInlayMarker {
    readonly id: string
    readonly type: InlayBlobType
}

export class DeferredInlayMarkerRegistry {
    readonly #markers = new Map<string, DeferredInlayMarker>()
    #nextSlot = 0
    #disposed = false

    register(id: string, type: InlayBlobType): string | undefined {
        if (this.#disposed) return undefined
        const slot = (this.#nextSlot++).toString(36)
        this.#markers.set(slot, Object.freeze({ id, type }))
        return slot
    }

    seal(slot: string): (DeferredInlayMarker & { token: string }) | undefined {
        const marker = this.#markers.get(slot)
        this.#markers.delete(slot)
        return marker ? { ...marker, token: crypto.randomUUID() } : undefined
    }

    clear(): void {
        this.#disposed = true
        this.#markers.clear()
    }
}

export function renderDeferredInlaySourceMarkup(
    id: string,
    source: InlayRenderSource,
    registry?: DeferredInlayMarkerRegistry,
): string {
    const slot = registry?.register(id, source.type)
    const marker = slot === undefined ? '' : ` data-risu-inlay-slot="${slot}"`
    const assetId = escapeHtmlAttribute(id)
    const mime = escapeHtmlAttribute(source.mime)
    switch (source.type) {
        case 'image': return `<img data-risu-inlay-id="${assetId}"${marker}/>`
        case 'video': return `<video controls><source data-risu-inlay-id="${assetId}"${marker} type="${mime}"></video>`
        case 'audio': return `<audio controls><source data-risu-inlay-id="${assetId}"${marker} type="${mime}"></audio>`
        default: return ''
    }
}

function startDeferredInlaySources(
    root: ParentNode,
    registry?: DeferredInlayMarkerRegistry,
): { cleanup: () => void, settled: Promise<void> } {
    let disposed = false
    const urls = new Set<string>()
    const markers = new Map<HTMLElement, DeferredInlayMarker & { token: string }>()
    for (const element of root.querySelectorAll<HTMLElement>('[data-risu-inlay-slot]')) {
        const slot = element.dataset.risuInlaySlot ?? ''
        const marker = registry?.seal(slot)
        if (!marker) continue
        const validKind = marker.type === 'image' && element instanceof HTMLImageElement
            || marker.type === 'video' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLVideoElement
            || marker.type === 'audio' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLAudioElement
        if (!validKind) continue
        element.removeAttribute('data-risu-inlay-slot')
        element.dataset.risuInlayToken = marker.token
        markers.set(element, marker)
    }
    registry?.clear()
    let elements = [...markers.keys()]
    const ids = [...new Set([...markers.values()].map((marker) => marker.id))]
    const settled = Promise.all(ids.map(async (id) => {
        let url: string | undefined
        try {
            const asset = await getInlayAssetBlob(id)
            if (!asset) return
            const attachable = elements.filter((element) => {
                const marker = markers.get(element)
                const validKind = marker?.type === 'image' && element instanceof HTMLImageElement
                    || marker?.type === 'video' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLVideoElement
                    || marker?.type === 'audio' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLAudioElement
                return element.isConnected
                    && element.dataset.risuInlayToken === marker?.token
                    && marker.id === id
                    && marker.type === asset.type
                    && validKind
            })
            if (disposed || attachable.length === 0) return
            url = URL.createObjectURL(asset.data)
            if (disposed) return URL.revokeObjectURL(url)
            let attached = 0
            for (const element of attachable) {
                const marker = markers.get(element)
                const validKind = marker?.type === 'image' && element instanceof HTMLImageElement
                    || marker?.type === 'video' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLVideoElement
                    || marker?.type === 'audio' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLAudioElement
                if (element.isConnected
                    && element.dataset.risuInlayToken === marker?.token
                    && marker?.id === id
                    && marker.type === asset.type
                    && validKind) {
                    element.setAttribute('src', url)
                    attached++
                    if (element instanceof HTMLSourceElement) {
                        const media = element.parentElement
                        if (media instanceof HTMLMediaElement) media.load()
                    }
                }
            }
            if (attached === 0) URL.revokeObjectURL(url)
            else urls.add(url)
        }
        catch {
            if (url && !urls.has(url)) URL.revokeObjectURL(url)
        }
    })).then(() => undefined)
    const cleanup = () => {
        if (disposed) return
        disposed = true
        for (const url of urls) URL.revokeObjectURL(url)
        urls.clear()
        registry?.clear()
        elements = []
        markers.clear()
    }
    return { cleanup, settled }
}

export function mountDeferredInlaySources(
    root: ParentNode,
    registry?: DeferredInlayMarkerRegistry,
): () => void {
    return startDeferredInlaySources(root, registry).cleanup
}

export async function resolveDeferredInlaySources(
    root: ParentNode,
    registry?: DeferredInlayMarkerRegistry,
): Promise<() => void> {
    const mounted = startDeferredInlaySources(root, registry)
    await mounted.settled
    return mounted.cleanup
}

export async function withResolvedDeferredInlaySources<T>(
    root: ParentNode,
    registry: DeferredInlayMarkerRegistry,
    callback: () => Promise<T>,
): Promise<T> {
    let cleanup = () => registry.clear()
    try {
        cleanup = await resolveDeferredInlaySources(root, registry)
        return await callback()
    }
    finally {
        cleanup()
    }
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
