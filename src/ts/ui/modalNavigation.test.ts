import { afterEach, expect, it, vi } from 'vitest'
import { modalNavigation } from './modalNavigation'

afterEach(() => {
    vi.restoreAllMocks()
    history.replaceState(null, '')
    document.body.replaceChildren()
})
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

it('dismisses only the top nested dialog on Escape and preserves ancestors on Back', () => {
    const nodes = Array.from({ length: 3 }, () => document.createElement('div'))
    const closes = nodes.map(() => vi.fn())
    const states: unknown[] = []
    const actions = nodes.map((node, index) => {
        const action = modalNavigation(node, { close: closes[index] })
        states.push(history.state)
        return action
    })
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    expect(closes[0]).not.toHaveBeenCalled()
    expect(closes[1]).not.toHaveBeenCalled()
    expect(closes[2]).toHaveBeenCalledOnce()

    history.replaceState(states[1], '')
    window.dispatchEvent(new PopStateEvent('popstate'))
    actions[2].destroy()
    expect(closes[0]).not.toHaveBeenCalled()
    expect(closes[1]).not.toHaveBeenCalled()

    history.replaceState(states[0], '')
    window.dispatchEvent(new PopStateEvent('popstate'))
    actions[1].destroy()
    expect(closes[0]).not.toHaveBeenCalled()
    expect(closes[1]).toHaveBeenCalledOnce()

    history.replaceState(null, '')
    window.dispatchEvent(new PopStateEvent('popstate'))
    actions[0].destroy()
    expect(closes[0]).toHaveBeenCalledOnce()
})

it('does not close a dialog for composition-owned Escape', () => {
    const close = vi.fn()
    const action = modalNavigation(document.createElement('div'), { close })
    for (const flags of [{ isComposing: true }, { keyCode: 229 }]) {
        const event = new KeyboardEvent('keydown', { key: 'Escape', cancelable: true, ...flags })
        window.dispatchEvent(event)
        expect(event.defaultPrevented).toBe(false)
    }
    expect(close).not.toHaveBeenCalled()
    history.replaceState(null, '')
    action.destroy()
})
