import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const settings = vi.hoisted(() => ({ allowAllExtentionFiles: false, ios: false }))
vi.mock('./storage/database.svelte', () => ({ getDatabase: () => settings }))
vi.mock('./characters', () => ({ createBlankChar: vi.fn(), getCharImage: vi.fn() }))
vi.mock('./stores.svelte', () => ({ DBState: { db: {} }, selectedCharID: { subscribe: vi.fn() } }))
vi.mock('./platform', () => ({ isTauri: false, isTauriMobile: false, isIOS: () => settings.ios }))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))

import { selectFileByDom, selectSingleFile } from './util'

function input(): HTMLInputElement {
    const element = document.querySelector<HTMLInputElement>('input[type="file"]')
    if (!element) throw new Error('Picker input was not mounted')
    return element
}

beforeEach(() => {
    vi.spyOn(HTMLInputElement.prototype, 'click').mockImplementation(() => {})
    settings.allowAllExtentionFiles = false
    settings.ios = false
})
afterEach(() => {
    vi.restoreAllMocks()
    document.body.replaceChildren()
})

describe('DOM file picker', () => {
    it('settles cancellation and removes the input and its event listeners', async () => {
        const pending = selectFileByDom(['png'])
        const element = input()
        const removed = vi.spyOn(element, 'removeEventListener')
        element.dispatchEvent(new Event('cancel'))
        expect(await pending).toEqual([])
        expect(element.isConnected).toBe(false)
        expect(removed).toHaveBeenCalledWith('change', expect.any(Function))
        expect(removed).toHaveBeenCalledWith('cancel', expect.any(Function))
    })

    it('settles an empty change event without retaining an input', async () => {
        const pending = selectFileByDom(['png'])
        input().dispatchEvent(new Event('change'))
        expect(await pending).toEqual([])
        expect(document.querySelector('input')).toBeNull()
    })

    it('returns the selected files, retaining extension filtering and multiple selection', async () => {
        const pending = selectFileByDom(['png'], 'multiple')
        const element = input()
        const accepted = new File(['synthetic'], 'CARD.PNG')
        Object.defineProperty(element, 'files', { value: [accepted, new File(['other'], 'other.txt')] })
        expect(element.multiple).toBe(true)
        expect(element.accept).toBe('.png')
        element.dispatchEvent(new Event('change'))
        expect(await pending).toEqual([accepted])
        expect(element.isConnected).toBe(false)
    })

    it.each(['setting', 'ios', 'wildcard'] as const)('keeps the %s accept-all path', async (mode) => {
        settings.allowAllExtentionFiles = mode === 'setting'
        settings.ios = mode === 'ios'
        const pending = selectFileByDom(mode === 'wildcard' ? ['*'] : ['png'])
        const element = input()
        const file = new File(['synthetic'], 'file.without-known-extension')
        Object.defineProperty(element, 'files', { value: [file] })
        element.dispatchEvent(new Event('change'))
        expect(await pending).toEqual([file])
    })

    it('returns null from the single-file wrapper after cancellation', async () => {
        const pending = selectSingleFile(['png'])
        input().dispatchEvent(new Event('cancel'))
        expect(await pending).toBeNull()
    })

    it('rejects an opening failure without leaking the input', async () => {
        vi.mocked(HTMLInputElement.prototype.click).mockImplementation(() => { throw new Error('opening failed') })
        await expect(selectFileByDom(['png'])).rejects.toThrow('opening failed')
        expect(document.querySelector('input')).toBeNull()
    })
})
