// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'
import { alertStore } from 'src/ts/stores.svelte'

const branchMocks = vi.hoisted(() => ({
    getChatBranches: vi.fn(),
}))

vi.mock('src/ts/gui/branches', () => branchMocks)
vi.mock('src/ts/stores.svelte', async () => (await import('./AlertComp.testSupport')).storesModule({ alertQueue: true }))
vi.mock('src/ts/util', () => ({ sleep: vi.fn() }))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => ({}) }))
vi.mock('src/ts/storage/deviceMarkers', () => ({ getDeviceMarkers: vi.fn() }))
vi.mock('../../ts/characters', async () => (await import('./AlertComp.testSupport')).charactersModule())
vi.mock('../../ts/parser/parser.svelte', async () => (await import('../../ts/parser/parser.testSupport')).parserStubModule({ ParseMarkdown: vi.fn() }))
vi.mock('src/ts/characterCards', async () => (await import('./AlertComp.testSupport')).characterCardsModule())
vi.mock('src/ts/globalApi.svelte', () => ({
    aiLawApplies: () => false,
    getFetchData: vi.fn(),
    getFetchLogs: () => [],
    openURL: vi.fn(),
}))
vi.mock('src/ts/tokenizer', async () => (await import('./AlertComp.testSupport')).tokenizerModule())
vi.mock('src/ts/gui/colorscheme', async () => (await import('./AlertComp.testSupport')).colorschemeModule())
vi.mock('../../ts/sourcemap', async () => (await import('./AlertComp.testSupport')).sourcemapModule())
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/storage/officialAccountMessage', async () => (await import('./AlertComp.testSupport')).officialAccountMessageModule())
vi.mock('@lucide/svelte', async () => (await import('./AlertComp.testSupport')).lucideModule())
vi.mock('../SideBars/BarIcon.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('../UI/GUI/TextInput.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('../UI/GUI/Button.svelte', async () => (await import('./AlertComp.testSupport')).componentModule('button'))
vi.mock('../UI/GUI/SelectInput.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('../UI/GUI/OptionInput.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('../UI/DeferredMarkdown.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('../UI/GUI/TextAreaInput.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('../Setting/Pages/Module/ModuleChatMenu.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())
vi.mock('./Help.svelte', async () => (await import('./AlertComp.testSupport')).componentModule())

vi.mock('src/ts/process/index.svelte', async () => ({
    doingChat: (await import('svelte/store')).writable(false),
    previewBody: '',
    sendChat: vi.fn(),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({ deactivateActiveWorkingSet: vi.fn() }))

import { alertActionConfirm, alertCheckboxConfirm } from 'src/ts/alert'
import { initHotkey } from 'src/ts/hotkey'
import AlertComp from './AlertComp.svelte'

let mounted: ReturnType<typeof mount> | undefined
let target: HTMLDivElement
const options = {
    title: 'Replace data?', description: 'Current data is cleared.', checkboxLabel: 'Clear current data', actionLabel: 'Replace', cancelLabel: 'Cancel', requireChecked: true,
    details: [{ label: 'File', value: 'synthetic.risunest · 1.0 KiB' }, { label: 'Type', value: 'RisuNest backup' }],
}
afterEach(async () => {
    alertStore.set({ type: 'none', msg: '' })
    if (mounted) await unmount(mounted)
    mounted = undefined
    document.body.replaceChildren()
})
async function show(requireChecked = true) {
    const result = alertCheckboxConfirm({ ...options, requireChecked })
    target = document.createElement('div')
    document.body.append(target)
    mounted = mount(AlertComp, { target })
    await tick()
    return { result }
}
function action(label: string) {
    return [...target.querySelectorAll('button')].find(button => button.textContent?.trim() === label)!
}
describe('checkbox confirmations', () => {
    it('gates acknowledgement until checked and reports the selection', async () => {
        const { result } = await show()
        expect(target.textContent).toContain(options.description)
        expect([...target.querySelectorAll('dl > *')].map(row => `${row.tagName}:${row.textContent}`))
            .toEqual(['DT:File', 'DD:synthetic.risunest · 1.0 KiB', 'DT:Type', 'DD:RisuNest backup'])
        expect(action('Replace').disabled).toBe(true)
        const checkbox = target.querySelector<HTMLInputElement>('input')!
        expect(checkbox.checked).toBe(false)
        checkbox.click()
        await tick()
        expect(action('Replace').disabled).toBe(false)
        action('Replace').click()
        await expect(result).resolves.toEqual({ confirmed: true, checked: true })
    })
    it.each([false, true])('allows optional action with selection %s', async checked => {
        const { result } = await show(false)
        expect(action('Replace').disabled).toBe(false)
        if (checked) { target.querySelector<HTMLInputElement>('input')!.click(); await tick() }
        action('Replace').click()
        await expect(result).resolves.toEqual({ confirmed: true, checked })
    })
    it.each(['button', 'Escape', 'overlay'])('cancels by %s without confirmation', async mode => {
        const { result } = await show()
        if (mode === 'button') action('Cancel').click()
        else if (mode === 'Escape') window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
        else target.querySelector<HTMLButtonElement>('button[aria-label="Cancel"]')!.click()
        await expect(result).resolves.toEqual({ confirmed: false, checked: false })
    })
    it('resolves cancellation when the dialog closes without a result', async () => {
        const { result } = await show()
        alertStore.set({ type: 'none', msg: '' })
        await expect(result).resolves.toEqual({ confirmed: false, checked: false })
    })
    it('keeps a line break in the title', async () => {
        const result = alertCheckboxConfirm({ ...options, title: 'Remove the plugin?\nSynthetic plugin' })
        target = document.createElement('div')
        document.body.append(target)
        mounted = mount(AlertComp, { target })
        await tick()
        const title = target.querySelector('#checkbox-confirm-title')!
        expect(title.textContent).toBe('Remove the plugin?\nSynthetic plugin')
        expect(title.classList.contains('whitespace-pre-wrap')).toBe(true)
        action('Cancel').click()
        await result
    })
    it('starts a consecutive dialog unchecked', async () => {
        const { result } = await show(false)
        target.querySelector<HTMLInputElement>('input')!.click()
        await tick()
        action('Replace').click()
        await result
        const next = alertCheckboxConfirm({ ...options })
        await tick()
        expect(target.querySelector<HTMLInputElement>('input')!.checked).toBe(false)
        expect(action('Replace').disabled).toBe(true)
        action('Cancel').click()
        await next
    })
    it('cancels only the checkbox dialog when the hotkey and dialog Escape handlers both run', async () => {
        const listening = vi.spyOn(document, 'addEventListener')
        initHotkey()
        try {
            const { result } = await show()
            const next = alertCheckboxConfirm({ ...options, title: 'Next?' })
            expect(listening.mock.calls.some(([type]) => type === 'keydown')).toBe(true)
            document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
            await expect(result).resolves.toEqual({ confirmed: false, checked: false })
            await vi.waitFor(() => expect(get(alertStore)).toMatchObject({ type: 'checkboxConfirm', msg: 'Next?' }))
            action('Cancel').click()
            await expect(next).resolves.toEqual({ confirmed: false, checked: false })
        } finally {
            for (const [type, handler] of listening.mock.calls) document.removeEventListener(type, handler as EventListener)
            listening.mockRestore()
        }
    })
})

describe('action confirmations', () => {
    const choice = { title: 'Keep these off?', description: 'Left off: Sync', actionLabel: 'Keep off', cancelLabel: 'Only this time' }
    async function ask() {
        const result = alertActionConfirm(choice)
        target = document.createElement('div')
        document.body.append(target)
        mounted = mount(AlertComp, { target })
        await tick()
        return { result }
    }
    it('shows the title, description and both labeled actions without a checkbox', async () => {
        const { result } = await ask()
        expect(target.querySelector('#checkbox-confirm-title')?.textContent).toBe(choice.title)
        expect(target.querySelector('#checkbox-confirm-description')?.textContent).toBe(choice.description)
        expect(target.querySelector('input[type="checkbox"]')).toBeNull()
        expect(target.querySelector('dl')).toBeNull()
        expect(action('Keep off').disabled).toBe(false)
        action('Keep off').click()
        await expect(result).resolves.toBe(true)
    })
    it.each(['button', 'Escape', 'overlay'])('answers no when cancelled by %s', async mode => {
        const { result } = await ask()
        if (mode === 'button') action('Only this time').click()
        else if (mode === 'Escape') window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
        else target.querySelector<HTMLButtonElement>('button[aria-label="Only this time"]')!.click()
        await expect(result).resolves.toBe(false)
    })
})
