// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
    listInlayAssets: vi.fn(),
    listInlayAssetMetadata: vi.fn(),
    removeInlayAsset: vi.fn(),
}))
const alertMocks = vi.hoisted(() => ({ alertConfirm: vi.fn() }))
const platformMocks = vi.hoisted(() => ({ isTauri: true }))

vi.mock('src/ts/process/files/inlays', () => inlayMocks)
vi.mock('src/ts/platform', () => ({
    get isTauri() {
        return platformMocks.isTauri
    },
}))
vi.mock('src/ts/alert', () => alertMocks)
vi.mock('src/lang', () => ({
    language: {
        playground: {
            inlayDeleteConfirm: 'Delete {name}',
            inlayDeleteMultipleConfirm: 'Delete {count}',
            inlayDeselectAll: 'Deselect all',
            inlayEmpty: 'Empty',
            inlayEmptyDesc: 'No inlays',
            inlayExplorer: 'Inlays',
            inlayTotalAssets: '{count} assets',
            inlayDeleteSelected: 'Delete selected',
            inlaySelectAll: 'Select all',
        },
    },
}))

import PlaygroundInlayExplorer from './PlaygroundInlayExplorer.svelte'

class TestIntersectionObserver {
    static instances: TestIntersectionObserver[] = []
    readonly observed = new Set<Element>()
    readonly disconnect = vi.fn(() => this.observed.clear())

    constructor(private readonly callback: IntersectionObserverCallback) {
        TestIntersectionObserver.instances.push(this)
    }

    observe = (element: Element) => this.observed.add(element)
    unobserve = (element: Element) => this.observed.delete(element)
    takeRecords = () => []
    readonly root = null
    readonly rootMargin = '0px'
    readonly thresholds = [0]

    setVisible(element: Element, visible: boolean): void {
        this.callback([{
            target: element,
            isIntersecting: visible,
            intersectionRatio: visible ? 1 : 0,
        } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
}

describe('PlaygroundInlayExplorer native previews', () => {
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        TestIntersectionObserver.instances = []
        vi.stubGlobal('IntersectionObserver', undefined)
        platformMocks.isTauri = true
        inlayMocks.listInlayAssets.mockResolvedValue([])
        inlayMocks.listInlayAssetMetadata.mockResolvedValue([{
            key: 'photo-id',
            kind: 'inlay',
            size: 4096,
            mime: 'image/webp',
            name: 'photo.webp',
            ext: 'webp',
            inlayType: 'image',
            width: 800,
            height: 600,
        }])
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/photo-id')
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.restoreAllMocks()
        vi.unstubAllGlobals()
        vi.clearAllMocks()
    })

    test.each(['audio', 'video'] as const)(
        'loads an asynchronous %s source and unloads it offscreen',
        async (type) => {
            vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
            inlayMocks.listInlayAssetMetadata.mockResolvedValue([
                {
                    key: 'clip-id',
                    kind: 'inlay',
                    size: 12,
                    mime: `${type}/webm`,
                    name: 'clip.webm',
                    ext: 'webm',
                    inlayType: type,
                },
            ])
            inlayMocks.getInlayAssetRenderUrl.mockResolvedValue(
                'http://risuasset.localhost/clip-id',
            )
            const target = document.createElement('div')
            document.body.appendChild(target)
            mounted = mount(PlaygroundInlayExplorer, { target })
            await vi.waitFor(() => expect(target.querySelector(type)).not.toBeNull())
            const media = target.querySelector(type) as HTMLMediaElement
            const loadedSources: Array<string | null> = []
            media.load = vi.fn(() => {
                loadedSources.push(media.querySelector('source')?.getAttribute('src') ?? null)
            })
            const card = target.querySelector('[data-inlay-preview-id]')!
            const observer = TestIntersectionObserver.instances.find((entry) =>
                entry.observed.has(card),
            )!

            observer.setVisible(card, true)
            await vi.waitFor(() =>
                expect(loadedSources).toContain('http://risuasset.localhost/clip-id'),
            )
            expect(media.querySelector('source')?.getAttribute('type')).toBe(`${type}/webm`)
            observer.setVisible(card, false)
            await vi.waitFor(() => expect(loadedSources.at(-1)).toBeNull())
            observer.setVisible(card, true)
            await vi.waitFor(() =>
                expect(loadedSources.at(-1)).toBe('http://risuasset.localhost/clip-id'),
            )
        },
    )

    test('lists metadata and requests the original native URL without loading the payload', async () => {
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())
        await tick()

        expect(inlayMocks.listInlayAssetMetadata).toHaveBeenCalledOnce()
        expect(inlayMocks.listInlayAssetMetadata).toHaveBeenCalledWith({ migrateLegacy: false })
        expect(inlayMocks.listInlayAssets).not.toHaveBeenCalled()
        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledWith('photo-id')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
        expect(target.querySelector('img')?.getAttribute('src')).toBe('http://risuasset.localhost/photo-id')
        expect(target.textContent).toContain('4.0 KB')
    })

    test('uses the original native URL for AVIF images', async () => {
        inlayMocks.listInlayAssetMetadata.mockResolvedValue([{
            key: 'photo-avif',
            kind: 'inlay',
            size: 2048,
            mime: 'image/avif',
            name: 'photo.avif',
            ext: 'avif',
            inlayType: 'image',
        }])
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/photo-avif')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())

        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledWith('photo-avif')
        expect(target.querySelector('img')?.getAttribute('src')).toBe('http://risuasset.localhost/photo-avif')
    })

    test('never revokes a native URL that resolves after destruction', async () => {
        let resolveUrl!: (url: string) => void
        inlayMocks.getInlayAssetRenderUrl.mockReturnValue(new Promise<string>((resolve) => {
            resolveUrl = resolve
        }))
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledOnce())
        await unmount(mounted)
        mounted = undefined
        resolveUrl('http://risuasset.localhost/photo-id')
        await Promise.resolve()
        await Promise.resolve()

        expect(revokeObjectURL).not.toHaveBeenCalled()
    })

