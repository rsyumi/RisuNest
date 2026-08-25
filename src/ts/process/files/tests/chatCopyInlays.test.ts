import { afterEach, describe, expect, test, vi } from 'vitest'

const inlayMocks = vi.hoisted(() => ({
    getInlayAssetBlob: vi.fn(),
    getInlayAssetMetadata: vi.fn(),
    getInlayAssetRenderUrl: vi.fn(),
}))

vi.mock('../inlays', () => inlayMocks)

import { copyImageSourceToDataUrl } from '../chatCopyInlays'
import {
    DeferredInlayMarkerRegistry,
    renderDeferredInlaySourceMarkup,
    withResolvedDeferredInlaySources,
} from '../inlayRenderSource'

describe('chat copy inlays', () => {
    afterEach(() => {
        vi.clearAllMocks()
        vi.unstubAllGlobals()
    })

    test('settles a corrupt image failure and revokes resolved inlay URLs', async () => {
        inlayMocks.getInlayAssetBlob.mockResolvedValue({
            data: new Blob(['corrupt'], { type: 'image/png' }), type: 'image', name: 'broken.png',
        })
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn(() => 'blob:copy-inlay'),
            revokeObjectURL,
        })
        vi.stubGlobal('fetch', vi.fn(async () => new Response(
            new Blob(['corrupt'], { type: 'image/png' }), { status: 200 },
        )))
        vi.stubGlobal('Image', class {
            crossOrigin = ''
            onload: (() => void) | null = null
            onerror: (() => void) | null = null
            set src(_: string) {
                queueMicrotask(() => this.onerror?.())
            }
        })
        const registry = new DeferredInlayMarkerRegistry()
        const doc = document.implementation.createHTMLDocument()
        doc.body.innerHTML = renderDeferredInlaySourceMarkup('broken', {
            url: '', mime: 'image/png', type: 'image', name: 'broken.png', size: 7, objectUrl: false,
        }, registry)
        let handled = false

        await withResolvedDeferredInlaySources(doc, registry, async () => {
            try {
                await copyImageSourceToDataUrl('blob:copy-inlay', 0.6)
            }
            catch {
                handled = true
            }
        })

        expect(handled).toBe(true)
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:copy-inlay')
    })
})
