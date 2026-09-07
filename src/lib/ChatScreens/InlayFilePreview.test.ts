// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { language } from 'src/lang'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
    listInlayAssetMetadata: vi.fn(),
}))

vi.mock('src/ts/process/files/inlays', () => inlayMocks)
const platform = vi.hoisted(() => ({ isTauri: true }))
vi.mock('src/ts/platform', () => ({ get isTauri() { return platform.isTauri } }))

import InlayFilePreview from './InlayFilePreview.svelte'
import InlayFilePreviewHarness from './InlayFilePreviewHarness.test.svelte'

class TestIntersectionObserver {
    static instance: TestIntersectionObserver | undefined
    readonly disconnect = vi.fn()
    constructor(private readonly callback: IntersectionObserverCallback) {
        TestIntersectionObserver.instance = this
    }
    observe = vi.fn()
    unobserve = vi.fn()
    takeRecords = () => []
    readonly root = null
    readonly rootMargin = '0px'
    readonly thresholds = [0]
    setVisible(element: Element, visible: boolean) {
        this.callback([{
            target: element,
            isIntersecting: visible,
            intersectionRatio: visible ? 1 : 0,
        } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
}

describe('InlayFilePreview', () => {
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        TestIntersectionObserver.instance = undefined
        vi.stubGlobal('IntersectionObserver', undefined)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
        vi.unstubAllGlobals()
        platform.isTauri = true
    })

    test('renders a native media URL with the stored MIME without loading base64 or Blob data', async () => {
        inlayMocks.getInlayAssetMetadata.mockResolvedValue({
            key: 'audio-id',
            kind: 'inlay',
            size: 12,
            mime: 'audio/ogg',
            name: 'voice.ogg',
            ext: 'ogg',
            inlayType: 'audio',
        })
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/audio-id')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(InlayFilePreview, { target, props: { id: 'audio-id' } })

        await vi.waitFor(() => expect(target.querySelector('source')).not.toBeNull())

        const source = target.querySelector('source')
        expect(source?.getAttribute('src')).toBe('http://risuasset.localhost/audio-id')
        expect(source?.getAttribute('type')).toBe('audio/ogg')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })

    test('revokes the previous web object URL when the attachment id changes and on unmount', async () => {
        platform.isTauri = false
        inlayMocks.getInlayAssetBlob.mockImplementation(async (id: string) => ({
            data: new Blob([id], { type: 'image/png' }),
            ext: 'png',
            name: `${id}.png`,
            type: 'image',
        }))
        const createObjectURL = vi.fn()
            .mockReturnValueOnce('blob:first')
            .mockReturnValueOnce('blob:second')
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(InlayFilePreviewHarness, { target })

        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:first'))
        ;(mounted as { setId(id: string): void }).setId('second-id')
        await tick()
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:second'))
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:first')

        await unmount(mounted)
        mounted = undefined
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:second')
    })

    test('owns a browser preview URL only while the attachment is near the viewport', async () => {
        platform.isTauri = false
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['image'], { type: 'image/png' }),
            ext: 'png',
            name: 'image.png',
            type: 'image',
            width: 800,
            height: 600,
        })
        const createObjectURL = vi.fn()
            .mockReturnValueOnce('blob:visible-first')
            .mockReturnValueOnce('blob:visible-second')
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL, revokeObjectURL })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(InlayFilePreview, { target, props: { id: 'image-id' } })
        await tick()
        const root = target.querySelector('[data-inlay-file-preview]')!
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()

        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:visible-first'))
        TestIntersectionObserver.instance?.setVisible(root, false)
        await tick()
        const placeholder = target.querySelector<HTMLElement>('[data-inlay-file-preview-box]')
        expect(placeholder).not.toBeNull()
        expect(placeholder?.style.aspectRatio).toBe('800 / 600')
        expect(target.querySelector('img')).not.toBeNull()
        expect(target.querySelector('img')?.getAttribute('src')).toBeNull()
        expect(revokeObjectURL).toHaveBeenCalledOnce()
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:visible-first')

        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:visible-second'))
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(2)
    })

    test('shows an unavailable attachment on failed reentry and recovers on the next load', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const metadata = {
            key: 'image-id',
            kind: 'inlay',
            size: 12,
            mime: 'image/png',
            name: 'image.png',
            ext: 'png',
            inlayType: 'image',
        }
        inlayMocks.getInlayAssetMetadata
            .mockResolvedValueOnce(metadata)
            .mockResolvedValueOnce(null)
            .mockResolvedValueOnce(metadata)
        inlayMocks.getInlayAssetRenderUrl
            .mockResolvedValueOnce('http://risuasset.localhost/image-id')
            .mockResolvedValueOnce('http://risuasset.localhost/image-id?retry=1')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(InlayFilePreview, { target, props: { id: 'image-id' } })
        await tick()
        const root = target.querySelector('[data-inlay-file-preview]')!

        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() =>
            expect(target.querySelector('img')?.getAttribute('src')).toBe(
                'http://risuasset.localhost/image-id',
            ),
        )
        TestIntersectionObserver.instance?.setVisible(root, false)
        await tick()
        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() => expect(target.textContent).toContain(language.inlayUnavailable))

        TestIntersectionObserver.instance?.setVisible(root, false)
        await tick()
        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() =>
            expect(target.querySelector('img')?.getAttribute('src')).toBe(
                'http://risuasset.localhost/image-id?retry=1',
            ),
        )
        expect(target.textContent).not.toContain(language.inlayUnavailable)
    })

    test.each([
        ['audio', 'pause'],
        ['video', 'ended'],
    ] as const)('pins a playing %s preview until %s offscreen', async (type, stopEvent) => {
        platform.isTauri = false
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob([type], { type: `${type}/webm` }),
            ext: 'webm',
            name: `clip.${type}`,
            type,
            width: 640,
            height: 360,
        })
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', { ...URL, createObjectURL: vi.fn(() => `blob:${type}`), revokeObjectURL })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(InlayFilePreview, { target, props: { id: `${type}-id` } })
        await tick()
        const root = target.querySelector('[data-inlay-file-preview]')!

        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() => expect(target.querySelector('source')?.getAttribute('src')).toBe(`blob:${type}`))
        const media = target.querySelector(type) as HTMLMediaElement
        let paused = false
        let ended = false
        Object.defineProperties(media, {
            paused: { configurable: true, get: () => paused },
            ended: { configurable: true, get: () => ended },
        })
        media.pause = vi.fn(() => { paused = true })
        media.load = vi.fn()
        media.dispatchEvent(new Event('play'))

        TestIntersectionObserver.instance?.setVisible(root, false)
        await tick()
        expect(media.pause).not.toHaveBeenCalled()
        expect(target.querySelector('source')?.getAttribute('src')).toBe(`blob:${type}`)
        expect(revokeObjectURL).not.toHaveBeenCalled()

        if (stopEvent === 'pause') paused = true
        else ended = true
        media.dispatchEvent(new Event(stopEvent))
        await vi.waitFor(() => expect(target.querySelector('source')?.getAttribute('src')).toBeNull())
        expect(revokeObjectURL).toHaveBeenCalledOnce()

        const loadedSources: Array<string | null> = []
        media.load = vi.fn(() => {
            loadedSources.push(media.querySelector('source')?.getAttribute('src') ?? null)
        })
        TestIntersectionObserver.instance?.setVisible(root, true)
        await vi.waitFor(() => expect(loadedSources).toContain(`blob:${type}`))
        expect(target.querySelector(type)).toBe(media)
    })
})
