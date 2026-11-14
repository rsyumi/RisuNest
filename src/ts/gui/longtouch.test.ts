import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { longpress } from './longtouch'
import { touchHoldMs } from './touchHold'

let row: HTMLElement
let button: HTMLButtonElement
let field: HTMLTextAreaElement
let held: ReturnType<typeof vi.fn<(event: MouseEvent) => void>>
let clicked: ReturnType<typeof vi.fn<(event: MouseEvent) => void>>
let actions: { destroy(): void }[]

function pointer(type: string, target: EventTarget, pointerType: string, init: PointerEventInit = {}) {
    const event = new PointerEvent(type, {
        bubbles: true, cancelable: true, pointerId: pointerType === 'mouse' ? 1 : 2, pointerType,
        isPrimary: true, button: 0, clientX: 20, clientY: 20, ...init,
    })
    target.dispatchEvent(event)
    return event
}

function click(target: EventTarget) {
    const event = new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 })
    target.dispatchEvent(event)
    return event
}

function contextmenu(target: EventTarget) {
    const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
    target.dispatchEvent(event)
    return event
}

function selectstart(target: EventTarget) {
    const event = new Event('selectstart', { bubbles: true, cancelable: true })
    target.dispatchEvent(event)
    return event
}

beforeEach(() => {
    vi.useFakeTimers()
    row = document.createElement('div')
    button = document.createElement('button')
    field = document.createElement('textarea')
    row.append(button, field)
    document.body.append(row)
    held = vi.fn()
    clicked = vi.fn()
    // Like Svelte's delegated handlers, the click handler sits above the element.
    row.addEventListener('click', clicked)
    actions = []
})

afterEach(() => {
    for (const action of actions) action.destroy()
    // A test that leaves its finger down would keep the hold's guard on the document.
    pointer('pointercancel', window, 'touch')
    document.body.replaceChildren()
    vi.useRealTimers()
})

it.each(['touch', 'mouse'])('runs a held %s press once with its event and drops the click that ends it', pointerType => {
    actions.push(longpress(button, held))
    pointer('pointerdown', button, pointerType, { shiftKey: true })
    pointer('pointermove', button, pointerType, { clientX: 25, clientY: 25 })
    vi.advanceTimersByTime(touchHoldMs - 1)
    expect(held).not.toHaveBeenCalled()
    vi.advanceTimersByTime(1)
    expect(held).toHaveBeenCalledOnce()
    expect(held.mock.calls[0][0].shiftKey).toBe(true)
    expect(contextmenu(button).defaultPrevented).toBe(true)
    // Only a finger brings the platform's long press with it.
    expect(selectstart(field).defaultPrevented).toBe(pointerType === 'touch')

    pointer('pointerup', button, pointerType)
    expect(click(button).defaultPrevented).toBe(true)
    expect(clicked).not.toHaveBeenCalled()

    pointer('pointerdown', button, pointerType)
    vi.advanceTimersByTime(100)
    pointer('pointerup', button, pointerType)
    click(button)
    expect(clicked).toHaveBeenCalledOnce()
    expect(held).toHaveBeenCalledOnce()
})

it('cancels the touchend of a fired touch hold, also after the action removed the element', () => {
    const touchend = () => {
        const event = new Event('touchend', { bubbles: true, cancelable: true })
        button.dispatchEvent(event)
        return event
    }
    const action = longpress(button, held)
    actions.push(action)
    pointer('pointerdown', button, 'touch')
    vi.advanceTimersByTime(100)
    pointer('pointerup', button, 'touch')
    expect(touchend().defaultPrevented).toBe(false)

    pointer('pointerdown', button, 'touch')
    pointer('pointermove', button, 'touch', { clientX: 40 })
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointerup', button, 'touch', { clientX: 40 })
    expect(touchend().defaultPrevented).toBe(false)

    // A pen fires touch events on phones and tablets only.
    pointer('pointerdown', button, 'pen')
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointerup', button, 'pen')
    expect(held).toHaveBeenCalledOnce()
    expect(touchend().defaultPrevented).toBe(true)
    pointer('pointerdown', button, 'pen')
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointerup', button, 'pen')
    pointer('pointerdown', button, 'touch')
    pointer('pointerup', button, 'touch')
    expect(touchend().defaultPrevented).toBe(false)

    pointer('pointerdown', button, 'touch')
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointerup', button, 'touch')
    expect(touchend().defaultPrevented).toBe(true)
    expect(touchend().defaultPrevented).toBe(false)

    // A hold whose touchend never came does not cancel the next tap.
    pointer('pointerdown', button, 'touch')
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointercancel', button, 'touch')
    pointer('pointerdown', button, 'touch')
    pointer('pointerup', button, 'touch')
    expect(touchend().defaultPrevented).toBe(false)

    held.mockImplementation(() => {
        action.destroy()
        button.remove()
    })
    pointer('pointerdown', button, 'touch')
    vi.advanceTimersByTime(touchHoldMs)
    expect(held).toHaveBeenCalledTimes(5)
    expect(touchend().defaultPrevented).toBe(true)
})

