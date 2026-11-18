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
