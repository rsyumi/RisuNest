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
import DeferredMarkdownHarness from './DeferredMarkdownHarness.test.svelte'
import { ParseMarkdown } from 'src/ts/parser/parser.svelte'

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
        vi.stubGlobal('IntersectionObserver', undefined)
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

    test('retains media leases during a pending and identical refresh, releasing only replaced media', async () => {
        vi.stubGlobal('IntersectionObserver', undefined)
        inlayMocks.getInlayAssetBlob.mockImplementation(async (id: string) => ({
            data: new Blob([id]),
            type: 'image',
            name: 'x.png',
        }))
        const revokeObjectURL = vi.fn()
        vi.stubGlobal('URL', {
            ...URL,
            createObjectURL: vi.fn((blob: Blob) => `blob:${blob.size}`),
            revokeObjectURL,
        })
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(DeferredMarkdownHarness, { target })
        await vi.waitFor(() =>
            expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:5'),
        )
        const image = target.querySelector('img')
        const parseCount = vi.mocked(ParseMarkdown).mock.calls.length
        ;(mounted as { refresh(value?: string): void }).refresh()
        await vi.waitFor(() =>
            expect(ParseMarkdown).toHaveBeenCalledTimes(parseCount + 1),
        )
        expect(target.querySelector('img')).toBe(image)
        expect(revokeObjectURL).not.toHaveBeenCalled()

        const implementation = vi.mocked(ParseMarkdown).getMockImplementation()!
        let finish!: () => void
        vi.mocked(ParseMarkdown).mockImplementationOnce(async (...args) => {
            await new Promise<void>((resolve) => {
                finish = resolve
            })
            return implementation(...args)
        })
        ;(mounted as { refresh(value?: string): void }).refresh('second')
        await vi.waitFor(() => expect(finish).toBeTypeOf('function'))
        expect(target.querySelector('img')).toBe(image)
        expect(revokeObjectURL).not.toHaveBeenCalled()
        finish()
        await vi.waitFor(() =>
            expect(target.querySelector('img')?.getAttribute('src')).toBe('blob:6'),
        )
        expect(revokeObjectURL).toHaveBeenCalledExactlyOnceWith('blob:5')
        await unmount(mounted)
        mounted = undefined
        expect(revokeObjectURL.mock.calls).toEqual([['blob:5'], ['blob:6']])
    })


})
