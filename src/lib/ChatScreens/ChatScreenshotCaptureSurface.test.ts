// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { Message } from 'src/ts/storage/database.svelte'

vi.mock('./Chat.svelte', async () => ({
    default: (await import('./ChatScreenshotProbe.test.svelte')).default,
}))

import ChatScreenshotCaptureSurface from './ChatScreenshotCaptureSurface.svelte'

type SurfaceInstance = {
    mountBatch(messages: readonly Message[], firstTurn: number, signal: AbortSignal): Promise<HTMLElement>
    unmountBatch(): Promise<void>
    dispose(): Promise<void>
}

describe('ChatScreenshotCaptureSurface', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        target = document.createElement('div')
        document.body.innerHTML = '<main class="default-chat-screen"><div data-live="preserved">live</div></main>'
        document.body.append(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
    })

    test('mounts only the current chronological batch outside the live chat subtree', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, {
            target,
            props: {
                totalTurns: 3,
                characterName: 'Character',
                currentUsername: 'User',
            },
        })
        const surface = mounted as SurfaceInstance
        const live = document.querySelector('.default-chat-screen')!.innerHTML
        const controller = new AbortController()

        const root = await surface.mountBatch(
            [
                { role: 'user', data: 'first' },
                { role: 'char', data: 'second' },
            ],
            1,
            controller.signal,
        )

        expect(root.closest('.default-chat-screen')).toBeNull()
        expect([...root.querySelectorAll('[data-capture-probe]')].map((element) => element.textContent)).toEqual([
            'first',
            'second',
        ])
        expect(document.querySelector('.default-chat-screen')!.innerHTML).toBe(live)

        await surface.mountBatch([{ role: 'user', data: 'third' }], 3, controller.signal)
        expect(root.querySelectorAll('[data-capture-probe]')).toHaveLength(1)
        expect(root.textContent).toContain('third')
        expect(root.textContent).not.toContain('first')
    })

    test('rejects pending readiness on abort and releases the batch', async () => {
        mounted = mount(ChatScreenshotCaptureSurface, {
            target,
            props: {
                totalTurns: 1,
                characterName: 'Character',
                currentUsername: 'User',
            },
        })
        const surface = mounted as SurfaceInstance
        const controller = new AbortController()
        const pending = surface.mountBatch(
            [{ role: 'char', data: 'pending' }],
            1,
            controller.signal,
        )

        controller.abort()
        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        await surface.unmountBatch()

        expect(target.querySelectorAll('[data-capture-probe]')).toHaveLength(0)
    })
})
