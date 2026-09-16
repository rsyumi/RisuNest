// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { InlayBlobMetadata, InlayBlobType } from 'src/ts/storage/blobStore'

const encodeOptions = { format: 'webp', quality: 85, maxDimension: 0, skipReencode: true } as const
const inlays = vi.hoisted(() => ({
    listInlayAssetMetadata: vi.fn(),
    getInlayEncodeOptions: vi.fn(() => ({ format: 'webp', quality: 85, maxDimension: 0, skipReencode: true })),
}))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(async (_message: string) => true) }))
const optimization = vi.hoisted(() => ({
    read: vi.fn(async () => new Uint8Array(2048)),
    encodeNewInlayImage: vi.fn(async (_key: string, _data: Uint8Array, input: { name: string }) => ({
        data: new Uint8Array(512),
        metadata: { kind: 'inlay', inlayType: 'image', mime: 'image/webp', name: input.name, ext: 'webp' },
    })),
    write: vi.fn(async () => undefined),
}))
vi.mock('src/ts/process/files/inlays', () => inlays)
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/platform', () => ({ isTauri: true }))
vi.mock('src/lang', async () => ({ language: (await import('src/lang/en')).languageEnglish }))
vi.mock('src/ts/storage/sync/serverSyncProduction', () => ({
    getServerSyncController: () => ({ snapshot: () => ({ running: false, paused: false, error: '' }) }),
}))
vi.mock('src/ts/storage/sync/serverAssetResidency', () => ({ getAssetResidencyStatus: vi.fn() }))
vi.mock('src/ts/process/files/inlayOptimizationRuntime', () => ({
    readInlayOptimizationEnvironment: async () => ({ syncConfigured: false }),
    createStoredInlayOptimizationDeps: () => ({
        read: optimization.read,
        encoder: { encodeNewInlayImage: optimization.encodeNewInlayImage },
        write: optimization.write,
    }),
}))

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
        inlays.getInlayEncodeOptions.mockReturnValue({ ...encodeOptions })
        alerts.alertConfirm.mockResolvedValue(true)
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

        expect(inlays.listInlayAssetMetadata).toHaveBeenCalledWith()
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

    it('confirms the irreversible run, converts, and reports the saving', async () => {
        inlays.listInlayAssetMetadata.mockResolvedValue(stored)
        const target = setup()
        press(target, 'Load')
        await vi.waitFor(() => expect(rows(target)).toHaveLength(4))

        press(target, 'Convert to WebP')
        await vi.waitFor(() => expect(target.querySelector('[data-inlay-optimize-result]')).not.toBeNull())

        expect(alerts.alertConfirm).toHaveBeenCalledOnce()
        expect(alerts.alertConfirm.mock.calls[0][0]).toContain('2 images (2.5 KiB)')
        expect(optimization.write).toHaveBeenCalledTimes(2)
        expect(target.querySelector('[data-inlay-optimize-result]')?.textContent)
            .toBe('Converted 2 and saved 3.0 KiB. Left alone 0, failed 0.')
        expect(inlays.listInlayAssetMetadata).toHaveBeenCalledTimes(2)
    })

    it('writes nothing when the confirmation is declined', async () => {
        inlays.listInlayAssetMetadata.mockResolvedValue(stored)
        alerts.alertConfirm.mockResolvedValue(false)
        const target = setup()
        press(target, 'Load')
        await vi.waitFor(() => expect(rows(target)).toHaveLength(4))

        press(target, 'Convert to WebP')
        await vi.waitFor(() => expect(alerts.alertConfirm).toHaveBeenCalledOnce())

        expect(optimization.write).not.toHaveBeenCalled()
        expect(target.querySelector('[data-inlay-optimize-result]')).toBeNull()
    })

    it('says there is nothing to convert when every image is already WebP', async () => {
        inlays.listInlayAssetMetadata.mockResolvedValue([asset('webp', 1024), asset('mp3', 4096, 'audio')])
        const target = setup()
        press(target, 'Load')
        await vi.waitFor(() => expect(rows(target)).toHaveLength(2))

        press(target, 'Convert to WebP')
        await vi.waitFor(() => expect(target.querySelector('[data-inlay-optimize-result]')?.textContent)
            .toBe('There is nothing to convert'))

        expect(alerts.alertConfirm).not.toHaveBeenCalled()
        expect(optimization.write).not.toHaveBeenCalled()
    })
})
