import { afterEach, describe, expect, it, vi } from 'vitest'

const platform = vi.hoisted(() => ({ android: true }))

vi.mock('./platform', () => ({
    get isTauriAndroid() { return platform.android },
}))

import {
    beginAndroidGenerationKeepAlive,
    endAndroidGenerationKeepAlive,
} from './androidGenerationKeepAlive'

describe('Android generation keep-alive', () => {
    afterEach(() => {
        platform.android = true
        delete (window as Window & { RisuGenerationKeepAlive?: unknown }).RisuGenerationKeepAlive
        localStorage.clear()
    })

    it('does not use an injected bridge outside Tauri Android', () => {
        const begin = vi.fn(() => true)
        const end = vi.fn()
        window.RisuGenerationKeepAlive = { begin, end } as any
        platform.android = false

        expect(beginAndroidGenerationKeepAlive(true)).toBe(false)
        endAndroidGenerationKeepAlive(true)

        expect(begin).not.toHaveBeenCalled()
        expect(end).not.toHaveBeenCalled()
    })

    it('is a safe no-op when disabled, unavailable, or throwing', () => {
        expect(beginAndroidGenerationKeepAlive(false)).toBe(false)
        expect(beginAndroidGenerationKeepAlive(true)).toBe(false)
        window.RisuGenerationKeepAlive = {
            begin: () => { throw new Error('bridge unavailable') },
        } as any
        expect(beginAndroidGenerationKeepAlive(true)).toBe(false)
        expect(() => endAndroidGenerationKeepAlive(true)).not.toThrow()
    })

    it('acquires once and releases only after a successful begin', () => {
        const begin = vi.fn(() => true)
        const end = vi.fn()
        window.RisuGenerationKeepAlive = { begin, end } as any

        const acquired = beginAndroidGenerationKeepAlive(true)
        endAndroidGenerationKeepAlive(acquired)
        endAndroidGenerationKeepAlive(false)

        expect(begin).toHaveBeenCalledTimes(1)
        expect(end).toHaveBeenCalledTimes(1)
    })
})
