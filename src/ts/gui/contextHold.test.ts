import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { contextHold } from './contextHold'
import { touchHoldMs } from './touchHold'

let node: HTMLElement
let child: HTMLElement
let menus: MouseEvent[]
let handleMenu: boolean
let clicked: ReturnType<typeof vi.fn<(event: MouseEvent) => void>>
let action: ReturnType<typeof contextHold>

function touch(type: string, x = 30, y = 60, count = 1) {
    const point = { identifier: 3, clientX: x, clientY: y, target: child }
    const event = new Event(type, { bubbles: true, cancelable: true })
    Object.defineProperties(event, {
        touches: { value: type === 'touchend' || type === 'touchcancel' ? [] : Array.from({ length: count }, (_, i) => ({ ...point, identifier: 3 + i })) },
        changedTouches: { value: [point] },
    })
    child.dispatchEvent(event)
    return event
}

function click() {
    const event = new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 })
    child.dispatchEvent(event)
    return event
}

beforeEach(() => {
    vi.useFakeTimers()
    node = document.createElement('div')
    child = document.createElement('span')
    node.append(child)
    document.body.append(node)
    menus = []
    handleMenu = true
    clicked = vi.fn()
    node.addEventListener('contextmenu', event => {
        menus.push(event)
        if (handleMenu) event.preventDefault()
    })
    document.body.addEventListener('click', clicked)
    action = contextHold(node)
})

afterEach(() => {
    action.destroy()
    document.body.removeEventListener('click', clicked)
    document.body.replaceChildren()
    vi.useRealTimers()
})

it('raises one menu at the release of a stationary hold and drops the click that follows', () => {
    touch('touchstart')
    vi.advanceTimersByTime(touchHoldMs - 1)
    touch('touchmove', 34, 64)
    expect(menus).toHaveLength(0)
    vi.advanceTimersByTime(1)
    expect(touch('touchend', 34, 64).defaultPrevented).toBe(true)
    expect(menus).toHaveLength(1)
    expect(menus[0]).toMatchObject({ target: child, clientX: 34, clientY: 64, bubbles: true, cancelable: true })
    expect(click().defaultPrevented).toBe(true)
    expect(clicked).not.toHaveBeenCalled()

    touch('touchstart')
    vi.advanceTimersByTime(100)
    expect(touch('touchend').defaultPrevented).toBe(false)
    click()
    expect(menus).toHaveLength(1)
    expect(clicked).toHaveBeenCalledOnce()
})

it('does not add a menu to the one Android raised during the hold', () => {
    touch('touchstart')
    vi.advanceTimersByTime(touchHoldMs + 50)
    child.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }))
    expect(touch('touchend').defaultPrevented).toBe(true)
    expect(menus).toHaveLength(1)
    click()
    expect(clicked).not.toHaveBeenCalled()
})

it.each(['move', 'release away', 'touchcancel', 'second finger'])('raises nothing after a hold that ended by %s', reason => {
    touch('touchstart')
    vi.advanceTimersByTime(touchHoldMs)
    if (reason === 'move') touch('touchmove', 30, 70)
    if (reason === 'touchcancel') touch('touchcancel')
    if (reason === 'second finger') {
        touch('touchstart', 30, 60, 2)
        vi.advanceTimersByTime(touchHoldMs)
    }
    expect(touch('touchend', 30, reason === 'release away' ? 70 : 60).defaultPrevented).toBe(false)
    expect(menus).toHaveLength(0)
    click()
    expect(clicked).toHaveBeenCalledOnce()
})

it('leaves the touch to the browser when nothing handles the menu', () => {
    handleMenu = false
    touch('touchstart')
    vi.advanceTimersByTime(touchHoldMs)
    expect(touch('touchend').defaultPrevented).toBe(false)
    expect(menus).toHaveLength(1)
    click()
    expect(clicked).toHaveBeenCalledOnce()
})

it('stays out of the way while disabled and after it is destroyed', () => {
    action.destroy()
    node.style.setProperty('user-select', 'text', 'important')
    action = contextHold(node, false)
    expect(node.style.getPropertyValue('user-select')).toBe('text')
    touch('touchstart')
    vi.advanceTimersByTime(touchHoldMs)
    touch('touchend')
    expect(menus).toHaveLength(0)

    action.update(true)
    expect(node.style.getPropertyValue('user-select')).toBe('none')
    action.destroy()
    expect(node.style.getPropertyValue('user-select')).toBe('text')
    expect(node.style.getPropertyPriority('user-select')).toBe('important')
    touch('touchstart')
    vi.advanceTimersByTime(touchHoldMs)
    touch('touchend')
    expect(menus).toHaveLength(0)
})
