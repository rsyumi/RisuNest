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

vi.mock('src/ts/process/files/inlays', () => inlayMocks)
vi.mock('src/ts/platform', () => ({ isTauri: true }))
vi.mock('src/ts/alert', () => ({ alertConfirm: vi.fn() }))
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

describe('PlaygroundInlayExplorer native previews', () => {
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
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
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/photo-id?thumb=256')
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    test('lists metadata and requests a 256 thumbnail without loading the payload', async () => {
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(target.querySelector('img')).not.toBeNull())
        await tick()

        expect(inlayMocks.listInlayAssetMetadata).toHaveBeenCalledOnce()
        expect(inlayMocks.listInlayAssetMetadata).toHaveBeenCalledWith({ migrateLegacy: false })
        expect(inlayMocks.listInlayAssets).not.toHaveBeenCalled()
        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledWith('photo-id', 256)
        expect(inlayMocks.getInlayAssetBlob).not.toHaveBeenCalled()
        expect(target.querySelector('img')?.getAttribute('src')).toBe('http://risuasset.localhost/photo-id?thumb=256')
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

        expect(inlayMocks.getInlayAssetRenderUrl).toHaveBeenCalledWith('photo-avif', undefined)
        expect(target.querySelector('img')?.getAttribute('src')).toBe('http://risuasset.localhost/photo-avif')
    })
})
