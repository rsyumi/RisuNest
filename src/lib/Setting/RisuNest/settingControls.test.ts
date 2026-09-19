import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { language } from 'src/lang'
import SegmentedButtons from './SegmentedButtons.svelte'
import ButtonHarness from './SettingButton.test.svelte'
import ToggleHarness from './SettingToggle.test.svelte'

vi.mock('@lucide/svelte', () => ({ LoaderCircleIcon: () => {} }))
vi.mock('src/lang', () => ({ language: { loading: 'Loading synthetic operation' } }))

let target: HTMLDivElement
let mounted: ReturnType<typeof mount> | undefined

beforeEach(() => {
    target = document.createElement('div')
    document.body.append(target)
})

afterEach(async () => {
    try {
        if (mounted) await unmount(mounted)
    } finally {
        mounted = undefined
        target.remove()
    }
})

describe('mounted shared settings controls', () => {
    it.each(['group', 'radiogroup'] as const)('updates only the selected option in a %s', async (role) => {
        const onchange = vi.fn()
        mounted = mount(SegmentedButtons, {
            target,
            props: {
                value: 'normal',
                options: [
                    { value: 'normal', label: 'Normal' },
                    { value: 'low', label: 'Low memory' },
                    { value: 'fast', label: 'High capacity' },
                ],
                label: 'Synthetic profile',
                role,
                onchange,
            },
        })
        await tick()
        const group = target.querySelector(`[role="${role}"]`)!
        expect(group.getAttribute('aria-label')).toBe('Synthetic profile')
        const buttons = [...group.querySelectorAll('button')]
        const state = role === 'group' ? 'aria-pressed' : 'aria-checked'
        const unusedState = role === 'group' ? 'aria-checked' : 'aria-pressed'
        expect(buttons.map((button) => button.getAttribute(state))).toEqual(['true', 'false', 'false'])
        for (const button of buttons) {
            expect(button.hasAttribute(unusedState)).toBe(false)
            expect(button.getAttribute('role')).toBe(role === 'radiogroup' ? 'radio' : null)
        }
        buttons[1].click()
        await tick()
        expect(buttons.map((button) => button.getAttribute(state))).toEqual(['false', 'true', 'false'])
        expect(onchange).toHaveBeenCalledExactlyOnceWith('low')
        buttons[1].click()
        await tick()
        expect(onchange).toHaveBeenCalledTimes(1)
    })

    it('blocks repeated actions while busy and becomes usable after completion', async () => {
        let resolve!: () => void
        const pending = new Promise<void>((done) => { resolve = done })
        const perform = vi.fn(() => pending)
        mounted = mount(ButtonHarness, { target, props: { perform } })
        await tick()
        const button = target.querySelector('button')!
        expect(button.type).toBe('button')
        expect(button.name).toBe('save')
        expect(button.title).toBe('Save synthetic settings')
        expect(button.textContent).toContain('Save settings')
        expect(button.disabled).toBe(false)
        expect(button.hasAttribute('aria-busy')).toBe(false)
        try {
            button.click()
            await tick()
            expect(perform).toHaveBeenCalledTimes(1)
            expect(button.disabled).toBe(true)
            expect(button.getAttribute('aria-busy')).toBe('true')
            expect(button.textContent).toContain(language.loading)
            button.click()
            await tick()
            expect(perform).toHaveBeenCalledTimes(1)
        } finally {
            resolve()
            await pending
            await tick()
        }
        expect(button.disabled).toBe(false)
        expect(button.hasAttribute('aria-busy')).toBe(false)
        expect(button.textContent).not.toContain(language.loading)
        button.click()
        await tick()
        expect(perform).toHaveBeenCalledTimes(2)
    })

    it('respects explicit disabled state even when no action is pending', async () => {
        const perform = vi.fn(async () => undefined)
        mounted = mount(ButtonHarness, { target, props: { perform, disabled: true } })
        await tick()
        const button = target.querySelector('button')!
        expect(button.disabled).toBe(true)
        expect(button.hasAttribute('aria-busy')).toBe(false)
        button.click()
        await tick()
        expect(perform).not.toHaveBeenCalled()
    })

    it('updates the bound setting and notifies its owner when the checkbox changes', async () => {
        const onchange = vi.fn()
        mounted = mount(ToggleHarness, { target, props: { onchange } })
        await tick()
        const checkbox = target.querySelector('input')!
        expect(checkbox.type).toBe('checkbox')
        expect(checkbox.labels?.[0].textContent).toContain('Keep synthetic history')
        expect(checkbox.checked).toBe(false)
        expect(target.querySelector('output')?.textContent).toBe('false')
        checkbox.click()
        await tick()
        expect(checkbox.checked).toBe(true)
        expect(target.querySelector('output')?.textContent).toBe('true')
        expect(onchange).toHaveBeenCalledExactlyOnceWith(true)
        checkbox.click()
        await tick()
        expect(checkbox.checked).toBe(false)
        expect(target.querySelector('output')?.textContent).toBe('false')
        expect(onchange.mock.calls).toEqual([[true], [false]])
    })

    it('does not change a disabled checkbox or its bound value', async () => {
        const onchange = vi.fn()
        mounted = mount(ToggleHarness, { target, props: { onchange, disabled: true } })
        await tick()
        const checkbox = target.querySelector('input')!
        expect(checkbox.disabled).toBe(true)
        checkbox.click()
        await tick()
        expect(checkbox.checked).toBe(false)
        expect(target.querySelector('output')?.textContent).toBe('false')
        expect(onchange).not.toHaveBeenCalled()
    })
})