    test('detaches and reattaches an original native preview as its gallery card leaves the viewport', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })
        await vi.waitFor(() => expect(target.querySelector('[data-inlay-preview-id="photo-id"]')).not.toBeNull())
        const card = target.querySelector('[data-inlay-preview-id="photo-id"]')!
        const observer = TestIntersectionObserver.instances[0]
        expect(target.querySelector('img')).not.toBeNull()
        expect(target.querySelector('img')?.getAttribute('src')).toBeNull()

        observer.setVisible(card, true)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('http://risuasset.localhost/photo-id'))
        observer.setVisible(card, false)
        await tick()
        expect(target.querySelector('img')).not.toBeNull()
        expect(target.querySelector('img')?.getAttribute('src')).toBeNull()
        expect(target.querySelector('img')?.classList.contains('h-40')).toBe(true)
        expect(revokeObjectURL).not.toHaveBeenCalled()

        observer.setVisible(card, true)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('http://risuasset.localhost/photo-id'))
        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledTimes(2)
    })

    test('keeps 108 native thumbnail previews detached outside each synthetic scroll page', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const assets = Array.from({ length: 108 }, (_, index) => ({
            key: `photo-${index}`,
            kind: 'inlay' as const,
            size: 4096,
            mime: 'image/webp',
            name: `photo-${index}.webp`,
            ext: 'webp',
            inlayType: 'image' as const,
            width: 800,
            height: 600,
        }))
        inlayMocks.listInlayAssetMetadata.mockResolvedValue(assets)
        inlayMocks.getInlayAssetRenderUrl.mockImplementation(async (id) => `http://risuasset.localhost/${id}`)
        const createObjectURL = vi.spyOn(URL, 'createObjectURL')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(target.querySelectorAll('[data-inlay-preview-id]')).toHaveLength(36))
        const previewObserver = TestIntersectionObserver.instances.find((observer) =>
            [...observer.observed].some((element) => element.matches('[data-inlay-preview-id]')),
        )!

        for (const pageStart of [0, 36, 72]) {
            const cards = [...target.querySelectorAll<HTMLElement>('[data-inlay-preview-id]')].slice(pageStart, pageStart + 36)
            for (const card of cards) previewObserver.setVisible(card, true)
            await vi.waitFor(() => expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledTimes(pageStart + 36))

            if (pageStart === 72) continue
            for (const card of cards) previewObserver.setVisible(card, false)
            await tick()
            expect(cards.every((card) => card.querySelector('img')?.getAttribute('src') === null)).toBe(true)

            const sentinel = target.querySelector('.h-12')!
            const loadMoreObserver = TestIntersectionObserver.instances.find((observer) => observer.observed.has(sentinel))!
            loadMoreObserver.setVisible(sentinel, true)
            await vi.waitFor(() => expect(target.querySelectorAll('[data-inlay-preview-id]')).toHaveLength(pageStart + 72))
        }

        expect(createObjectURL).not.toHaveBeenCalled()
    })
})

