import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
    listInlayAssetMetadata: vi.fn(),
}))

vi.mock('../inlays', () => inlayMocks)

import {
    DeferredInlayMarkerRegistry,
    getInlayRenderSource,
    getInlayRenderSources,
    mountDeferredInlaySources,
    resolveDeferredInlaySources,
    renderDeferredInlaySourceMarkup,
    renderInlaySourceMarkup,
} from '../inlayRenderSource'

class TestIntersectionObserver {
    static instances: TestIntersectionObserver[] = []
    readonly observed = new Set<Element>()
    readonly disconnect = vi.fn(() => this.observed.clear())

    constructor(
        private readonly callback: IntersectionObserverCallback,
        _options?: IntersectionObserverInit,
    ) {
        TestIntersectionObserver.instances.push(this)
    }

    observe = (element: Element) => this.observed.add(element)
    unobserve = (element: Element) => this.observed.delete(element)
    takeRecords = () => []
    readonly root = null
    readonly rootMargin = '0px'
    readonly thresholds = [0]

    setVisible(element: Element, isIntersecting: boolean): void {
        this.callback([{
            target: element,
            isIntersecting,
            intersectionRatio: isIntersecting ? 1 : 0,
        } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
}

describe('getInlayRenderSource', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        TestIntersectionObserver.instances = []
        vi.stubGlobal('IntersectionObserver', undefined)
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn(() => 'blob:web-preview'),
            revokeObjectURL: vi.fn(),
        })
    })

    afterEach(() => {
        vi.unstubAllGlobals()
    })

    test('attaches a native original only while its marker is visible', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup('native-image', {
            url: 'http://risuasset.localhost/native-image',
            mime: 'image/webp',
            type: 'image',
            name: 'native.webp',
            size: 42,
            objectUrl: false,
        }, registry)
        document.body.append(root)
        const image = root.querySelector('img')!

        const cleanup = mountDeferredInlaySources(root, registry)
        const observer = TestIntersectionObserver.instances[0]
        expect(image.getAttribute('src')).toBeNull()

        observer.setVisible(image, true)
        await Promise.resolve()
        expect(image.getAttribute('src')).toBe('http://risuasset.localhost/native-image')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()

        observer.setVisible(image, false)
        expect(image.getAttribute('src')).toBeNull()
        expect(URL.revokeObjectURL).not.toHaveBeenCalled()

        observer.setVisible(image, true)
        await Promise.resolve()
        expect(image.getAttribute('src')).toBe('http://risuasset.localhost/native-image')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()

        cleanup()
        expect(observer.disconnect).toHaveBeenCalledOnce()
        expect(image.getAttribute('src')).toBeNull()
        root.remove()
    })

    test('keeps a shared browser object URL until its last visible marker exits', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['shared'], { type: 'image/png' }),
            type: 'image',
            name: 'shared.png',
        })
        const create = vi.fn()
            .mockReturnValueOnce('blob:shared-first')
            .mockReturnValueOnce('blob:shared-second')
        const revoke = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL: create, revokeObjectURL: revoke })
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = [0, 1].map(() => renderDeferredInlaySourceMarkup('shared', {
            url: '', mime: 'image/png', type: 'image', name: 'shared.png', size: 6, objectUrl: false,
        }, registry)).join('')
        document.body.append(root)
        const images = [...root.querySelectorAll('img')]

        const cleanup = mountDeferredInlaySources(root, registry)
        const observer = TestIntersectionObserver.instances[0]
        observer.setVisible(images[0], true)
        observer.setVisible(images[1], true)
        await Promise.resolve(); await Promise.resolve()
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1)
        expect(images.map((image) => image.getAttribute('src'))).toEqual(['blob:shared-first', 'blob:shared-first'])

        observer.setVisible(images[0], false)
        expect(revoke).not.toHaveBeenCalled()
        expect(images[1].getAttribute('src')).toBe('blob:shared-first')

        observer.setVisible(images[1], false)
        expect(revoke).toHaveBeenCalledOnce()
        expect(revoke).toHaveBeenCalledWith('blob:shared-first')

        observer.setVisible(images[0], true)
        await Promise.resolve(); await Promise.resolve()
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(2)
        expect(images[0].getAttribute('src')).toBe('blob:shared-second')

        cleanup()
        expect(revoke).toHaveBeenCalledTimes(2)
        expect(revoke).toHaveBeenLastCalledWith('blob:shared-second')
        root.remove()
    })

    test.each(['audio', 'video'] as const)('keeps playing deferred %s pinned until playback stops offscreen', async (type) => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup(`native-${type}`, {
            url: `http://risuasset.localhost/native-${type}`,
            mime: `${type}/webm`,
            type,
            name: `native.${type}`,
            size: 42,
            objectUrl: false,
        }, registry)
        document.body.append(root)
        const media = root.querySelector(type) as HTMLMediaElement
        let paused = false
        let ended = false
        Object.defineProperties(media, {
            paused: { configurable: true, get: () => paused },
            ended: { configurable: true, get: () => ended },
        })
        media.pause = vi.fn(() => { paused = true })
        media.load = vi.fn()
        const source = media.querySelector('source')!

        mountDeferredInlaySources(root, registry)
        const observer = TestIntersectionObserver.instances[0]
        observer.setVisible(media, true)
        await Promise.resolve()
        expect(source.getAttribute('src')).toContain(`native-${type}`)

        media.dispatchEvent(new Event('play'))
        observer.setVisible(media, false)
        expect(media.pause).not.toHaveBeenCalled()
        expect(source.getAttribute('src')).toContain(`native-${type}`)

        paused = true
        media.dispatchEvent(new Event('pause'))
        expect(source.getAttribute('src')).toBeNull()
        expect(media.load).toHaveBeenCalledTimes(2)

        observer.setVisible(media, true)
        await Promise.resolve()
        paused = false
        ended = false
        media.dispatchEvent(new Event('play'))
        observer.setVisible(media, false)
        ended = true
        media.dispatchEvent(new Event('ended'))
        expect(source.getAttribute('src')).toBeNull()
        root.remove()
    })

    test.each(['audio', 'video'] as const)('unloads non-playing deferred %s when it leaves the viewport', async (type) => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup(`native-${type}`, {
            url: `http://risuasset.localhost/native-${type}`,
            mime: `${type}/webm`,
            type,
            name: `native.${type}`,
            size: 42,
            objectUrl: false,
        }, registry)
        document.body.append(root)
        const media = root.querySelector(type) as HTMLMediaElement
        media.pause = vi.fn()
        media.load = vi.fn()
        const source = media.querySelector('source')!

        mountDeferredInlaySources(root, registry)
        const observer = TestIntersectionObserver.instances[0]
        observer.setVisible(media, true)
        await Promise.resolve()
        observer.setVisible(media, false)

        expect(media.pause).not.toHaveBeenCalled()
        expect(source.getAttribute('src')).toBeNull()
        expect(media.load).toHaveBeenCalledTimes(2)
        root.remove()
    })

    test('ignores late visibility and blob completion after navigation cleanup', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        let resolve: (value: any) => void
        inlayMocks.getInlayAssetBlob.mockReturnValue(new Promise((done) => { resolve = done }))
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup('late-navigation', {
            url: '', mime: 'image/png', type: 'image', name: 'late.png', size: 1, objectUrl: false,
        }, registry)
        document.body.append(root)
        const image = root.querySelector('img')!

        const cleanup = mountDeferredInlaySources(root, registry)
        const observer = TestIntersectionObserver.instances[0]
        observer.setVisible(image, true)
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce()
        cleanup()
        root.remove()

        observer.setVisible(image, true)
        resolve!({ data: new Blob(['late']), type: 'image', name: 'late.png' })
        await Promise.resolve(); await Promise.resolve()
        expect(image.getAttribute('src')).toBeNull()
        expect(URL.createObjectURL).not.toHaveBeenCalled()
        expect(URL.revokeObjectURL).not.toHaveBeenCalled()
    })

    test('detaches an existing original asset URL offscreen without changing its markup layout', () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const root = document.createElement('div')
        root.innerHTML = '<figure class="custom-bot-layout"><img class="portrait" src="http://risuasset.localhost/assets/original.png"><figcaption>caption</figcaption></figure>'
        document.body.append(root)
        const image = root.querySelector('img')!

        const cleanup = mountDeferredInlaySources(root, new DeferredInlayMarkerRegistry())
        const observer = TestIntersectionObserver.instances[0]
        expect(image.getAttribute('src')).toBeNull()
        expect(root.querySelector('figure')?.className).toBe('custom-bot-layout')
        expect(root.querySelector('figcaption')?.textContent).toBe('caption')

        observer.setVisible(image, true)
        expect(image.getAttribute('src')).toBe('http://risuasset.localhost/assets/original.png')
        observer.setVisible(image, false)
        expect(image.getAttribute('src')).toBeNull()
        expect(URL.revokeObjectURL).not.toHaveBeenCalled()

        cleanup()
        root.remove()
    })

    test('keeps live browser object URLs bounded across repeated gallery-style visits', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['gallery'], { type: 'image/png' }),
            type: 'image',
            name: 'gallery.png',
        })
        let liveUrls = 0
        let peakLiveUrls = 0
        let nextUrl = 0
        const create = vi.fn(() => {
            liveUrls++
            peakLiveUrls = Math.max(peakLiveUrls, liveUrls)
            return `blob:gallery-${nextUrl++}`
        })
        const revoke = vi.fn(() => {
            liveUrls--
        })
        vi.stubGlobal('URL', { ...URL, createObjectURL: create, revokeObjectURL: revoke })
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup('gallery', {
            url: '', mime: 'image/png', type: 'image', name: 'gallery.png', size: 7, objectUrl: false,
        }, registry)
        document.body.append(root)
        const image = root.querySelector('img')!

        const cleanup = mountDeferredInlaySources(root, registry)
        const observer = TestIntersectionObserver.instances[0]
        for (let visit = 0; visit < 1_000; visit++) {
            observer.setVisible(image, true)
            await Promise.resolve(); await Promise.resolve()
            observer.setVisible(image, false)
        }

        expect(create).toHaveBeenCalledTimes(1_000)
        expect(revoke).toHaveBeenCalledTimes(1_000)
        expect(peakLiveUrls).toBe(1)
        expect(liveUrls).toBe(0)
        cleanup()
        root.remove()
    })

    test('uses the native render URL and stored MIME without reading payload bytes', async () => {
        inlayMocks.getInlayAssetMetadata.mockResolvedValue({
            key: 'video-id',
            kind: 'inlay',
            size: 42,
            mime: 'video/webm',
            name: 'clip.webm',
            ext: 'webm',
            inlayType: 'video',
        })
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/video-id')

        await expect(getInlayRenderSource('video-id', true)).resolves.toEqual({
            url: 'http://risuasset.localhost/video-id',
            mime: 'video/webm',
            type: 'video',
            name: 'clip.webm',
            size: 42,
            objectUrl: false,
        })
        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledWith('video-id')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })

    test('does not read the legacy store when native metadata is missing', async () => {
        inlayMocks.getInlayAssetMetadata.mockResolvedValue(null)

        await expect(getInlayRenderSource('legacy-audio', true)).resolves.toBeNull()
        expect(inlayMocks.getInlayAssetMetadata).toHaveBeenCalledWith('legacy-audio', { migrateLegacy: false })
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
        expect(inlayMocks.getInlayAssetRenderUrl).not.toHaveBeenCalled()
    })

    test('resolves only unique referenced native IDs without listing unrelated metadata or reading payloads', async () => {
        inlayMocks.listInlayAssetMetadata.mockResolvedValue(Array.from({ length: 10_000 }, (_, index) => ({
            key: `unrelated-${index}`,
            kind: 'inlay',
            size: 10,
            mime: 'image/png',
            name: `unrelated-${index}.png`,
            ext: 'png',
            inlayType: 'image',
        })))
        inlayMocks.getInlayAssetMetadata.mockImplementation(async (id: string) => ({
            key: id,
            kind: 'inlay',
            size: 10,
            mime: 'image/png',
            name: `${id}.png`,
            ext: 'png',
            inlayType: 'image',
        }))
        inlayMocks.getInlayAssetRenderUrl.mockImplementation(async (id: string) => `http://asset.local/${id}`)

        const sources = await getInlayRenderSources(['shown-a', 'shown-a', 'shown-b'], true)

        expect([...sources.keys()]).toEqual(['shown-a', 'shown-b'])
        expect(inlayMocks.getInlayAssetMetadata).toHaveBeenCalledTimes(2)
        expect(inlayMocks.getInlayAssetMetadata).toHaveBeenNthCalledWith(1, 'shown-a', { migrateLegacy: false })
        expect(inlayMocks.getInlayAssetMetadata).toHaveBeenNthCalledWith(2, 'shown-b', { migrateLegacy: false })
        expect(inlayMocks.listInlayAssetMetadata).not.toHaveBeenCalled()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })

    test('uses the stored MIME in media markup', () => {
        expect(renderInlaySourceMarkup({
            url: 'http://risuasset.localhost/video-id',
            mime: 'video/webm',
            type: 'video',
            name: 'clip.webm',
            size: 42,
            objectUrl: false,
        })).toBe('<video controls><source src="http://risuasset.localhost/video-id" type="video/webm"></video>')
    })

    test('escapes URL and MIME attribute values', () => {
        expect(renderInlaySourceMarkup({
            url: 'http://example.test/a?x="<>&\'value',
            mime: 'video/webm" onload="bad<>&\'',
            type: 'video',
            name: 'clip.webm',
            size: 42,
            objectUrl: false,
        })).toBe('<video controls><source src="http://example.test/a?x=&quot;&lt;&gt;&amp;&#39;value" type="video/webm&quot; onload=&quot;bad&lt;&gt;&amp;&#39;"></video>')
    })

    test('escapes deferred marker IDs', () => {
        const registry = new DeferredInlayMarkerRegistry()
        expect(renderDeferredInlaySourceMarkup('a"<', {
            url: '', mime: 'image/png', type: 'image', name: 'a', size: 1, objectUrl: false,
        }, registry)).toContain('data-risu-inlay-id="a&quot;&lt;"')
    })

    test('ignores forged raw markers and mismatched element kinds', async () => {
        const registry = new DeferredInlayMarkerRegistry()
        const generated = renderDeferredInlaySourceMarkup('image-id', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)
        const slot = generated.match(/data-risu-inlay-slot="([^"]+)/)?.[1]
        const root = document.createElement('div')
        root.innerHTML = `<img data-risu-inlay-id="forged" data-risu-inlay-token="guessed"><video><source data-risu-inlay-slot="${slot}"></video>`
        const cleanup = mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
        cleanup()
    })

    test('does not retain dropped generated markers', () => {
        const registry = new DeferredInlayMarkerRegistry()
        for (let index = 0; index < 1000; index++) {
            expect(renderDeferredInlaySourceMarkup(`drop-${index}`, { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)).toContain('data-risu-inlay-slot')
        }
        registry.clear()
    })

    test('loads each mounted ID once and releases URLs once', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({ data: new Blob(['x'.repeat(33 * 1024 * 1024)]), type: 'image', name: 'x' })
        const create = vi.fn((_: Blob) => `blob:${create.mock.calls.length}`)
        const revoke = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL: create, revokeObjectURL: revoke })
        const root = document.createElement('div')
        const registry = new DeferredInlayMarkerRegistry()
        root.innerHTML = Array.from({ length: 129 }, () => renderDeferredInlaySourceMarkup('same', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)).join('')
        document.body.append(root)
        const cleanup = mountDeferredInlaySources(root, registry)
        await Promise.resolve()
        await Promise.resolve()
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1)
        expect(root.querySelectorAll('[src="blob:1"]')).toHaveLength(129)
        cleanup(); cleanup()
        expect(revoke).toHaveBeenCalledTimes(1)
        root.remove()
    })

    test.each(['audio', 'video'] as const)('reloads deferred %s after assigning its source', async (type) => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({ data: new Blob(['x']), type, name: `x.${type}` })
        const root = document.createElement('div')
        const registry = new DeferredInlayMarkerRegistry()
        root.innerHTML = renderDeferredInlaySourceMarkup('media', { url: '', mime: `${type}/x`, type, name: 'x', size: 1, objectUrl: false }, registry)
        const media = root.querySelector(type) as HTMLMediaElement
        media.load = vi.fn()
        document.body.append(root)

        mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()

        expect(media.load).toHaveBeenCalledTimes(1)
        root.remove()
    })

    test('releases pending elements without creating a URL after cleanup', async () => {
        let resolve: (value: any) => void
        inlayMocks.getInlayAssetBlob.mockReturnValue(new Promise((done) => { resolve = done }))
        const revoke = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL: vi.fn(() => 'blob:late'), revokeObjectURL: revoke })
        const root = document.createElement('div')
        const registry = new DeferredInlayMarkerRegistry()
        root.innerHTML = renderDeferredInlaySourceMarkup('late', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)
        const element = root.querySelector('img')!
        const setAttribute = vi.spyOn(element, 'setAttribute')
        mountDeferredInlaySources(root, registry)()
        setAttribute.mockClear()
        root.remove()
        resolve!({ data: new Blob(['x']), type: 'image', name: 'x' })
        await Promise.resolve(); await Promise.resolve()
        expect(root.querySelector('img')?.getAttribute('src')).toBeNull()
        expect(URL.createObjectURL).not.toHaveBeenCalled()
        expect(revoke).not.toHaveBeenCalled()
        expect(setAttribute).not.toHaveBeenCalled()
    })

    test('awaits deferred source attachment for detached document serialization', async () => {
        let resolve: (value: any) => void
        inlayMocks.getInlayAssetBlob.mockReturnValue(new Promise((done) => { resolve = done }))
        const registry = new DeferredInlayMarkerRegistry()
        const doc = document.implementation.createHTMLDocument()
        doc.body.innerHTML = renderDeferredInlaySourceMarkup('copy', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)
        const resolving = resolveDeferredInlaySources(doc, registry)
        let settled = false
        void resolving.then(() => { settled = true })
        await Promise.resolve()
        expect(settled).toBe(false)

        resolve!({ data: new Blob(['x']), type: 'image', name: 'x' })
        const cleanup = await resolving

        expect(doc.querySelector('img')?.getAttribute('src')).toBe('blob:web-preview')
        cleanup()
        expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:web-preview')
    })

    test('rejects capture readiness when object URL resolution fails', async () => {
        const error = new Error('blob read failed')
        inlayMocks.getInlayAssetBlob.mockRejectedValueOnce(error)
        const registry = new DeferredInlayMarkerRegistry()
        const clear = vi.spyOn(registry, 'clear')
        const doc = document.implementation.createHTMLDocument()
        doc.body.innerHTML = renderDeferredInlaySourceMarkup('broken', {
            url: '', mime: 'image/png', type: 'image', name: 'broken.png', size: 1, objectUrl: false,
        }, registry)

        await expect(
            resolveDeferredInlaySources(doc, registry, { rejectOnError: true }),
        ).rejects.toBe(error)
        expect(clear).toHaveBeenCalled()
    })

    test('does not let a copied slot authorize another ID', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({ data: new Blob(['x']), type: 'image', name: 'x' })
        const registry = new DeferredInlayMarkerRegistry()
        const generated = renderDeferredInlaySourceMarkup('original', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)
        const slot = generated.match(/data-risu-inlay-slot="([^"]+)/)?.[1]
        const root = document.createElement('div')
        root.innerHTML = `${generated}<img data-risu-inlay-id="other" data-risu-inlay-slot="${slot}">`
        document.body.append(root)

        mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()

        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1)
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledWith('original')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalledWith('other')
        expect(root.querySelector('[data-risu-inlay-id="other"]')?.getAttribute('src')).toBeNull()
        root.remove()
    })

    test('does not let a sealed token authorize a newly forged ID', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({ data: new Blob(['x']), type: 'image', name: 'x' })
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup('original', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)
        document.body.append(root)
        const cleanup = mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()
        const observedToken = root.querySelector('img')?.dataset.risuInlayToken
        expect(observedToken).toBeTruthy()
        inlayMocks.getInlayAssetBlob.mockClear()

        root.insertAdjacentHTML('beforeend', `<img data-risu-inlay-id="other" data-risu-inlay-token="${observedToken}">`)
        mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()

        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
        expect(root.querySelector('[data-risu-inlay-id="other"]')?.getAttribute('src')).toBeNull()
        cleanup()
        root.remove()
    })

    test('revokes a URL when its only marker disconnects during URL creation', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({ data: new Blob(['x']), type: 'image', name: 'x' })
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = renderDeferredInlaySourceMarkup('race', { url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false }, registry)
        document.body.append(root)
        const revoke = vi.fn()
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn(() => {
                root.querySelector('img')?.remove()
                return 'blob:detached'
            }),
            revokeObjectURL: revoke,
        })

        mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()

        expect(revoke).toHaveBeenCalledTimes(1)
        expect(revoke).toHaveBeenCalledWith('blob:detached')
        root.remove()
    })

    test('preserves the web Blob fallback when legacy metadata is not listed yet', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['voice'], { type: 'audio/ogg' }),
            ext: 'ogg',
            name: 'voice.ogg',
            type: 'audio',
        })

        await expect(getInlayRenderSource('legacy-audio', false)).resolves.toEqual({
            url: 'blob:web-preview',
            mime: 'audio/ogg',
            type: 'audio',
            name: 'voice.ogg',
            size: 5,
            objectUrl: true,
        })
        expect(inlayMocks.getInlayAssetRenderUrl).not.toHaveBeenCalled()
    })
})
