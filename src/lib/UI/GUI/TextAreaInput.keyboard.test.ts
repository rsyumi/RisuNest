import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    os: 'macos',
    matches: vi.fn(() => true),
}))
vi.mock('@tauri-apps/plugin-os', () => ({ platform: () => state.os }))
vi.mock('src/ts/hotkey', () => ({ hotkeyMatches: state.matches }))
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
        DBState: { db: { hotkeys: [{ action: 'popupEditor', key: '.', ctrl: true }] } },
        disableHighlight: writable(false),
        settingsOpen: writable(false), MobileGUI: writable(false), MobileGUIStack: writable(0),
        MobileSideBar: writable(0), selectedCharID: writable(0),
    }
})
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/util', () => ({ sleep: async () => {} }))

import TextAreaInput, { textareaRange } from './TextAreaInput.svelte'
import { textEditorPopup } from 'src/ts/gui/textEditorPopup.svelte'
let component: ReturnType<typeof mount> | undefined

beforeEach(() => {
    state.os = 'macos'
    state.matches.mockClear()
    Object.defineProperty(globalThis, '__TAURI_INTERNALS__', { configurable: true, value: {} })
})
afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    textEditorPopup.request = null
    document.body.replaceChildren()
    Reflect.deleteProperty(globalThis, '__TAURI_INTERNALS__')
})

async function input(highlight = false, value = 'preserve text'): Promise<HTMLElement> {
    const target = document.createElement('div')
    document.body.append(target)
    component = mount(TextAreaInput, { target, props: { value, highlight } })
    await tick()
    return target.querySelector<HTMLElement>(highlight ? '[contenteditable]' : 'textarea')!
}

describe('popup editor keyboard entry', () => {
    it('passes a native Mac Command-only shortcut through the component guard', async () => {
        const target = await input()
        const event = new KeyboardEvent('keydown', { key: '.', metaKey: true, bubbles: true, cancelable: true })
        target.dispatchEvent(event)
        expect(state.matches).toHaveBeenCalledOnce()
        expect(event.defaultPrevented).toBe(true)
        expect(textEditorPopup.request?.value).toBe('preserve text')
    })

    it('does not reinterpret the Windows meta key as the configured Ctrl shortcut', async () => {
        state.os = 'windows'
        const target = await input()
        const event = new KeyboardEvent('keydown', { key: '.', metaKey: true, bubbles: true, cancelable: true })
        target.dispatchEvent(event)
        expect(state.matches).not.toHaveBeenCalled()
        expect(event.defaultPrevented).toBe(false)
        expect(textEditorPopup.request).toBeNull()
    })

    it.each([false, true])('leaves IME Enter to the input method (highlight %s)', async (highlight) => {
        const target = await input(highlight)
        for (const flags of [{ isComposing: true }, { keyCode: 229 }]) {
            const event = new KeyboardEvent('keydown', {
                key: 'Enter', metaKey: true, bubbles: true, cancelable: true, ...flags,
            })
            target.dispatchEvent(event)
            expect(event.defaultPrevented).toBe(false)
        }
        expect(state.matches).not.toHaveBeenCalled()
        expect(highlight ? target.textContent : (target as HTMLTextAreaElement).value).toBe('preserve text')
    })
})

describe('selectRange', () => {
    // The instance function a parent reaches through bind:this.
    const selectRange = (range: { start: number, end: number } | null): boolean => component!.selectRange(range)

    it('selects the range in the plain textarea, or only focuses it when there is no range that fits', async () => {
        const textarea = await input() as HTMLTextAreaElement
        expect(selectRange({ start: 9, end: 13 })).toBe(true)
        expect(document.activeElement).toBe(textarea)
        expect([textarea.selectionStart, textarea.selectionEnd]).toEqual([9, 13])
        textarea.blur()
        expect(selectRange(null)).toBe(false)
        expect(document.activeElement).toBe(textarea)
        expect(selectRange({ start: 3, end: 99 })).toBe(false)
    })

    it('selects the range in text stored with Windows line breaks, which the textarea gives as LF', async () => {
        const textarea = await input(false, 'one\r\ntwo\r\nthree') as HTMLTextAreaElement
        // Browsers give a textarea's value with LF line breaks, whatever was stored.
        Object.defineProperty(textarea, 'value', { configurable: true, get: () => 'one\ntwo\nthree' })
        expect(selectRange({ start: 5, end: 8 })).toBe(true)
        expect([textarea.selectionStart, textarea.selectionEnd]).toEqual([4, 7])
    })

    it('moves a range of stored text onto the value a textarea gives', () => {
        // a CR LF b CR LF c: pairs at 1 and 4.
        const text = 'a\r\nb\r\nc'
        const moved = (start: number, end: number) => {
            const { value, start: from, end: to } = textareaRange(text, { start, end })
            return value.slice(from, to)
        }
        expect(textareaRange(text, { start: 0, end: 7 })).toEqual({ value: 'a\nb\nc', start: 0, end: 5 })
        expect(moved(0, 1)).toBe('a')
        expect(moved(3, 4)).toBe('b')
        expect(moved(6, 7)).toBe('c')
        // A range that starts or ends on a pair, or between its CR and LF.
        expect(moved(1, 4)).toBe('\nb')
        expect(moved(2, 4)).toBe('\nb')
        expect(moved(3, 5)).toBe('b')
        expect(moved(3, 6)).toBe('b\n')
        // A lone CR is one character either way.
        expect(textareaRange('a\rb\r\nc', { start: 2, end: 6 })).toEqual({ value: 'a\nb\nc', start: 2, end: 5 })
    })

    it('selects the range in the highlighted editor, across its text nodes', async () => {
        const editor = await input(true)
        expect(selectRange({ start: 9, end: 13 })).toBe(true)
        expect(document.activeElement).toBe(editor)
        expect(window.getSelection()!.toString()).toBe('text')
        editor.replaceChildren(document.createTextNode('preserve '), document.createTextNode('text'))
        expect(selectRange({ start: 6, end: 11 })).toBe(true)
        expect(window.getSelection()!.toString()).toBe('ve te')
        expect(window.getSelection()!.getRangeAt(0).startContainer).toBe(editor.childNodes[0])
        expect(window.getSelection()!.getRangeAt(0).endContainer).toBe(editor.childNodes[1])
        expect(selectRange({ start: 10, end: 12 })).toBe(true)
        expect(window.getSelection()!.toString()).toBe('ex')
    })

    it('only focuses the highlighted editor while its text differs from the value', async () => {
        const editor = await input(true)
        editor.replaceChildren(document.createTextNode('changed text'))
        window.getSelection()!.removeAllRanges()
        expect(selectRange({ start: 0, end: 5 })).toBe(false)
        expect(document.activeElement).toBe(editor)
        expect(window.getSelection()!.toString()).toBe('')
    })
})
