// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'

const platform = vi.hoisted(() => ({ isMobile: true }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule(() => ({ runtime: 'web', os: platform.isMobile ? 'android' : 'windows' })))
vi.mock('src/ts/stores.svelte', async () => {
    const { createAlertQueue } = await import('src/ts/alertQueue')
    return { alertStore: createAlertQueue({ type: 'none', msg: '' }, { gapMs: 0 }) }
})
vi.mock('./TextEditorMonaco.svelte', async () => ({ default: (await import('./TextEditorMonacoStub.test.svelte')).default }))
vi.mock('./TextEditorPreview.svelte', async () => ({ default: (await import('./TextEditorPreviewStub.test.svelte')).default }))

import TextEditorPopup from './TextEditorPopup.svelte'
import { modalNavigation } from 'src/ts/ui/modalNavigation'
import { alertStore } from 'src/ts/stores.svelte'
import {
    openTextEditorPopup,
    textEditorPopup,
    type TextEditorPopupRequest,
} from 'src/ts/gui/textEditorPopup.svelte'

let component: ReturnType<typeof mount> | undefined

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    textEditorPopup.request = null
    alertStore.set({ type: 'none', msg: '' })
    platform.isMobile = true
    document.body.replaceChildren()
})

async function open(overrides: Partial<TextEditorPopupRequest> = {}) {
    const request: TextEditorPopupRequest = {
        value: 'Original text',
        save: vi.fn(() => true),
        cancel: vi.fn(),
        ...overrides,
    }
    openTextEditorPopup(request)
    const target = document.createElement('div')
    document.body.append(target)
    component = mount(TextEditorPopup, { target, props: { request } })
    await tick()
    const textarea = target.querySelector('textarea')!
    const buttons = [...target.querySelectorAll('button')]
    return {
        request,
        target,
        textarea,
        close: target.querySelector<HTMLButtonElement>('button[aria-label="Close"]')!,
        cancel: buttons.find((button) => button.textContent?.trim() === 'Cancel')!,
        save: buttons.find((button) => button.textContent?.trim() === 'Save')!,
        button: (label: string) => [...target.querySelectorAll('button')].find((button) => button.textContent?.trim() === label),
    }
}

async function openOnDesktop(overrides: Partial<TextEditorPopupRequest> = {}) {
    platform.isMobile = false
    const opened = await open(overrides)
    const editor = await vi.waitFor(() => {
        const element = opened.target.querySelector<HTMLElement>('[data-monaco-stub]')
        expect(element).not.toBeNull()
        return element!
    })
    return { ...opened, editor, editorInput: editor.querySelector('textarea')! }
}

const escape = () => new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })

function type(textarea: HTMLTextAreaElement, value: string) {
    textarea.value = value
    textarea.dispatchEvent(new Event('input', { bubbles: true }))
}

