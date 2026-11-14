// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

// handleDualAction needs none of the chat modules utils.ts loads for its other helpers.
vi.mock('src/ts/process/scripts', () => ({}))
vi.mock('src/ts/storage/database.svelte', () => ({}))
vi.mock('src/ts/stores.svelte', () => ({}))
vi.mock('src/ts/chatMessageUi', () => ({}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireCompleteConversation: vi.fn(),
    captureSelectedConversationTarget: vi.fn(),
    getPersistentDataRuntime: vi.fn(),
    peekActiveConversationSession: vi.fn(),
}))

import { handleDualAction } from './utils'

let button: HTMLButtonElement
let main: ReturnType<typeof vi.fn<() => void>>
let alternative: ReturnType<typeof vi.fn<() => void>>
let action: ReturnType<typeof handleDualAction>

function touch(type: string, x = 10, y = 10) {
    const point = { identifier: 4, clientX: x, clientY: y, target: button }
    const event = new Event(type, { bubbles: true, cancelable: true })
    Object.defineProperties(event, {
        touches: { value: type === 'touchend' || type === 'touchcancel' ? [] : [point] },
        changedTouches: { value: [point] },
    })
    button.dispatchEvent(event)
    return event
}

function tap() {
    touch('touchstart')
    const end = touch('touchend')
    // Touch screens follow a tap with a click.
    button.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 }))
    return end
}

beforeEach(() => {
    vi.useFakeTimers()
    // A computer with a touch screen.
    Object.defineProperty(window, 'ontouchend', { configurable: true, value: null })
    button = document.createElement('button')
    document.body.append(button)
    main = vi.fn()
    alternative = vi.fn()
    action = handleDualAction(button, { onMainAction: main, onAlternativeAction: alternative })
})

afterEach(() => {
    action.destroy()
    Reflect.deleteProperty(window, 'ontouchend')
    document.body.replaceChildren()
    vi.useRealTimers()
})

describe('HypaV3 dual action buttons', () => {
    it('runs the main action on a mouse click and the alternative on a Shift click', () => {
        button.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 }))
        expect(main).toHaveBeenCalledOnce()
        button.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1, shiftKey: true }))
        expect(alternative).toHaveBeenCalledOnce()
        expect(main).toHaveBeenCalledOnce()
    })

    it('runs the main action once for a tap after the double-tap wait', () => {
        tap()
        vi.advanceTimersByTime(299)
        expect(main).not.toHaveBeenCalled()
        vi.advanceTimersByTime(1)
        expect(main).toHaveBeenCalledOnce()
        expect(alternative).not.toHaveBeenCalled()
    })

    it('runs only the alternative action for a double tap', () => {
        tap()
        vi.advanceTimersByTime(100)
        expect(tap().defaultPrevented).toBe(true)
        vi.advanceTimersByTime(1000)
        expect(alternative).toHaveBeenCalledOnce()
        expect(main).not.toHaveBeenCalled()
    })

    it.each(['away', 'and back'])('runs nothing for a touch that scrolled %s, even when a click follows', shape => {
        touch('touchstart')
        if (shape === 'and back') touch('touchmove', 10, 40)
        touch('touchend', 10, shape === 'away' ? 40 : 12)
        button.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 }))
        vi.advanceTimersByTime(1000)
        expect(main).not.toHaveBeenCalled()
        expect(alternative).not.toHaveBeenCalled()
    })

    it('runs nothing for a tap on a disabled button', () => {
        button.disabled = true
        touch('touchstart')
        touch('touchend')
        vi.advanceTimersByTime(1000)
        expect(main).not.toHaveBeenCalled()
    })
})
