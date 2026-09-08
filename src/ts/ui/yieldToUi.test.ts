import { afterEach, describe, expect, it, vi } from 'vitest'

import { yieldToMainThread, yieldToUi } from './yieldToUi'

const originalVisibilityState = Object.getOwnPropertyDescriptor(
    document,
    'visibilityState',
)

afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
    if (originalVisibilityState) {
        Object.defineProperty(
            document,
            'visibilityState',
            originalVisibilityState,
        )
    }
    vi.restoreAllMocks()
})

describe('yieldToMainThread', () => {
    it('uses scheduler.yield with its scheduler receiver', async () => {
        const scheduler = {
            yield(this: unknown) {
                expect(this).toBe(scheduler)
                return Promise.resolve()
            },
        }
        vi.stubGlobal('scheduler', scheduler)

        await yieldToMainThread()
    })

    it('falls back to a task instead of resolving in the current microtask checkpoint', async () => {
        vi.useFakeTimers()
        vi.stubGlobal('scheduler', undefined)
        let settled = false

        void yieldToMainThread().then(() => {
            settled = true
        })
        await Promise.resolve()

        expect(settled).toBe(false)
        await vi.runAllTimersAsync()
        expect(settled).toBe(true)
    })
})

describe('yieldToUi', () => {
    it('waits for a visible animation frame and a following task without a fixed delay', async () => {
        vi.useFakeTimers()
        Object.defineProperty(document, 'visibilityState', {
            configurable: true,
            value: 'visible',
        })
        let frameCallback: FrameRequestCallback | undefined
        const cancelAnimationFrame = vi.fn()
        vi.stubGlobal(
            'requestAnimationFrame',
            vi.fn((callback: FrameRequestCallback) => {
                frameCallback = callback
                return 17
            }),
        )
        vi.stubGlobal('cancelAnimationFrame', cancelAnimationFrame)
        let settled = false

        void yieldToUi().then(() => {
            settled = true
        })
        expect(settled).toBe(false)

        frameCallback?.(0)
        await vi.advanceTimersByTimeAsync(0)

        expect(settled).toBe(true)
        expect(cancelAnimationFrame).toHaveBeenCalledWith(17)
        expect(vi.getTimerCount()).toBe(0)
    })

    it('settles through the safety timer and cancels a pending frame', async () => {
        vi.useFakeTimers()
        Object.defineProperty(document, 'visibilityState', {
            configurable: true,
            value: 'visible',
        })
        let frameCallback: FrameRequestCallback | undefined
        const cancelAnimationFrame = vi.fn()
        vi.stubGlobal(
            'requestAnimationFrame',
            vi.fn((callback: FrameRequestCallback) => {
                frameCallback = callback
                return 23
            }),
        )
        vi.stubGlobal('cancelAnimationFrame', cancelAnimationFrame)
        let settled = false

        void yieldToUi().then(() => {
            settled = true
        })
        await vi.advanceTimersByTimeAsync(49)
        expect(settled).toBe(false)

        await vi.advanceTimersByTimeAsync(1)
        expect(settled).toBe(true)
        expect(cancelAnimationFrame).toHaveBeenCalledWith(23)
        expect(vi.getTimerCount()).toBe(0)

        frameCallback?.(50)
        expect(vi.getTimerCount()).toBe(0)
    })

    it('uses the main-thread yield when the document is hidden', async () => {
        Object.defineProperty(document, 'visibilityState', {
            configurable: true,
            value: 'hidden',
        })
        const requestAnimationFrame = vi.fn()
        const schedulerYield = vi.fn(() => Promise.resolve())
        vi.stubGlobal('requestAnimationFrame', requestAnimationFrame)
        vi.stubGlobal('scheduler', { yield: schedulerYield })

        await yieldToUi()

        expect(schedulerYield).toHaveBeenCalledOnce()
        expect(requestAnimationFrame).not.toHaveBeenCalled()
    })
})
