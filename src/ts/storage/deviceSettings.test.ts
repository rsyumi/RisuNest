import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'

type DeviceSettingsUpdate = Parameters<typeof import('./deviceSettings').updateDeviceSettings>[0]

// @ts-expect-error The persisted settings schema is not caller-configurable.
const schemaUpdate: DeviceSettingsUpdate = { schema: 'risunest.device-settings/v1' }
void schemaUpdate

async function loadDeviceSettings() {
    vi.resetModules()
    return await import('./deviceSettings')
}

const defaults = {
    schema: 'risunest.device-settings/v1',
    performanceProfile: 'normal',
    androidKeepAliveDuringGeneration: true,
    nativeFileLogEnabled: true,
}

describe('device settings', () => {
    beforeEach(() => {
        localStorage.clear()
        setRuntimePerformanceProfile('normal')
    })

    afterEach(() => {
        vi.restoreAllMocks()
        vi.unstubAllEnvs()
        vi.resetModules()
        localStorage.clear()
        setRuntimePerformanceProfile('normal')
    })

    it('uses the exact defaults when no stored settings exist', async () => {
        const { getDeviceSettings } = await loadDeviceSettings()

        expect(getDeviceSettings()).toEqual(defaults)
    })

    it('preserves an explicitly disabled generation keep-alive after reload', async () => {
        const { updateDeviceSettings } = await loadDeviceSettings()
        updateDeviceSettings({ androidKeepAliveDuringGeneration: false })

        const { getDeviceSettings } = await loadDeviceSettings()
        expect(getDeviceSettings().androidKeepAliveDuringGeneration).toBe(false)
    })


    it('recovers defaults from malformed stored JSON', async () => {
        localStorage.setItem('risuNestDeviceSettings', '{not json')
        const deviceSettings = await loadDeviceSettings()
        expect(deviceSettings.getDeviceSettings()).toEqual(defaults)
    })

    it.each([
        ['absent', null],
        ['invalid', '{not json'],
    ])('applies the normalized default profile when storage is %s', async (_case, stored) => {
        vi.stubEnv('VITE_RUNTIME_PERFORMANCE_PROFILE', 'low-spec')
        if (stored !== null) localStorage.setItem('risuNestDeviceSettings', stored)

        const deviceSettings = await loadDeviceSettings()
        deviceSettings.getDeviceSettings()
        const { getRuntimePerformanceProfile } = await import('../runtimePerformanceProfile')

        expect(getRuntimePerformanceProfile()).toBe('normal')
    })

    it('defers storage and runtime initialization until the first API use, then initializes once', async () => {
        vi.resetModules()
        const runtimeProfile = await import('../runtimePerformanceProfile')
        runtimeProfile.setRuntimePerformanceProfile('low-spec')
        const getItem = vi.spyOn(localStorage, 'getItem')
        const setProfile = vi.spyOn(runtimeProfile, 'setRuntimePerformanceProfile')
        const deviceSettings = await import('./deviceSettings')

        expect(getItem).not.toHaveBeenCalled()
        expect(setProfile).not.toHaveBeenCalled()

        expect(deviceSettings.getDeviceSettings()).toEqual(defaults)
        expect(setProfile).toHaveBeenCalledOnce()
        expect(setProfile).toHaveBeenCalledWith('normal')

        deviceSettings.getDeviceSettings()
        deviceSettings.updateDeviceSettings({ nativeFileLogEnabled: false })
        const unsubscribe = deviceSettings.subscribeDeviceSettings(vi.fn())
        unsubscribe()

        expect(getItem).toHaveBeenCalledOnce()
        expect(setProfile).toHaveBeenCalledOnce()
    })

    it('loads persisted settings once when update is the first API use', async () => {
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify({
            ...defaults,
            performanceProfile: 'low-spec',
        }))
        vi.resetModules()
        const runtimeProfile = await import('../runtimePerformanceProfile')
        const getItem = vi.spyOn(localStorage, 'getItem')
        const setProfile = vi.spyOn(runtimeProfile, 'setRuntimePerformanceProfile')
        const deviceSettings = await import('./deviceSettings')

        expect(getItem).not.toHaveBeenCalled()
        expect(setProfile).not.toHaveBeenCalled()

        const updated = deviceSettings.updateDeviceSettings({
            nativeFileLogEnabled: false,
        })

        expect(updated).toEqual({
            ...defaults,
            performanceProfile: 'low-spec',
            nativeFileLogEnabled: false,
        })
        expect(getItem).toHaveBeenCalledOnce()
        expect(setProfile).toHaveBeenCalledOnce()
        expect(setProfile).toHaveBeenCalledWith('low-spec')
    })

    it('loads settings once when subscribe is the first API use', async () => {
        vi.resetModules()
        const runtimeProfile = await import('../runtimePerformanceProfile')
        runtimeProfile.setRuntimePerformanceProfile('low-spec')
        const getItem = vi.spyOn(localStorage, 'getItem')
        const setProfile = vi.spyOn(runtimeProfile, 'setRuntimePerformanceProfile')
        const deviceSettings = await import('./deviceSettings')

        expect(getItem).not.toHaveBeenCalled()
        expect(setProfile).not.toHaveBeenCalled()

        const unsubscribeFirst = deviceSettings.subscribeDeviceSettings(vi.fn())
        const unsubscribeSecond = deviceSettings.subscribeDeviceSettings(vi.fn())

        expect(getItem).toHaveBeenCalledOnce()
        expect(setProfile).toHaveBeenCalledOnce()
        expect(setProfile).toHaveBeenCalledWith('normal')

        unsubscribeFirst()
        unsubscribeSecond()
    })

    it('guards storage read and write failures', async () => {
        const getItem = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
            throw new Error('blocked read')
        })
        const deviceSettings = await loadDeviceSettings()
        expect(deviceSettings.getDeviceSettings()).toEqual(defaults)

        getItem.mockRestore()
        const setItem = vi
            .spyOn(Storage.prototype, 'setItem')
            .mockImplementation(() => {
                throw new Error('blocked write')
            })
        expect(() =>
            deviceSettings.updateDeviceSettings({
                nativeFileLogEnabled: false,
            }),
        ).not.toThrow()
        expect(deviceSettings.getDeviceSettings().nativeFileLogEnabled).toBe(
            false,
        )
        setItem.mockRestore()
    })

    it('validates and persists complete updates', async () => {
        const { getDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()

        updateDeviceSettings({
            performanceProfile: 'low-spec',
            androidKeepAliveDuringGeneration: true,
            nativeFileLogEnabled: false,
        })

        expect(getDeviceSettings()).toEqual({
            ...defaults,
            performanceProfile: 'low-spec',
            androidKeepAliveDuringGeneration: true,
            nativeFileLogEnabled: false,
        })
        expect(JSON.parse(localStorage.getItem('risuNestDeviceSettings') ?? '')).toEqual(getDeviceSettings())
    })


    it('ignores a runtime schema override while applying valid settings', async () => {
        const { getDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()

        updateDeviceSettings({
            schema: 'not-a-device-settings-schema',
            nativeFileLogEnabled: false,
        } as never)

        expect(getDeviceSettings()).toEqual({
            ...defaults,
            nativeFileLogEnabled: false,
        })
    })

    it('returns an isolated normalized snapshot after persistence and notification', async () => {
        const { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()
        const listener = vi.fn(() => {
            expect(
                JSON.parse(
                    localStorage.getItem('risuNestDeviceSettings') ?? '',
                ),
            ).toEqual({
                ...defaults,
                androidKeepAliveDuringGeneration: false,
            })
        })
        subscribeDeviceSettings(listener)

        const updated = updateDeviceSettings({
            androidKeepAliveDuringGeneration: false,
        })

        expect(listener).toHaveBeenCalledOnce()
        expect(updated).toEqual({
            ...defaults,
            androidKeepAliveDuringGeneration: false,
        })
        updated.androidKeepAliveDuringGeneration = true
        expect(getDeviceSettings().androidKeepAliveDuringGeneration).toBe(false)
    })

    it('returns immutable snapshots and notifies only active subscribers', async () => {
        const {
            getDeviceSettings,
            subscribeDeviceSettings,
            updateDeviceSettings,
        } = await loadDeviceSettings()
        const listener = vi.fn()
        const unsubscribe = subscribeDeviceSettings(listener)
        const snapshot = getDeviceSettings()
        ;(
            snapshot as { androidKeepAliveDuringGeneration: boolean }
        ).androidKeepAliveDuringGeneration = true

        updateDeviceSettings({ androidKeepAliveDuringGeneration: false })
        unsubscribe()
        updateDeviceSettings({ androidKeepAliveDuringGeneration: true })

        expect(getDeviceSettings().androidKeepAliveDuringGeneration).toBe(true)
        expect(listener).toHaveBeenCalledTimes(1)
        expect(listener).toHaveBeenCalledWith({
            ...defaults,
            androidKeepAliveDuringGeneration: false,
        })
    })

    it('applies a changed performance profile immediately', async () => {
        const { updateDeviceSettings } = await loadDeviceSettings()
        const { getRuntimePerformanceProfile } = await import('../runtimePerformanceProfile')

        updateDeviceSettings({ performanceProfile: 'low-spec' })

        expect(getRuntimePerformanceProfile()).toBe('low-spec')
    })
})
