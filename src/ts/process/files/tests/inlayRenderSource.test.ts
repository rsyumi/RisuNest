import { beforeEach, describe, expect, test, vi } from 'vitest'

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

describe('getInlayRenderSource', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn(() => 'blob:web-preview'),
            revokeObjectURL: vi.fn(),
        })
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