it.each(['pointerup', 'pointercancel', 'pointerdown'])('keeps the platform long press off what a held touch put under the finger until %s', ending => {
    const menu = vi.fn()
    row.addEventListener('contextmenu', menu)
    // The action replaces the pressed button, as a menu item that opens a dialog does.
    const action = longpress(button, () => {
        action.destroy()
        button.remove()
    })
    pointer('pointerdown', button, 'touch')
    expect(selectstart(field).defaultPrevented).toBe(false)
    vi.advanceTimersByTime(touchHoldMs)
    expect(selectstart(field).defaultPrevented).toBe(true)
    expect(contextmenu(field).defaultPrevented).toBe(true)
    expect(menu).not.toHaveBeenCalled()

    // Another pointer's release does not end it. The finger lifting does, and so does the next press when the lift was never seen.
    pointer('pointerup', field, 'mouse')
    expect(selectstart(field).defaultPrevented).toBe(true)
    pointer(ending, field, 'touch')
    expect(selectstart(field).defaultPrevented).toBe(false)
    expect(contextmenu(field).defaultPrevented).toBe(false)
    expect(menu).toHaveBeenCalledOnce()
})

it.each(['move', 'pointercancel', 'release'])('drops the hold on %s and leaves the next click alone', reason => {
    actions.push(longpress(button, held))
    pointer('pointerdown', button, 'touch')
    pointer('pointermove', button, 'touch', { clientX: 25, clientY: 25 })
    if (reason === 'move') pointer('pointermove', button, 'touch', { clientX: 20, clientY: 30 })
    if (reason === 'pointercancel') pointer('pointercancel', button, 'touch')
    if (reason === 'release') pointer('pointerup', button, 'touch')
    vi.advanceTimersByTime(touchHoldMs * 2)
    expect(held).not.toHaveBeenCalled()
    click(button)
    expect(clicked).toHaveBeenCalledOnce()
})

it('leaves a held right mouse button to the context menu', () => {
    actions.push(longpress(button, held))
    pointer('pointerdown', button, 'mouse', { button: 2 })
    vi.advanceTimersByTime(touchHoldMs * 2)
    expect(held).not.toHaveBeenCalled()
    expect(contextmenu(button).defaultPrevented).toBe(false)
})

it('runs on Android\'s own long press without waiting and keeps its menu off', () => {
    actions.push(longpress(button, held))
    pointer('pointerdown', button, 'touch')
    vi.advanceTimersByTime(touchHoldMs / 2)
    expect(contextmenu(button).defaultPrevented).toBe(true)
    expect(held).toHaveBeenCalledOnce()
    vi.advanceTimersByTime(touchHoldMs)
    expect(held).toHaveBeenCalledOnce()
})

it('leaves a touch hold on a text field to text selection unless touch is allowed', () => {
    actions.push(longpress(field, held))
    pointer('pointerdown', field, 'touch')
    vi.advanceTimersByTime(touchHoldMs * 2)
    expect(contextmenu(field).defaultPrevented).toBe(false)
    pointer('pointerup', field, 'touch')
    expect(held).not.toHaveBeenCalled()
    expect(field.style.getPropertyValue('user-select')).toBe('')

    pointer('pointerdown', field, 'mouse')
    pointer('pointermove', field, 'mouse', { clientX: 21 })
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointerup', field, 'mouse')
    expect(held).not.toHaveBeenCalled()

    pointer('pointerdown', field, 'mouse')
    vi.advanceTimersByTime(touchHoldMs)
    pointer('pointerup', field, 'mouse')
    expect(held).toHaveBeenCalledOnce()

    actions.pop()!.destroy()
    actions.push(longpress(field, { onLongPress: held, touch: true }))
    pointer('pointerdown', field, 'touch')
    vi.advanceTimersByTime(touchHoldMs)
    expect(held).toHaveBeenCalledTimes(2)
})

it('cancels a pending hold and restores the element when destroyed', () => {
    button.style.setProperty('user-select', 'text', 'important')
    const action = longpress(button, held)
    expect(button.style.getPropertyValue('user-select')).toBe('none')
    pointer('pointerdown', button, 'touch')
    action.destroy()
    vi.advanceTimersByTime(touchHoldMs * 2)
    expect(held).not.toHaveBeenCalled()
    expect(button.style.getPropertyValue('user-select')).toBe('text')
    expect(button.style.getPropertyPriority('user-select')).toBe('important')
})
