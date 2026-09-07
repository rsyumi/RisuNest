import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import SidebarAvatar from './SidebarAvatar.svelte'

vi.mock('src/ts/gui/tooltip', () => ({
    tooltipRight: () => ({ destroy() {} }),
}))

function deferred<T>() {
    let reject!: (reason?: unknown) => void
    const promise = new Promise<T>((_resolve, rejectPromise) => {
        reject = rejectPromise
    })
    return { promise, reject }
}

describe('SidebarAvatar', () => {
    let mounted: ReturnType<typeof mount> | null = null

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = null
        document.body.replaceChildren()
    })

    it('renders the avatar placeholder when its source promise rejects', async () => {
        const source = deferred<string>()
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(SidebarAvatar, {
            target,
            props: {
                rounded: true,
                src: source.promise,
                name: 'Synthetic avatar',
            },
        })

        source.reject(new Error('synthetic avatar failure'))
        await tick()

        expect(target.querySelector('img')).toBeNull()
        expect(target.querySelector('.sidebar-avatar')).not.toBeNull()
    })

    it('renders the slot placeholder when its background promise rejects', async () => {
        const background = deferred<string>()
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(SidebarAvatar, {
            target,
            props: {
                rounded: false,
                src: 'slot',
                backgroundimg: background.promise,
                name: 'Synthetic slot avatar',
            },
        })

        background.reject(new Error('synthetic background failure'))
        await tick()

        const placeholder = target.querySelector<HTMLElement>('.sidebar-avatar')
        expect(placeholder).not.toBeNull()
        expect(placeholder?.style.backgroundImage).toBe('')
    })
})
