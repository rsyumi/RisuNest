import { beforeEach, describe, expect, test, vi } from 'vitest'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
    listInlayAssetMetadata: vi.fn(),
}))

vi.mock('../inlays', () => inlayMocks)

import {
    getInlayRenderSource,
    getInlayRenderSources,
    getNativeInlayThumbnailSize,
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
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/video-id?thumb=256')

        await expect(getInlayRenderSource('video-id', true, 256)).resolves.toEqual({
            url: 'http://risuasset.localhost/video-id?thumb=256',
            mime: 'video/webm',
            type: 'video',
            name: 'clip.webm',
            size: 42,
            objectUrl: false,
        })
        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledWith('video-id', 256)
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

    test.each([
        ['image/gif', 256],
        ['image/jpeg', 256],
        ['image/png', 256],
        ['image/webp', 256],
        ['image/avif', undefined],
        ['image/svg+xml', undefined],
    ] as const)('selects the native thumbnail size for %s', (mime, expected) => {
        expect(getNativeInlayThumbnailSize({ mime, inlayType: 'image' }, true)).toBe(expected)
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
