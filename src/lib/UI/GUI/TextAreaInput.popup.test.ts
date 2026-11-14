// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

vi.mock('src/ts/gui/guisize', async () => {
    const { writable } = await import('svelte/store')
    return { textAreaSize: writable(0), textAreaTextSize: writable(1) }
})
vi.mock('src/ts/gui/highlight', () => ({
    highlighter: vi.fn(), getNewHighlightId: () => 1, removeHighlight: vi.fn(), AllCBS: [],
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { hotkeys: [], longPressToPopupEditor: false } },
        disableHighlight: writable(false),
    }
})
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
const hotkey = vi.hoisted(() => ({ matches: false }))
vi.mock('src/ts/hotkey', () => ({ hotkeyMatches: () => hotkey.matches }))
vi.mock('src/ts/util', () => ({ sleep: async () => {} }))

import TextAreaInput from './TextAreaInput.svelte'
import TextAreaInputInsidePopupHarness from './TextAreaInputInsidePopupHarness.test.svelte'
import { textEditorPopup } from 'src/ts/gui/textEditorPopup.svelte'
import { DBState } from 'src/ts/stores.svelte'

let component: ReturnType<typeof mount> | undefined

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    textEditorPopup.request = null
    hotkey.matches = false
    DBState.db.longPressToPopupEditor = false
    document.body.replaceChildren()
})

async function input(props: Record<string, unknown>) {
    const target = document.createElement('div')
    document.body.append(target)
    component = mount(TextAreaInput, { target, props: { value: 'Field text', ...props } })
    await tick()
    const expand = target.querySelector<HTMLButtonElement>('button[aria-label="Edit in popup"]')!
    return { target, expand }
}

describe('text area popup editor button', () => {
    it('opens the popup editor with the field text', async () => {
        const { expand } = await input({})
        expect(expand).not.toBeNull()
        expand.click()
        expect(textEditorPopup.request).toMatchObject({ value: 'Field text', language: 'markdown', preview: true })
    })

    it('opens the popup editor in the field language from the popup editor hotkey', async () => {
        const { target } = await input({ popupLanguage: 'lua' })
        hotkey.matches = true
        const event = new KeyboardEvent('keydown', { key: 'x', ctrlKey: true, bubbles: true, cancelable: true })
        target.querySelector('textarea')!.dispatchEvent(event)
        expect(event.defaultPrevented).toBe(true)
        expect(textEditorPopup.request).toMatchObject({ value: 'Field text', language: 'lua', preview: false })
    })

    it('opens the popup editor on long press only when that option is on', async () => {
        const { target } = await input({})
        const field = target.querySelector('textarea')!
        const ignored = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
        field.dispatchEvent(ignored)
        expect(ignored.defaultPrevented).toBe(false)
        expect(textEditorPopup.request).toBeNull()

        DBState.db.longPressToPopupEditor = true
        const pressed = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
        field.dispatchEvent(pressed)
        expect(pressed.defaultPrevented).toBe(true)
        expect(textEditorPopup.request?.value).toBe('Field text')
    })

    it('opens the popup editor once from a touch hold only when that option is on', async () => {
        vi.useFakeTimers()
        const touch = (field: HTMLTextAreaElement, type: string) => {
            const point = { identifier: 1, clientX: 20, clientY: 20, target: field }
            const event = new Event(type, { bubbles: true, cancelable: true })
            Object.defineProperties(event, {
                touches: { value: type === 'touchend' ? [] : [point] },
                changedTouches: { value: [point] },
            })
            field.dispatchEvent(event)
            if (type === 'touchstart') vi.advanceTimersByTime(500)
            return event
        }
        const menus = vi.fn()
        document.addEventListener('contextmenu', menus)
        try {
            DBState.db.longPressToPopupEditor = undefined
            const off = (await input({})).target.querySelector('textarea')!
            touch(off, 'touchstart')
            expect(touch(off, 'touchend').defaultPrevented).toBe(false)
            expect(menus).not.toHaveBeenCalled()
            expect(textEditorPopup.request).toBeNull()
            await unmount(component!)
            component = undefined

            // iOS raises no contextmenu for a long press.
            DBState.db.longPressToPopupEditor = true
            const field = (await input({})).target.querySelector('textarea')!
            touch(field, 'touchstart')
            expect(touch(field, 'touchend').defaultPrevented).toBe(true)
            expect(textEditorPopup.request?.value).toBe('Field text')

            // Android raises its own during the hold.
            textEditorPopup.request = null
            touch(field, 'touchstart')
            field.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }))
            const opened = textEditorPopup.request
            expect(opened?.value).toBe('Field text')
            touch(field, 'touchend')
            expect(textEditorPopup.request).toBe(opened)
        } finally {
            document.removeEventListener('contextmenu', menus)
            vi.useRealTimers()
        }
    })

    it('does not open another popup editor from a field inside the popup editor', async () => {
        const target = document.createElement('div')
        document.body.append(target)
        component = mount(TextAreaInputInsidePopupHarness, { target })
        await tick()
        expect(target.querySelector('button[aria-label="Edit in popup"]')).toBeNull()

        hotkey.matches = true
        DBState.db.longPressToPopupEditor = true
        const field = target.querySelector('textarea')!
        const key = new KeyboardEvent('keydown', { key: 'x', ctrlKey: true, bubbles: true, cancelable: true })
        const press = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
        field.dispatchEvent(key)
        field.dispatchEvent(press)
        expect(key.defaultPrevented).toBe(false)
        expect(press.defaultPrevented).toBe(false)
        expect(textEditorPopup.request).toBeNull()
    })

    it.each([false, true])('writes the saved text back and reports it to change listeners (highlight %s)', async (highlight) => {
        const onInput = vi.fn()
        const changes: string[] = []
        // Like the toggle text areas, the value is not bound and persists only from the change event.
        const onchange = (event: { currentTarget: HTMLTextAreaElement | HTMLDivElement }) => {
            changes.push(event.currentTarget instanceof HTMLDivElement
                ? event.currentTarget.textContent ?? ''
                : event.currentTarget.value)
        }
        const { target, expand } = await input({ highlight, onInput, onchange })
        expand.click()

        await expect(textEditorPopup.request!.save('Saved text')).resolves.toBe(true)
        expect(onInput).toHaveBeenCalled()
        expect(changes).toEqual(['Saved text'])
        const field = target.querySelector<HTMLElement>(highlight ? '[contenteditable]' : 'textarea')!
        expect(highlight ? field.textContent : (field as HTMLTextAreaElement).value).toBe('Saved text')
    })

    it('closes its popup editor without saving when the field goes away', async () => {
        const onchange = vi.fn()
        const { expand } = await input({ onchange })
        expand.click()
        // The editor renders before anything can remove the field.
        await tick()
        const request = textEditorPopup.request!
        const cancel = vi.spyOn(request, 'cancel')

        await unmount(component!)
        component = undefined
        expect(textEditorPopup.request).toBeNull()
        expect(cancel).toHaveBeenCalledOnce()
        expect(onchange).not.toHaveBeenCalled()
    })

    it('leaves another field popup editor open when it goes away', async () => {
        const { expand } = await input({})
        expand.click()
        await expect(textEditorPopup.request!.save('Saved text')).resolves.toBe(true)
        const other = { value: 'other', save: () => true }
        textEditorPopup.request = other

        await unmount(component!)
        component = undefined
        expect(textEditorPopup.request).toBe(other)
    })
})
