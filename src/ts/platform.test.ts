import { afterEach, describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-os', () => ({
    type: vi.fn(),
    version: vi.fn()
}))

type ExpectedRuntime = {
    isTauriAndroid: boolean
    isMobile: boolean
    isTauriMobile: boolean
    isTauriDesktop: boolean
}

const cases: Array<[string, boolean, string, ExpectedRuntime]> = [
    ['Android Tauri', true, 'Mozilla/5.0 (Linux; Android 14)', {
        isTauriAndroid: true,
        isMobile: true,
        isTauriMobile: true,
        isTauriDesktop: false,
    }],
    ['Windows Tauri', true, 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)', {
        isTauriAndroid: false,
        isMobile: false,
        isTauriMobile: false,
        isTauriDesktop: true,
    }],
    ['Android web', false, 'Mozilla/5.0 (Linux; Android 14)', {
        isTauriAndroid: false,
        isMobile: true,
        isTauriMobile: false,
        isTauriDesktop: false,
    }],
    ['iOS Tauri', true, 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)', {
        isTauriAndroid: false,
        isMobile: true,
        isTauriMobile: true,
        isTauriDesktop: false,
    }],
]

const originalTauriInternals = (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
const originalUserAgent = Object.getOwnPropertyDescriptor(navigator, 'userAgent')

afterEach(() => {
    const windowWithTauri = window as Window & { __TAURI_INTERNALS__?: unknown }
    if (originalTauriInternals === undefined) {
        delete windowWithTauri.__TAURI_INTERNALS__
    } else {
        windowWithTauri.__TAURI_INTERNALS__ = originalTauriInternals
    }

    if (originalUserAgent) {
        Object.defineProperty(navigator, 'userAgent', originalUserAgent)
    } else {
        delete (navigator as Navigator & { userAgent?: string }).userAgent
    }
    vi.resetModules()
})

describe('runtime classification', () => {
    it.each(cases)('%s classifies Tauri and mobile runtime', async (_name, tauri, userAgent, expected) => {
        vi.resetModules()
        const windowWithTauri = window as Window & { __TAURI_INTERNALS__?: unknown }
        if (tauri) {
            windowWithTauri.__TAURI_INTERNALS__ = {}
        } else {
            delete windowWithTauri.__TAURI_INTERNALS__
        }
        Object.defineProperty(navigator, 'userAgent', {
            configurable: true,
            value: userAgent,
        })

        const runtime = await import('./platform')

        expect({
            isTauriAndroid: runtime.isTauriAndroid,
            isMobile: runtime.isMobile,
            isTauriMobile: runtime.isTauriMobile,
            isTauriDesktop: runtime.isTauriDesktop,
        }).toEqual(expected)
    })
})
