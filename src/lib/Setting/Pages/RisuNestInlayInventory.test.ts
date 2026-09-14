// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { InlayBlobMetadata, InlayBlobType } from 'src/ts/storage/blobStore'

const inlays = vi.hoisted(() => ({ listInlayAssetMetadata: vi.fn() }))
vi.mock('src/ts/process/files/inlays', () => inlays)
vi.mock('src/ts/platform', () => ({ isTauri: true }))
vi.mock('src/lang', async () => ({ language: (await import('src/lang/en')).languageEnglish }))

import RisuNestInlayInventory from './RisuNestInlayInventory.svelte'

function asset(ext: string, size: number, inlayType: InlayBlobType = 'image'): InlayBlobMetadata {
    return { key: `${inlayType}-${ext}-${size}`, kind: 'inlay', size, mime: '', name: `a.${ext}`, ext, inlayType }
}

const stored = [
    asset('webp', 1024), asset('webp', 1024), asset('webp', 1024),
    asset('png', 2048), asset('', 512), asset('mp3', 4096, 'audio'),
]

describe('RisuNestInlayInventory', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    function setup(): HTMLElement {
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestInlayInventory, { target })
        return target
    }

    function press(target: HTMLElement, label: string): void {
        const button = [...target.querySelectorAll('button')]
            .find((candidate) => candidate.textContent?.trim() === label)
        expect(button, `no ${label} button`).toBeDefined()
        button?.click()
    }

    function rows(target: HTMLElement): string[] {
        return [...target.querySelectorAll('[data-inlay-inventory-row]')].map((row) => (
            [...row.querySelectorAll('td')]
                .map((cell) => cell.textContent?.replace(/\s+/g, ' ').trim() ?? '')
                .filter(Boolean)
                .join(' ')
        ))
    }

    it('reads nothing until the load button is pressed', () => {
        const target = setup()

        expect(inlays.listInlayAssetMetadata).not.toHaveBeenCalled()
        expect(rows(target)).toEqual([])
    })

    it('lists extensions by count with the totals after loading', async () => {
        inlays.listInlayAssetMetadata.mockResolvedValue(stored)
        const target = setup()

        press(target, 'Load')
        await vi.waitFor(() => expect(rows(target)).toHaveLength(4))

        expect(inlays.listInlayAssetMetadata).toHaveBeenCalledWith({ migrateLegacy: false })
        expect(target.querySelector('[data-inlay-inventory-summary]')?.textContent).toBe('6 files, 9.5 KiB')
        expect(rows(target)).toEqual([
            'webp 3 3.0 KiB',
            'No extension 1 512 bytes',
            'png 1 2.0 KiB',
            'mp3 1 4.0 KiB',
        ])
        expect(target.textContent).toContain('Audio, video, and signatures')
    })

    it('reports an empty store instead of an extension table', async () => {
        inlays.listInlayAssetMetadata.mockResolvedValue([])
        const target = setup()

        press(target, 'Load')
        await vi.waitFor(() => expect(target.textContent).toContain('No attachments are stored'))

        expect(rows(target)).toEqual([])
        expect(target.textContent).not.toContain('Audio, video, and signatures')
    })

    it('reports a failed read and loads again on the next press', async () => {
        inlays.listInlayAssetMetadata.mockRejectedValueOnce(new Error('unavailable'))
        inlays.listInlayAssetMetadata.mockResolvedValue(stored)
        const target = setup()

        press(target, 'Load')
        await vi.waitFor(() => expect(target.querySelector('[role="alert"]')?.textContent)
            .toContain("Couldn't load the stored attachments."))

        press(target, 'Load')
        await vi.waitFor(() => expect(rows(target)).toHaveLength(4))
        expect(target.querySelector('[role="alert"]')).toBeNull()
    })
})
