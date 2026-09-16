import { beforeEach, describe, expect, it, vi } from 'vitest'

import {
    DEVICE_MARKER_KEYS,
    createLocalDeviceMarkers,
    createNativeDeviceMarkers,
    getDeviceMarkers,
    initializeDeviceMarkers,
    installDeviceMarkers,
} from './deviceMarkers'
import type { NativeDeviceSettings } from './nativeDeviceSettings'

function settingsBackend() {
    const stored = new Map<string, unknown>()
    const settings: NativeDeviceSettings = {
        get: vi.fn(async (key: string) => stored.get(key) ?? null),
        readMany: vi.fn(async (keys: readonly string[]) =>
            keys.map((key) => stored.get(key) ?? null)),
        set: vi.fn(async (key: string, value: unknown | null) => {
            if (value === null) stored.delete(key)
            else stored.set(key, value)
        }),
        patch: vi.fn(async () => undefined),
    }
    return { stored, settings }
}

describe('device markers', () => {
    const nativeWindow = window as Window & { __TAURI_INTERNALS__?: unknown }

    beforeEach(() => {
        nativeWindow.__TAURI_INTERNALS__ = {}
        installDeviceMarkers(null)
    })

    it('loads every marker in one read and answers from memory', async () => {
        const { stored, settings } = settingsBackend()
        stored.set('accountst', 'able')
        stored.set('hub', 'nightly')

        const markers = await initializeDeviceMarkers(settings)

        expect(settings.readMany).toHaveBeenCalledTimes(1)
        expect(settings.readMany).toHaveBeenCalledWith(DEVICE_MARKER_KEYS)
        expect(markers.getItem('accountst')).toBe('able')
        expect(markers.getItem('hub')).toBe('nightly')
        expect(markers.getItem('dosync')).toBeNull()
        expect(getDeviceMarkers()).toBe(markers)
    })

    it('sends one key per change and reports the failed commit at the flush', async () => {
        const { stored, settings } = settingsBackend()
        const markers = createNativeDeviceMarkers(settings, DEVICE_MARKER_KEYS.map(() => null))

        markers.setItem('dosync', 'sync')
        markers.setItem('accountst', 'able')
        expect(markers.getItem('dosync')).toBe('sync')
        await markers.flush()
        expect(settings.set).toHaveBeenCalledTimes(2)
        expect(settings.set).toHaveBeenNthCalledWith(1, 'dosync', 'sync')
        expect(settings.set).toHaveBeenNthCalledWith(2, 'accountst', 'able')
        expect(stored.get('accountst')).toBe('able')

        markers.removeItem('accountst')
        await markers.flush()
        expect(stored.has('accountst')).toBe(false)
        expect(markers.getItem('accountst')).toBeNull()

        vi.mocked(settings.set).mockRejectedValueOnce(new Error('device file is read only'))
        markers.setItem('nightlyWarned', 'true')
        await expect(markers.flush()).rejects.toThrow('device file is read only')
    })

    it('rejects a key that is not a device marker', () => {
        const { settings } = settingsBackend()
        const markers = createNativeDeviceMarkers(settings, DEVICE_MARKER_KEYS.map(() => null))
        expect(() => markers.setItem('fallbackRisuToken', 'x')).toThrow('not a device marker')
        expect(() => markers.getItem('mainpage')).toThrow('not a device marker')
        expect(() => createLocalDeviceMarkers(localStorage).setItem('mainpage', 'x'))
            .toThrow('not a device marker')
    })

    it('reports a stored marker that is not a string', () => {
        const { settings } = settingsBackend()
        expect(() => createNativeDeviceMarkers(
            settings,
            DEVICE_MARKER_KEYS.map((key) => (key === 'hub' ? { nightly: true } : null)),
        )).toThrow('unreadable')
    })

    it('refuses to answer a native install before the device file is loaded', () => {
        expect(() => getDeviceMarkers()).toThrow('not loaded yet')
    })

    it('falls back to local storage on the web build', () => {
        delete nativeWindow.__TAURI_INTERNALS__
        localStorage.setItem('hub', 'nightly')
        try {
            expect(getDeviceMarkers().getItem('hub')).toBe('nightly')
        } finally {
            localStorage.removeItem('hub')
        }
    })
})