describe('text editor popup', () => {
    it('shows the text in a titled plain editor on touch devices without opening the keyboard', async () => {
        const { target, textarea, close } = await open({ title: 'Description' })
        expect(target.querySelector('[role="dialog"]')?.getAttribute('aria-modal')).toBe('true')
        expect(target.querySelector('h2')?.textContent).toBe('Description')
        expect(textarea.value).toBe('Original text')
        await vi.waitFor(() => expect(document.activeElement).toBe(close))
        await vi.dynamicImportSettled()
        await tick()
        expect(target.querySelector('[data-monaco-stub]')).toBeNull()
    })

    it('has the code editor once its import settles on other devices', async () => {
        platform.isMobile = false
        const { target } = await open()
        await vi.dynamicImportSettled()
        await tick()
        expect(target.querySelector('[data-monaco-stub]')).not.toBeNull()
    })

    it('saves the edited text and closes', async () => {
        const { request, textarea, save } = await open()
        type(textarea, 'Edited text')
        save.click()
        await vi.waitFor(() => expect(textEditorPopup.request).toBeNull())
        expect(request.save).toHaveBeenCalledWith('Edited text')
        expect(request.cancel).not.toHaveBeenCalled()
    })

    it('reports the draft as it is typed', async () => {
        const input = vi.fn()
        const { textarea } = await open({ input })
        type(textarea, 'Typed draft')
        expect(input).toHaveBeenLastCalledWith('Typed draft')
    })

    it('stays open with the draft when the save is refused', async () => {
        const { request, textarea, save } = await open({ save: vi.fn(async () => false) })
        type(textarea, 'Refused text')
        save.click()
        await vi.waitFor(() => expect(request.save).toHaveBeenCalledWith('Refused text'))
        await tick()
        expect(textEditorPopup.request).toBe(request)
        expect(textarea.value).toBe('Refused text')
        expect(save.disabled).toBe(false)
    })

    it.each(['close', 'cancel'] as const)('discards the draft from the %s button', async (button) => {
        const opened = await open()
        type(opened.textarea, 'Discarded text')
        opened[button].click()
        expect(textEditorPopup.request).toBeNull()
        expect(opened.request.cancel).toHaveBeenCalledOnce()
        expect(opened.request.save).not.toHaveBeenCalled()
    })

    it('saves on Ctrl+Enter and keeps typing keys from the app hotkeys', async () => {
        const hotkeys = vi.fn()
        document.addEventListener('keydown', hotkeys)
        try {
            const { request, textarea } = await open()
            type(textarea, 'Shortcut text')
            const typing = new KeyboardEvent('keydown', { key: '[', ctrlKey: true, bubbles: true, cancelable: true })
            textarea.dispatchEvent(typing)
            textarea.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true, cancelable: true }))
            await vi.waitFor(() => expect(request.save).toHaveBeenCalledWith('Shortcut text'))
            expect(hotkeys).not.toHaveBeenCalled()
        } finally {
            document.removeEventListener('keydown', hotkeys)
        }
    })

    it('discards the draft on Escape and leaves the modal underneath open', async () => {
        const host = document.createElement('div')
        document.body.append(host)
        const closeHost = vi.fn()
        // Settings and other RisuNest modals close on Escape through the same helper.
        const hostNavigation = modalNavigation(host, { close: closeHost })
        try {
            const { request, textarea } = await open()
            type(textarea, 'Discarded text')
            const escape = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })
            textarea.dispatchEvent(escape)
            expect(escape.defaultPrevented).toBe(true)
            expect(request.cancel).toHaveBeenCalledOnce()
            expect(request.save).not.toHaveBeenCalled()
            expect(textEditorPopup.request).toBeNull()
            expect(closeHost).not.toHaveBeenCalled()
        } finally {
            hostNavigation.destroy()
        }
    })

    it('closes an alert shown over it on Escape before closing itself', async () => {
        const { request, textarea } = await open()
        const notice = alertStore.open({ type: 'normal', msg: 'Synthetic notice' })
        textarea.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
        expect(get(alertStore).type).toBe('none')
        await expect(notice).resolves.toBe('')
        expect(request.cancel).not.toHaveBeenCalled()
        expect(textEditorPopup.request).toBe(request)

        textarea.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
        expect(request.cancel).toHaveBeenCalledOnce()
        expect(textEditorPopup.request).toBeNull()
    })

    it('leaves IME Enter and Escape to the input method', async () => {
        const { request, textarea } = await open()
        for (const key of ['Enter', 'Escape']) {
            const event = new KeyboardEvent('keydown', { key, ctrlKey: true, isComposing: true, bubbles: true, cancelable: true })
            textarea.dispatchEvent(event)
            expect(event.defaultPrevented).toBe(false)
        }
        expect(request.save).not.toHaveBeenCalled()
        expect(request.cancel).not.toHaveBeenCalled()
    })

    it('cancels the previous request when another editor opens', () => {
        const first: TextEditorPopupRequest = { value: 'a', save: vi.fn(() => true), cancel: vi.fn() }
        const second: TextEditorPopupRequest = { value: 'b', save: vi.fn(() => true), cancel: vi.fn() }
        openTextEditorPopup(first)
        openTextEditorPopup(second)
        expect(first.cancel).toHaveBeenCalledOnce()
        expect(second.cancel).not.toHaveBeenCalled()
        expect(textEditorPopup.request).toBe(second)
    })
})

