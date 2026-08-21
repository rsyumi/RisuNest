import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { keepFocusedInputVisible } from './imeVisibility'

class TestVisualViewport extends EventTarget {
    height = 800
    offsetTop = 0
}

const originalVisualViewport = Object.getOwnPropertyDescriptor(window, 'visualViewport')

let animationFrames: FrameRequestCallback[]
let visualViewport: TestVisualViewport

function flushAnimationFrames() {
    const pendingFrames = animationFrames.splice(0)
    pendingFrames.forEach((callback) => callback(0))
}

function createApp(inputTop: number, inputBottom: number) {
    const app = document.createElement('main')
    const input = document.createElement('textarea')
    app.appendChild(input)
    document.body.appendChild(app)

    vi.spyOn(app, 'getBoundingClientRect').mockImplementation(() => ({
        bottom: 800,
        height: 800,
        left: 0,
        right: 400,
        top: 0,
        width: 400,
        x: 0,
        y: 0,
        toJSON: () => ({}),
    }))
    vi.spyOn(input, 'getBoundingClientRect').mockImplementation(() => {
        const shift = Number((app.style.translate ?? '').match(/-(\d+)px/)?.[1] ?? 0)
        return {
            bottom: inputBottom - shift,
            height: inputBottom - inputTop,
            left: 0,
            right: 400,
            top: inputTop - shift,
            width: 400,
            x: 0,
            y: inputTop - shift,
            toJSON: () => ({}),
        }
    })

    return { app, input }
}

function createPluginFrame() {
    const frame = document.createElement('iframe')
    frame.setAttribute('data-risu-plugin-frame', '')
    frame.tabIndex = 0
    document.body.appendChild(frame)

    vi.spyOn(frame, 'getBoundingClientRect').mockImplementation(() => {
        const shift = Number((frame.style.translate ?? '').match(/-(\d+)px/)?.[1] ?? 0)
        return {
            bottom: 800 - shift,
            height: 800,
            left: 0,
            right: 400,
            top: -shift,
            width: 400,
            x: 0,
            y: -shift,
            toJSON: () => ({}),
        }
    })

    return frame
}

function reportPluginInput(frame: HTMLIFrameElement, rect: { top: number; bottom: number } | null) {
    window.dispatchEvent(new MessageEvent('message', {
        data: { type: 'RISU_PLUGIN_INPUT_FOCUS', rect },
        source: frame.contentWindow,
    }))
}

beforeEach(() => {
    animationFrames = []
    visualViewport = new TestVisualViewport()
    Object.defineProperty(window, 'visualViewport', { configurable: true, value: visualViewport })
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
        animationFrames.push(callback)
        return animationFrames.length
    })
    vi.stubGlobal('cancelAnimationFrame', vi.fn())
})

afterEach(() => {
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()

    if (originalVisualViewport) {
        Object.defineProperty(window, 'visualViewport', originalVisualViewport)
    } else {
        delete (window as Window & { visualViewport?: VisualViewport }).visualViewport
    }
})

describe('IME visibility', () => {
    it('keeps the app in place when the focused input remains visible', () => {
        const { app, input } = createApp(200, 240)
        const action = keepFocusedInputVisible(app, true)

        input.focus()
        visualViewport.height = 480
        visualViewport.offsetTop = 20
        visualViewport.dispatchEvent(new Event('resize'))
        flushAnimationFrames()

        expect(app.style.translate ?? '').toBe('')

        action.destroy()
    })

    it('pans the whole app when the focused input becomes occluded before input', () => {
        const { app, input } = createApp(700, 780)
        const action = keepFocusedInputVisible(app, true)

        input.focus()
        flushAnimationFrames()
        expect(app.style.translate ?? '').toBe('')

        visualViewport.height = 480
        visualViewport.offsetTop = 20
        visualViewport.dispatchEvent(new Event('resize'))
        flushAnimationFrames()

        expect(app.style.translate ?? '').toContain('-288px')

        visualViewport.dispatchEvent(new Event('resize'))
        flushAnimationFrames()
        expect(app.style.translate ?? '').toContain('-288px')

        action.destroy()
    })

    it('restores the app when focus leaves it', () => {
        const { app, input } = createApp(700, 780)
        const outsideButton = document.createElement('button')
        document.body.appendChild(outsideButton)
        const action = keepFocusedInputVisible(app, true)

        input.focus()
        visualViewport.height = 500
        visualViewport.dispatchEvent(new Event('resize'))
        flushAnimationFrames()
        expect(app.style.translate ?? '').not.toBe('')

        outsideButton.focus()
        flushAnimationFrames()

        expect(app.style.translate ?? '').toBe('')

        action.destroy()
    })

    it('pans the focused plugin frame when its input becomes occluded', () => {
        const { app } = createApp(200, 240)
        const frame = createPluginFrame()
        const action = keepFocusedInputVisible(app, true)

        frame.focus()
        expect(document.activeElement).toBe(frame)
        reportPluginInput(frame, { top: 700, bottom: 780 })

        visualViewport.height = 480
        visualViewport.offsetTop = 20
        visualViewport.dispatchEvent(new Event('resize'))
        flushAnimationFrames()

        expect(frame.style.translate ?? '').toContain('-288px')
        expect(app.style.translate ?? '').toBe('')

        reportPluginInput(frame, null)
        flushAnimationFrames()
        expect(frame.style.translate ?? '').toBe('')

        action.destroy()
    })
})
