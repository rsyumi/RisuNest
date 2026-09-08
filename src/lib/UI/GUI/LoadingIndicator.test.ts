// @vitest-environment happy-dom

import { afterEach, describe, expect, it } from 'vitest'
import { mount, unmount } from 'svelte'

import LoadingIndicator from './LoadingIndicator.svelte'

let mounted: ReturnType<typeof mount> | undefined

afterEach(async () => {
    if (mounted) await unmount(mounted)
    mounted = undefined
    document.body.replaceChildren()
})

function render(props: { label: string; detail?: string; compact?: boolean }) {
    const target = document.createElement('div')
    document.body.appendChild(target)
    mounted = mount(LoadingIndicator, { target, props })
    return target
}

describe('LoadingIndicator', () => {
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
