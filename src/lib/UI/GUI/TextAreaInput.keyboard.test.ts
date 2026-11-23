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

import TextAreaInput from './TextAreaInput.svelte'
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

async function input(highlight = false): Promise<HTMLElement> {
    const target = document.createElement('div')
    document.body.append(target)
    component = mount(TextAreaInput, { target, props: { value: 'preserve text', highlight } })
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