describe('PlaygroundInlayExplorer browser preview ownership', () => {
    let mounted: ReturnType<typeof mount> | undefined

    const metadata = {
        key: 'photo-id',
        kind: 'inlay' as const,
        size: 4096,
        mime: 'image/webp',
        name: 'photo.webp',
        ext: 'webp',
        inlayType: 'image' as const,
        width: 800,
        height: 600,
    }
    const asset = {
        data: new Blob(['photo'], { type: 'image/webp' }),
        type: 'image' as const,
        name: 'photo.webp',
        width: 800,
        height: 600,
    }

    beforeEach(() => {
        TestIntersectionObserver.instances = []
        vi.stubGlobal('IntersectionObserver', undefined)
        platformMocks.isTauri = false
        inlayMocks.listInlayAssetMetadata.mockResolvedValue([metadata])
        inlayMocks.removeInlayAsset.mockResolvedValue(undefined)
        alertMocks.alertConfirm.mockResolvedValue(true)
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:photo-preview')
        vi.spyOn(URL, 'revokeObjectURL')
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        platformMocks.isTauri = true
        vi.restoreAllMocks()
        vi.unstubAllGlobals()
        vi.clearAllMocks()
    })

    test('revokes a browser URL that resolves after destruction exactly once', async () => {
        let resolveAsset!: (value: typeof asset) => void
        inlayMocks.getInlayAssetBlob.mockReturnValue(new Promise((resolve) => {
            resolveAsset = resolve
        }))
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce())
        await unmount(mounted)
        mounted = undefined
        resolveAsset(asset)

        await vi.waitFor(() => expect(URL.revokeObjectURL).toHaveBeenCalledOnce())
        expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:photo-preview')
    })

    test('deduplicates concurrent requests for the same preview', async () => {
        let resolveAsset!: (value: typeof asset) => void
        inlayMocks.getInlayAssetBlob.mockReturnValue(new Promise((resolve) => {
            resolveAsset = resolve
        }))
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce())
        target.querySelector<HTMLInputElement>('input[type="checkbox"]')?.click()
        await tick()
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce()

        resolveAsset(asset)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:photo-preview'))
        expect(URL.createObjectURL).toHaveBeenCalledOnce()
        expect(URL.revokeObjectURL).not.toHaveBeenCalled()

        await unmount(mounted)
        mounted = undefined
        expect(URL.revokeObjectURL).toHaveBeenCalledOnce()
    })

    test('revokes a preview that resolves after its asset is removed', async () => {
        let resolveAsset!: (value: typeof asset) => void
        inlayMocks.getInlayAssetBlob.mockReturnValue(new Promise((resolve) => {
            resolveAsset = resolve
        }))
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce())
        const deleteButton = [...target.querySelectorAll('button')]
            .find((button) => button.textContent?.trim() === 'Delete')
        deleteButton?.click()
        await vi.waitFor(() => expect(inlayMocks.removeInlayAsset).toHaveBeenCalledWith('photo-id'))
        resolveAsset(asset)

        await vi.waitFor(() => expect(URL.revokeObjectURL).toHaveBeenCalledOnce())
        expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:photo-preview')
        expect(target.querySelector('img')).toBeNull()
    })

    test('resolves concurrent callers to null after a shared failure, then retries on re-entry', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        let rejectAsset!: (reason: Error) => void
        inlayMocks.getInlayAssetBlob
            .mockReturnValueOnce(new Promise((_, reject) => {
                rejectAsset = reject
            }))
            .mockResolvedValueOnce(asset)
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(target.querySelector('[data-inlay-preview-id="photo-id"]')).not.toBeNull())
        const card = target.querySelector('[data-inlay-preview-id="photo-id"]')!
        const observer = TestIntersectionObserver.instances[0]
        observer.setVisible(card, true)
        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce())
        observer.setVisible(card, true)
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce()

        rejectAsset(new Error('preview failed'))
        await new Promise((resolve) => setTimeout(resolve, 0))
        await tick()
        expect(target.querySelector('img')).not.toBeNull()
        expect(target.querySelector('img')?.getAttribute('src')).toBeNull()

        observer.setVisible(card, false)
        observer.setVisible(card, true)

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(2))
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:photo-preview'))
    })

    test('revokes an offscreen browser preview and creates a fresh URL on re-entry', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        inlayMocks.getInlayAssetBlob.mockResolvedValue(asset)
        vi.mocked(URL.createObjectURL)
            .mockReturnValueOnce('blob:photo-first')
            .mockReturnValueOnce('blob:photo-second')
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })
        await vi.waitFor(() => expect(target.querySelector('[data-inlay-preview-id="photo-id"]')).not.toBeNull())
        const card = target.querySelector('[data-inlay-preview-id="photo-id"]')!
        const observer = TestIntersectionObserver.instances[0]

        observer.setVisible(card, true)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:photo-first'))
        observer.setVisible(card, false)
        await tick()
        expect(target.querySelector('img')).not.toBeNull()
        expect(target.querySelector('img')?.getAttribute('src')).toBeNull()
        expect(URL.revokeObjectURL).toHaveBeenCalledOnce()
        expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:photo-first')

        observer.setVisible(card, true)
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:photo-second'))
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(2)
    })

    test('keeps browser thumbnail object URLs to one synthetic scroll page and revokes detached URLs once', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const assets = Array.from({ length: 108 }, (_, index) => ({
            ...metadata,
            key: `photo-${index}`,
            name: `photo-${index}.webp`,
        }))
        const liveUrls = new Set<string>()
        const createdUrls: string[] = []
        let maxLiveUrls = 0
        inlayMocks.listInlayAssetMetadata.mockResolvedValue(assets)
        inlayMocks.getInlayAssetBlob.mockImplementation(async (id) => ({ ...asset, name: `${id}.webp` }))
        vi.mocked(URL.createObjectURL).mockImplementation(() => {
            const url = `blob:photo-${createdUrls.length}`
            createdUrls.push(url)
            liveUrls.add(url)
            maxLiveUrls = Math.max(maxLiveUrls, liveUrls.size)
            return url
        })
        vi.mocked(URL.revokeObjectURL).mockImplementation((url) => {
            liveUrls.delete(url)
        })
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(target.querySelectorAll('[data-inlay-preview-id]')).toHaveLength(36))
        const previewObserver = TestIntersectionObserver.instances.find((observer) =>
            [...observer.observed].some((element) => element.matches('[data-inlay-preview-id]')),
        )!

        for (const pageStart of [0, 36, 72]) {
            const cards = [...target.querySelectorAll<HTMLElement>('[data-inlay-preview-id]')].slice(pageStart, pageStart + 36)
            for (const card of cards) previewObserver.setVisible(card, true)
            await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(pageStart + 36))
            await vi.waitFor(() => expect(cards.every((card) => card.querySelector('img')?.hasAttribute('src'))).toBe(true))
            expect(liveUrls.size).toBeLessThanOrEqual(36)

            if (pageStart === 72) continue
            for (const card of cards) previewObserver.setVisible(card, false)
            await vi.waitFor(() => expect(cards.every((card) => card.querySelector('img')?.getAttribute('src') === null)).toBe(true))
            expect(liveUrls).toHaveLength(0)

            const sentinel = target.querySelector('.h-12')!
            const loadMoreObserver = TestIntersectionObserver.instances.find((observer) => observer.observed.has(sentinel))!
            loadMoreObserver.setVisible(sentinel, true)
            await vi.waitFor(() => expect(target.querySelectorAll('[data-inlay-preview-id]')).toHaveLength(pageStart + 72))
        }

        expect(maxLiveUrls).toBeLessThanOrEqual(36)
        await unmount(mounted)
        mounted = undefined
        expect(liveUrls).toHaveLength(0)
        expect(URL.revokeObjectURL).toHaveBeenCalledTimes(createdUrls.length)
        for (const url of createdUrls) {
            expect(URL.revokeObjectURL).toHaveBeenCalledWith(url)
        }
    })

    test.each([
        ['audio', 'pause'],
        ['video', 'ended'],
    ] as const)('pins a playing %s preview until %s offscreen', async (type, stopEvent) => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        inlayMocks.listInlayAssetMetadata.mockResolvedValue([{
            ...metadata,
            mime: `${type}/webm`,
            name: `clip.${type}`,
            ext: 'webm',
            inlayType: type,
            width: 640,
            height: 360,
        }])
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            ...asset,
            data: new Blob([type], { type: `${type}/webm` }),
            type,
            name: `clip.${type}`,
            width: 640,
            height: 360,
        })
        vi.mocked(URL.createObjectURL).mockReturnValue(`blob:${type}`)
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })
        await vi.waitFor(() => expect(target.querySelector('[data-inlay-preview-id="photo-id"]')).not.toBeNull())
        const card = target.querySelector('[data-inlay-preview-id="photo-id"]')!
        const observer = TestIntersectionObserver.instances[0]
        observer.setVisible(card, true)
        await vi.waitFor(() => expect(target.querySelector('source')?.getAttribute('src')).toBe(`blob:${type}`))
        const media = target.querySelector(type) as HTMLMediaElement
        let paused = false
        let ended = false
        Object.defineProperties(media, {
            paused: { configurable: true, get: () => paused },
            ended: { configurable: true, get: () => ended },
        })
        media.pause = vi.fn(() => { paused = true })
        media.dispatchEvent(new Event('play'))

        observer.setVisible(card, false)
        await tick()
        expect(media.pause).not.toHaveBeenCalled()
        expect(target.querySelector('source')?.getAttribute('src')).toBe(`blob:${type}`)
        expect(URL.revokeObjectURL).not.toHaveBeenCalled()

        if (stopEvent === 'pause') paused = true
        else ended = true
        media.dispatchEvent(new Event(stopEvent))
        await vi.waitFor(() => expect(target.querySelector('source')?.getAttribute('src')).toBeNull())
        expect(URL.revokeObjectURL).toHaveBeenCalledWith(`blob:${type}`)
    })
})
