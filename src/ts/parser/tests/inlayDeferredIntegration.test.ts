import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { writable } from 'svelte/store'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
}))

const databaseState = vi.hoisted(() => ({
    db: {
        characters: [{ chatPage: 0, chats: [{}], defaultVariables: '' }],
        globalChatVariables: {},
        templateDefaultVariables: '',
        hideAllImages: false,
    },
}))

vi.mock(import('../../process/files/inlays'), () => inlayMocks)
vi.mock(import('../../platform'), () => ({ isTauri: false, isNodeServer: false }))
vi.mock(
    import('../../storage/database.svelte'),
    () => ({
        appVer: '1234.5.67',
        getCurrentCharacter: () => ({}),
        getDatabase: () => databaseState.db,
    } as typeof import('../../storage/database.svelte')),
)
vi.mock(import('../../globalApi.svelte'), () => ({
    aiWatermarkingLawApplies: () => false,
    getFileSrc: () => Promise.resolve(''),
}))
vi.mock(import('../../stores.svelte'), () => ({
    DBState: databaseState,
    selIdState: { selId: 0 },
    selectedCharID: writable(0),
} as typeof import('../../stores.svelte')))

import { ParseMarkdown } from '../parser.svelte'
import { DeferredInlayMarkerRegistry, mountDeferredInlaySources } from '../../process/files/inlayRenderSource'

describe('deferred inlay parser integration', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        vi.stubGlobal('IntersectionObserver', undefined)
        databaseState.db.hideAllImages = false
        inlayMocks.getInlayAssetMetadata.mockResolvedValue({
            key: 'image-id',
            kind: 'inlay',
            size: 1,
            mime: 'image/png',
            name: 'image.png',
            ext: 'png',
            inlayType: 'image',
        })
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['x'], { type: 'image/png' }),
            type: 'image',
            name: 'image.png',
        })
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn(() => 'blob:parsed-inlay'),
            revokeObjectURL: vi.fn(),
        })
        document.body.replaceChildren()
    })

    afterEach(() => {
        document.body.replaceChildren()
        vi.unstubAllGlobals()
    })

    test('loads a generated image marker once after the sanitized parser output is mounted', async () => {
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown('{{inlay::image-id}}', null, 'back', -1, {}, { deferredInlays: registry })
        document.body.append(root)

        const cleanup = mountDeferredInlaySources(root, registry)

        await vi.waitFor(() => expect(root.querySelector('img')?.getAttribute('src')).toBe('blob:parsed-inlay'))
        expect(inlayMocks.getInlayAssetMetadata).toHaveBeenCalledWith('image-id')
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1)
        cleanup()
    })

    test('keeps cache and edit markup byte-stable with or without lifecycle ownership', async () => {
        for (const mode of ['pretranslate', 'notrim'] as const) {
            const firstCacheKey = await ParseMarkdown('{{inlay::image-id}}', null, mode)
            const secondCacheKey = await ParseMarkdown('{{inlay::image-id}}', null, mode)
            const ownedRegistry = new DeferredInlayMarkerRegistry()
            const ownedMarkup = await ParseMarkdown(
                '{{inlay::image-id}}', null, mode, -1, {}, { deferredInlays: ownedRegistry },
            )

            expect(firstCacheKey).toBe(secondCacheKey)
            expect(ownedMarkup).toBe(firstCacheKey)
            expect(firstCacheKey).toContain('data-risu-inlay-slot')
            expect(firstCacheKey).not.toContain('data-risu-inlay-token')
        }
    })

    test('keeps raw forged marker attributes inert after parsing and sanitizing', async () => {
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown(
            '<img data-risu-inlay-id="forged" data-risu-inlay-token="forged.image.Zm9yZ2Vk">',
            null,
            'back',
        )
        document.body.append(root)

        mountDeferredInlaySources(root, registry)
        await Promise.resolve()

        expect(inlayMocks.getInlayAssetMetadata).not.toHaveBeenCalled()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
        expect(root.querySelector('img')?.getAttribute('src')).toBeNull()
    })

    test('rejects the old prefix, type, and base64 ID token form', async () => {
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown(
            '<img data-risu-inlay-id="other-id" data-risu-inlay-token="observed.image.b3RoZXItaWQ=">',
            null,
            'back',
        )
        document.body.append(root)

        mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()

        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalledWith('other-id')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })

    test('binds an observed and reused slot to its immutable original asset', async () => {
        const registry = new DeferredInlayMarkerRegistry()
        const parsed = await ParseMarkdown('{{inlay::image-id}}', null, 'back', -1, {}, { deferredInlays: registry })
        const slot = parsed.match(/data-risu-inlay-slot="([^"]+)"/)?.[1]
        expect(slot).toBeTruthy()
        const altered = parsed.replace('data-risu-inlay-id="image-id"', 'data-risu-inlay-id="other-id"')
        const root = document.createElement('div')
        root.innerHTML = `${altered}${parsed}`
        document.body.append(root)

        mountDeferredInlaySources(root, registry)
        await Promise.resolve(); await Promise.resolve()

        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(1)
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledWith('image-id')
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalledWith('other-id')
        expect(root.querySelectorAll('[src="blob:parsed-inlay"]')).toHaveLength(1)
    })

    test('leaves parser output inert when its caller does not own a registry', async () => {
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown('{{inlay::image-id}}', null, 'back')
        document.body.append(root)

        mountDeferredInlaySources(root, new DeferredInlayMarkerRegistry())
        await Promise.resolve(); await Promise.resolve()

        expect(root.querySelector('[data-risu-inlay-token]')).toBeNull()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })

    test('does not load a hidden generated image inlay', async () => {
        databaseState.db.hideAllImages = true
        const registry = new DeferredInlayMarkerRegistry()
        const root = document.createElement('div')
        root.innerHTML = await ParseMarkdown('{{inlay::image-id}}', null, 'back', -1, {}, { deferredInlays: registry })
        document.body.append(root)

        mountDeferredInlaySources(root, registry)
        await Promise.resolve()

        expect(root.querySelector('[data-risu-inlay-token]')).toBeNull()
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
    })
})
