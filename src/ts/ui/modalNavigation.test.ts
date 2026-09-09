import { expect, it, vi } from 'vitest'
import { modalNavigation } from './modalNavigation'
it('dismisses on Escape and restores focus to the opener', async () => {
    const opener = document.createElement('button'),
        modal = document.createElement('div')
    modal.innerHTML = '<button>Close</button>'
    document.body.append(opener, modal)
    opener.focus()
    const back = vi.spyOn(history, 'back').mockImplementation(() => {})
    const close = vi.fn()
    const action = modalNavigation(modal, { close })
    await Promise.resolve()
    expect(document.activeElement).toBe(modal.firstChild)
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    expect(close).toHaveBeenCalledOnce()
    action.destroy()
    expect(document.activeElement).toBe(opener)
    expect(back).toHaveBeenCalledOnce()
    back.mockRestore()
    history.replaceState(null, '')
    document.body.replaceChildren()
})
it('consumes a WebView/browser Back without going back a second time', () => {
    const back = vi.spyOn(history, 'back').mockImplementation(() => {})
    const close = vi.fn(),
        modal = document.createElement('div')
    const action = modalNavigation(modal, { close })
    history.replaceState(null, '')
    window.dispatchEvent(new PopStateEvent('popstate'))
    expect(close).toHaveBeenCalledOnce()
    action.destroy()
    expect(back).not.toHaveBeenCalled()
    back.mockRestore()
})
