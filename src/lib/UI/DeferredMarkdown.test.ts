// @vitest-environment happy-dom

import { afterEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
}))

vi.mock('src/ts/process/files/inlays', () => inlayMocks)
vi.mock('src/ts/parser/parser.svelte', async () => {
    const { renderDeferredInlaySourceMarkup } = await import('src/ts/process/files/inlayRenderSource')
    return {
        ParseMarkdown: vi.fn(async (data: string, ...args: unknown[]) => {
            const context = args[4] as { deferredInlays?: import('src/ts/process/files/inlayRenderSource').DeferredInlayMarkerRegistry }
            return renderDeferredInlaySourceMarkup(data, {
                url: '', mime: 'image/png', type: 'image', name: 'x', size: 1, objectUrl: false,
            }, context.deferredInlays)
        }),
    }
})

import DeferredMarkdown from './DeferredMarkdown.svelte'

describe('DeferredMarkdown', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
        vi.unstubAllGlobals()
    })

    test('owns browser inlay attachment and revokes it on destruction', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['x'], { type: 'image/png' }), type: 'image', name: 'x.png',
        })
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn(() => 'blob:deferred-markdown'),
            revokeObjectURL,
        })
        const target = document.createElement('div')
        document.body.append(target)

        mounted = mount(DeferredMarkdown, { target, props: { data: 'shared-inlay' } })
        await vi.waitFor(() => expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:deferred-markdown'))
        expect(inlayMocks.getInlayAssetBlob).toHaveBeenCalledWith('shared-inlay')

        await unmount(mounted)
        mounted = undefined
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:deferred-markdown')
    })
})
