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

describe('PlaygroundInlayExplorer native previews', () => {
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
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
        inlayMocks.getInlayAssetRenderUrl.mockResolvedValue('http://risuasset.localhost/photo-id?thumb=256')
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.restoreAllMocks()
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
        resolveUrl('http://risuasset.localhost/photo-id?thumb=256')
        await Promise.resolve()
        await Promise.resolve()

        expect(revokeObjectURL).not.toHaveBeenCalled()
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

    test('retries after a failed preview lookup', async () => {
        inlayMocks.getInlayAssetBlob
            .mockRejectedValueOnce(new Error('preview failed'))
            .mockResolvedValueOnce(asset)
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(PlaygroundInlayExplorer, { target })

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledOnce())
        target.querySelector<HTMLInputElement>('input[type="checkbox"]')?.click()

        await vi.waitFor(() => expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledTimes(2))
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:photo-preview'))
    })
})
