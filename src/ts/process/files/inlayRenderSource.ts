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
    readonly source: InlayRenderSource
}

export class DeferredInlayMarkerRegistry {
    readonly #markers = new Map<string, DeferredInlayMarker>()
    #nextSlot = 0
    #disposed = false

    register(id: string, source: InlayRenderSource): string | undefined {
        if (this.#disposed) return undefined
        const slot = (this.#nextSlot++).toString(36)
        this.#markers.set(slot, Object.freeze({ id, type: source.type, source: { ...source } }))
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
    const slot = registry?.register(id, source)
    const marker = slot === undefined ? '' : ` data-risu-inlay-slot="${slot}"`
    const assetId = escapeHtmlAttribute(id)
    const mime = escapeHtmlAttribute(source.mime)
    const dimensions = source.width && source.height
        ? ` width="${Math.floor(source.width)}" height="${Math.floor(source.height)}"`
        : ''
    switch (source.type) {
        case 'image': return `<img data-risu-inlay-id="${assetId}"${marker}${dimensions} loading="lazy"/>`
        case 'video': return `<video controls><source data-risu-inlay-id="${assetId}"${marker} type="${mime}"></video>`
        case 'audio': return `<audio controls><source data-risu-inlay-id="${assetId}"${marker} type="${mime}"></audio>`
        default: return ''
    }
}

function startDeferredInlaySources(
    root: ParentNode,
    registry?: DeferredInlayMarkerRegistry,
    options: { rejectOnError?: boolean, viewportAware?: boolean } = {},
): { cleanup: () => void, settled: Promise<void> } {
    let disposed = false
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
    const markerTargets = new Map<Element, HTMLElement[]>()
    for (const element of markers.keys()) {
        const target = element instanceof HTMLSourceElement && element.parentElement instanceof HTMLMediaElement
            ? element.parentElement
            : element
        const elements = markerTargets.get(target) ?? []
        elements.push(element)
        markerTargets.set(target, elements)
    }
    const originalSources = new Map<Element, Array<{ element: HTMLElement, url: string }>>()
    const isManagedOriginalUrl = (url: string) => {
        if (url.startsWith('data:') || url.startsWith('risuasset:') || url.includes('://risuasset.localhost/')) return true
        try {
            return new URL(url, document.baseURI).hostname === 'risuasset.localhost'
        }
        catch {
            return false
        }
    }
    const visibleById = new Map<string, Set<HTMLElement>>()
    const resources = new Map<string, { url: string, objectUrl: boolean }>()
    const pending = new Map<string, Promise<void>>()

    const isValid = (element: HTMLElement, marker: DeferredInlayMarker & { token: string }) => {
        const validKind = marker.type === 'image' && element instanceof HTMLImageElement
            || marker.type === 'video' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLVideoElement
            || marker.type === 'audio' && element instanceof HTMLSourceElement && element.parentElement instanceof HTMLAudioElement
        return element.isConnected
            && element.dataset.risuInlayToken === marker.token
            && validKind
    }
    const attach = (element: HTMLElement, url: string) => {
        element.setAttribute('src', url)
        if (element instanceof HTMLSourceElement && element.parentElement instanceof HTMLMediaElement) {
            element.parentElement.load()
        }
    }
    const mediaElementFor = (element: HTMLElement): HTMLMediaElement | null => {
        return element instanceof HTMLSourceElement && element.parentElement instanceof HTMLMediaElement
            ? element.parentElement
            : null
    }
    const isPlaying = (element: HTMLElement) => {
        const media = mediaElementFor(element)
        return media !== null && !media.paused && !media.ended
    }
    const detach = (element: HTMLElement, stopPlayback = false) => {
        if (element instanceof HTMLSourceElement && element.parentElement instanceof HTMLMediaElement) {
            if (stopPlayback && !element.parentElement.paused && !element.parentElement.ended) {
                element.parentElement.pause()
            }
            element.removeAttribute('src')
            element.parentElement.load()
            return
        }
        element.removeAttribute('src')
    }
    const releaseResource = (id: string) => {
        const resource = resources.get(id)
        if (resource?.objectUrl) URL.revokeObjectURL(resource.url)
        resources.delete(id)
    }
    const loadResource = (id: string) => {
        const existing = pending.get(id)
        if (existing) return existing
        const marker = [...markers.values()].find((candidate) => candidate.id === id)
        if (!marker) return Promise.resolve()
        const load = (async () => {
            let resource: { url: string, objectUrl: boolean } | undefined
            let createdObjectUrl: string | undefined
            try {
                if (marker.source.url && !marker.source.objectUrl) {
                    resource = { url: marker.source.url, objectUrl: false }
                }
                else {
                    const asset = await getInlayAssetBlob(id)
                    if (!asset || asset.type !== marker.type || disposed) return
                    const visible = visibleById.get(id)
                    if (!visible?.size) return
                    createdObjectUrl = URL.createObjectURL(asset.data)
                    resource = { url: createdObjectUrl, objectUrl: true }
                }
                if (disposed) {
                    if (createdObjectUrl) URL.revokeObjectURL(createdObjectUrl)
                    return
                }
                const visible = visibleById.get(id)
                const attachable = visible
                    ? [...visible].filter((element) => {
                        const candidate = markers.get(element)
                        return candidate?.id === id && isValid(element, candidate)
                    })
                    : []
                if (attachable.length === 0) {
                    if (createdObjectUrl) URL.revokeObjectURL(createdObjectUrl)
                    return
                }
                resources.set(id, resource)
                for (const element of attachable) attach(element, resource.url)
            }
            catch (error) {
                if (createdObjectUrl && resources.get(id)?.url !== createdObjectUrl) {
                    URL.revokeObjectURL(createdObjectUrl)
                }
                if (options.rejectOnError) throw error
            }
        })().finally(() => {
            if (pending.get(id) === load) pending.delete(id)
        })
        pending.set(id, load)
        return load
    }
    const setVisible = (element: HTMLElement, visible: boolean) => {
        const marker = markers.get(element)
        if (disposed || !marker || !isValid(element, marker)) return
        let visibleElements = visibleById.get(marker.id)
        if (visible) {
            if (!visibleElements) {
                visibleElements = new Set()
                visibleById.set(marker.id, visibleElements)
            }
            visibleElements.add(element)
            const resource = resources.get(marker.id)
            if (resource) attach(element, resource.url)
            else void loadResource(marker.id)
            return
        }
        if (isPlaying(element)) return
        detach(element)
        visibleElements?.delete(element)
        if (!visibleElements?.size) {
            visibleById.delete(marker.id)
            releaseResource(marker.id)
        }
    }

    const useViewport = options.viewportAware && typeof IntersectionObserver !== 'undefined'
    if (useViewport) {
        for (const element of root.querySelectorAll<HTMLElement>('img[src], source[src]')) {
            const url = element.getAttribute('src') ?? ''
            if (!isManagedOriginalUrl(url)) continue
            const target = element instanceof HTMLSourceElement && element.parentElement instanceof HTMLMediaElement
                ? element.parentElement
                : element
            const sources = originalSources.get(target) ?? []
            sources.push({ element, url })
            originalSources.set(target, sources)
            element.dataset.risuManagedMedia = 'true'
            if (element instanceof HTMLImageElement && !element.hasAttribute('loading')) element.loading = 'lazy'
            detach(element)
        }
    }

    const targetVisibility = new Map<Element, boolean>()
    const playbackListeners: Array<{ media: HTMLMediaElement, release: () => void }> = []
    const releaseOffscreenTarget = (target: Element) => {
        if (disposed || targetVisibility.get(target) !== false) return
        for (const element of markerTargets.get(target) ?? []) setVisible(element, false)
        if (target instanceof HTMLMediaElement && !target.paused && !target.ended) return
        for (const source of originalSources.get(target) ?? []) detach(source.element)
    }
    if (useViewport) {
        const targets = new Set([...markerTargets.keys(), ...originalSources.keys()])
        for (const target of targets) {
            if (!(target instanceof HTMLMediaElement)) continue
            const release = () => releaseOffscreenTarget(target)
            target.addEventListener('pause', release)
            target.addEventListener('ended', release)
            playbackListeners.push({ media: target, release })
        }
    }

    let observer: IntersectionObserver | null = null
    if (useViewport) {
        observer = new IntersectionObserver((entries) => {
            if (disposed) return
            for (const entry of entries) {
                targetVisibility.set(entry.target, entry.isIntersecting)
                for (const element of markerTargets.get(entry.target) ?? []) {
                    setVisible(element, entry.isIntersecting)
                }
                for (const source of originalSources.get(entry.target) ?? []) {
                    if (entry.isIntersecting) attach(source.element, source.url)
                    else if (!(entry.target instanceof HTMLMediaElement && !entry.target.paused && !entry.target.ended)) {
                        detach(source.element)
                    }
                }
            }
        }, { root: null, rootMargin: '256px 0px', threshold: 0 })
        for (const target of markerTargets.keys()) observer.observe(target)
        for (const target of originalSources.keys()) observer.observe(target)
    }
    else {
        for (const element of markers.keys()) setVisible(element, true)
    }

    const settled = Promise.all([...pending.values()]).then(() => undefined)
    const cleanup = () => {
        if (disposed) return
        disposed = true
        observer?.disconnect()
        observer = null
        for (const { media, release } of playbackListeners) {
            media.removeEventListener('pause', release)
            media.removeEventListener('ended', release)
        }
        playbackListeners.length = 0
        for (const element of markers.keys()) detach(element, true)
        for (const sources of originalSources.values()) {
            for (const source of sources) detach(source.element, true)
        }
        for (const id of resources.keys()) releaseResource(id)
        visibleById.clear()
        targetVisibility.clear()
        registry?.clear()
        markerTargets.clear()
        originalSources.clear()
        markers.clear()
    }
    return { cleanup, settled }
}

export function mountDeferredInlaySources(
    root: ParentNode,
    registry?: DeferredInlayMarkerRegistry,
): () => void {
    return startDeferredInlaySources(root, registry, { viewportAware: true }).cleanup
}

export async function resolveDeferredInlaySources(
    root: ParentNode,
    registry?: DeferredInlayMarkerRegistry,
    options: { rejectOnError?: boolean } = {},
): Promise<() => void> {
    const mounted = startDeferredInlaySources(root, registry, options)
    try {
        await mounted.settled
        return mounted.cleanup
    }
    catch (error) {
        mounted.cleanup()
        throw error
    }
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
    knownMetadata?: InlayBlobMetadata,
): Promise<InlayRenderSource | null> {
    if (native) {
        const metadata = knownMetadata
            ?? await getInlayAssetMetadata(id, { migrateLegacy: false })
        if (!metadata) return null
        const url = await getInlayAssetRenderUrl(id)
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
