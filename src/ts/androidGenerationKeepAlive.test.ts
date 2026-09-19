import { afterEach, describe, expect, it, vi } from 'vitest'

const platform = vi.hoisted(() => ({ android: true }))

vi.mock('./platform', () => ({
    get isTauriAndroid() { return platform.android },
}))

import {
    beginAndroidGenerationKeepAlive,
    endAndroidGenerationKeepAlive,
    requestAndroidGenerationNotifications,
} from './androidGenerationKeepAlive'

describe('Android generation keep-alive', () => {
    afterEach(() => {
        platform.android = true
        delete window.RisuGenerationKeepAlive
        localStorage.clear()
    })

    it('does not use an injected bridge outside Tauri Android', async () => {
        const begin = vi.fn(async () => true)
        const end = vi.fn()
        window.RisuGenerationKeepAlive = { begin, end } as any
        platform.android = false

        expect(await beginAndroidGenerationKeepAlive(true)).toBe(false)
        await endAndroidGenerationKeepAlive(true)

        expect(begin).not.toHaveBeenCalled()
        expect(end).not.toHaveBeenCalled()
    })

    it('is a safe no-op when disabled, unavailable, or rejecting', async () => {
        expect(await beginAndroidGenerationKeepAlive(false)).toBe(false)
        expect(await beginAndroidGenerationKeepAlive(true)).toBe(false)
        window.RisuGenerationKeepAlive = {
            begin: async () => { throw new Error('bridge unavailable') },
            end: async () => { throw new Error('bridge unavailable') },
        } as any
        expect(await beginAndroidGenerationKeepAlive(true)).toBe(false)
        await expect(endAndroidGenerationKeepAlive(true)).resolves.toBeUndefined()
    })

    it('acquires once and releases only after a successful asynchronous begin', async () => {
        const begin = vi.fn(async () => true)
        const end = vi.fn(async () => true)
        window.RisuGenerationKeepAlive = { begin, end } as any

        const acquired = await beginAndroidGenerationKeepAlive(true)
        await endAndroidGenerationKeepAlive(acquired)
        await endAndroidGenerationKeepAlive(false)

        expect(begin).toHaveBeenCalledTimes(1)
        expect(end).toHaveBeenCalledTimes(1)
    })

    it('keeps explicit permission requests separate from generation acquisition', async () => {
        const requestNotifications = vi.fn(async () => {})
        window.RisuGenerationKeepAlive = {
            begin: async () => false,
            requestNotifications,
        } as any
        expect(await beginAndroidGenerationKeepAlive(true)).toBe(false)
        expect(requestNotifications).not.toHaveBeenCalled()
        await requestAndroidGenerationNotifications()
        expect(requestNotifications).toHaveBeenCalledOnce()
    })

    it('does not mistake an asynchronous refusal for ownership', async () => {
        const end = vi.fn()
        window.RisuGenerationKeepAlive = {
            begin: async () => false,
            end,
        } as any

        const acquired = await beginAndroidGenerationKeepAlive(true)
        expect(acquired).toBe(false)
        await endAndroidGenerationKeepAlive(acquired)
        expect(end).not.toHaveBeenCalled()
    })
})
