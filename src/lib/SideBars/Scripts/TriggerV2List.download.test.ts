// @vitest-environment happy-dom

import { afterEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'

const objectUrlMocks = vi.hoisted(() => ({
    downloadBlobWithObjectUrl: vi.fn(),
}))

vi.mock('src/ts/objectUrl', () => objectUrlMocks)
vi.mock('src/ts/process/triggers', () => ({
    displayAllowList: [],
    requestAllowList: [],
}))
vi.mock('src/ts/alert', () => ({ alertMd: vi.fn() }))
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', async () => ({
    default: (await import('src/lib/UI/GUI/PortalConsumer.svelte')).default,
}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        db: {
            characters: [],
            showDeprecatedTriggerV2: false,
        },
    },
}))

import TriggerV2List from './TriggerV2List.svelte'

describe('TriggerV2List export', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    test('downloads the exported triggers through the deferred object URL helper', async () => {
        const target = document.createElement('div')
        document.body.appendChild(target)
        mounted = mount(TriggerV2List, {
            target,
            props: {
                value: [
                    { comment: '', type: 'start', conditions: [], effect: [] },
                    { comment: 'Export me', type: 'manual', conditions: [], effect: [] },
                ],
            },
        })

        target.querySelector('button')?.click()
        await vi.waitFor(() => expect(document.body.querySelector('svg.lucide-download')).not.toBeNull())
        document.body.querySelector('svg.lucide-download')?.closest('button')?.click()

        expect(objectUrlMocks.downloadBlobWithObjectUrl).toHaveBeenCalledOnce()
        const [blob, filename] = objectUrlMocks.downloadBlobWithObjectUrl.mock.calls[0]
        await expect(blob.text()).resolves.toContain('"comment": "Export me"')
        expect(blob.type).toBe('application/json')
        expect(filename).toMatch(/^triggers-\d+\.json$/)
    })
})
