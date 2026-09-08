// @vitest-environment happy-dom

import { afterEach, describe, expect, it } from 'vitest'
import { flushSync, mount, unmount } from 'svelte'
import { createClassComponent } from 'svelte/legacy'

import LoadingIndicator from './LoadingIndicator.svelte'

let mounted: ReturnType<typeof mount> | undefined

afterEach(async () => {
    if (mounted) await unmount(mounted)
    mounted = undefined
    document.body.replaceChildren()
})

function render(props: {
    label: string
    detail?: string
    elapsedText?: string
    compact?: boolean
}) {
    const target = document.createElement('div')
    document.body.appendChild(target)
    mounted = mount(LoadingIndicator, { target, props })
    return target
}

describe('LoadingIndicator', () => {
    it('updates the announced stage while elapsed time stays outside the live region', () => {
        const target = document.createElement('div')
        document.body.appendChild(target)
        const component = createClassComponent({
            component: LoadingIndicator,
            target,
            props: { label: 'Loading', detail: 'Opening storage', elapsedText: '0s elapsed' },
        })
        try {
            flushSync(() =>
                component.$set({ detail: 'Preparing plugins', elapsedText: '2s elapsed' }),
            )
            const status = target.querySelector('[role="status"]')
            expect(status?.textContent).toContain('Preparing plugins')
            expect(status?.textContent).not.toContain('Opening storage')
            expect(status?.textContent).not.toContain('elapsed')
            expect(target.querySelector('[aria-live="off"]')?.textContent).toBe('2s elapsed')
            flushSync(() => component.$set({ elapsedText: '3s elapsed' }))
            expect(status?.textContent).toContain('Preparing plugins')
            expect(target.querySelector('[aria-live="off"]')?.textContent).toBe('3s elapsed')
        } finally {
            component.$destroy()
        }
    })

    it('announces its label and detail while hiding the decorative ring', () => {
        const target = render({
            label: 'Loading chats',
            detail: 'Opening conversation',
        })
        const status = target.querySelector('[role="status"]')

        expect(status?.getAttribute('aria-live')).toBe('polite')
        expect(status?.textContent?.replace(/\s+/g, ' ').trim()).toBe(
            'Loading chats Opening conversation',
        )
        expect(status?.querySelector('[aria-hidden="true"]')).not.toBeNull()
    })

    it('supports compact inline presentation without requiring detail text', () => {
        const target = render({ label: 'Loading', compact: true })
        const status = target.querySelector('[role="status"]')

        expect(status?.textContent?.trim()).toBe('Loading')
        expect(status?.classList.contains('compact')).toBe(true)
    })
})
