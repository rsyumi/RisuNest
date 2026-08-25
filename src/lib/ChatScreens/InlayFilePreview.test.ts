// @vitest-environment happy-dom

import { afterEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

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

describe('InlayFilePreview', () => {
    let mounted: ReturnType<typeof mount> | undefined

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
})