describe('text editor popup on desktop', () => {
    it('edits in the code editor with the requested language and reports the draft', async () => {
        const input = vi.fn()
        const { target, editor, editorInput, save, request } = await openOnDesktop({ language: 'lua', input })
        expect(editor.dataset.language).toBe('lua')
        expect(target.querySelectorAll('textarea')).toHaveLength(1)
        expect(editorInput.value).toBe('Original text')
        type(editorInput, 'Edited in code')
        expect(input).toHaveBeenLastCalledWith('Edited in code')
        save.click()
        await vi.waitFor(() => expect(request.save).toHaveBeenCalledWith('Edited in code'))
    })

    it('treats the text as markdown when no language is given', async () => {
        const { editor } = await openOnDesktop()
        expect(editor.dataset.language).toBe('markdown')
    })

    it('saves once on Ctrl+Enter in the code editor and keeps it from the app hotkeys', async () => {
        const hotkeys = vi.fn()
        document.addEventListener('keydown', hotkeys)
        try {
            const { request, editorInput } = await openOnDesktop()
            type(editorInput, 'Shortcut text')
            editorInput.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true, cancelable: true }))
            await vi.waitFor(() => expect(request.save).toHaveBeenCalledWith('Shortcut text'))
            await tick()
            expect(request.save).toHaveBeenCalledOnce()
            expect(hotkeys).not.toHaveBeenCalled()
        } finally {
            document.removeEventListener('keydown', hotkeys)
        }
    })

    it('lets the code editor close its own widget on Escape before closing the popup', async () => {
        const host = document.createElement('div')
        document.body.append(host)
        const closeHost = vi.fn()
        const hostNavigation = modalNavigation(host, { close: closeHost })
        try {
            const { request, editor, editorInput } = await openOnDesktop()
            editor.dataset.widget = 'open'
            const first = escape()
            editorInput.dispatchEvent(first)
            expect(first.defaultPrevented).toBe(true)
            expect(editor.dataset.widget).toBe('closed')
            expect(textEditorPopup.request).toBe(request)

            const second = escape()
            editorInput.dispatchEvent(second)
            expect(second.defaultPrevented).toBe(true)
            expect(request.cancel).toHaveBeenCalledOnce()
            expect(textEditorPopup.request).toBeNull()
            expect(closeHost).not.toHaveBeenCalled()
        } finally {
            hostNavigation.destroy()
        }
    })

    it('closes an alert shown over it before the code editor sees Escape', async () => {
        const { request, editor, editorInput } = await openOnDesktop()
        editor.dataset.widget = 'open'
        const notice = alertStore.open({ type: 'normal', msg: 'Synthetic notice' })
        editorInput.dispatchEvent(escape())
        expect(get(alertStore).type).toBe('none')
        await expect(notice).resolves.toBe('')
        expect(editor.dataset.widget).toBe('open')
        expect(request.cancel).not.toHaveBeenCalled()
        expect(textEditorPopup.request).toBe(request)
    })
})

describe('text editor popup preview', () => {
    it('offers the preview only when the request asks for it', async () => {
        const { button } = await openOnDesktop()
        expect(button('Preview')).toBeUndefined()
    })

    it('previews the draft and returns to the editor', async () => {
        const { target, editor, editorInput, button } = await openOnDesktop({ preview: true, presetEditor: true })
        type(editorInput, 'Previewed draft')
        button('Preview')!.click()
        const preview = await vi.waitFor(() => {
            const element = target.querySelector<HTMLElement>('[data-preview-stub]')
            expect(element).not.toBeNull()
            return element!
        })
        expect(preview.textContent).toBe('Previewed draft')
        expect(preview.dataset.presetEditor).toBe('true')
        expect(editor.closest('.invisible')).not.toBeNull()

        button('Edit')!.click()
        await vi.waitFor(() => expect(target.querySelector('[data-preview-stub]')).toBeNull())
        expect(editor.closest('.invisible')).toBeNull()
        await vi.waitFor(() => expect(document.activeElement).toBe(editorInput))
    })

    it('closes from Escape while previewing', async () => {
        const { request, target, button } = await openOnDesktop({ preview: true })
        const toggle = button('Preview')!
        toggle.click()
        await vi.waitFor(() => expect(target.querySelector('[data-preview-stub]')).not.toBeNull())
        toggle.dispatchEvent(escape())
        expect(request.cancel).toHaveBeenCalledOnce()
        expect(textEditorPopup.request).toBeNull()
    })
})
