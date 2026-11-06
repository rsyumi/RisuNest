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
    generationHistoryLimitEnabled: false,
    generationHistoryLimitMultiplier: 2,
    messageSendKey: 'enter',
    pluginOutputCurrentChatOnly: false,
    automaticSnapshotsEnabled: true,
    snapshotKeepCount: 8,
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

    it.each(['enter', 'ctrl-shift-enter', 'button'] as const)('persists the %s send key across reloads', async (messageSendKey) => {
        const device = await loadDeviceSettings()
        device.updateDeviceSettings({ messageSendKey })
        await device.flushDeviceSettings()

        const reloaded = await loadDeviceSettings()
        expect(reloaded.getDeviceSettings().messageSendKey).toBe(messageSendKey)
    })

    it('keeps send keys independent between device stores', async () => {
        const device = await loadDeviceSettings()
        const { createNativeDeviceMarkers, DEVICE_MARKER_KEYS } = await import('./deviceMarkers')
        const firstWrite = vi.fn(async () => {})
        const secondWrite = vi.fn(async () => {})
        const store = (set: typeof firstWrite) => createNativeDeviceMarkers({
            get: async () => null, readMany: async () => [], set, patch: async () => {},
        }, DEVICE_MARKER_KEYS.map(() => null))
        const first = store(firstWrite)
        const second = store(secondWrite)
        device.loadDeviceSettings(first)
        device.updateDeviceSettings({ messageSendKey: 'button' })
        await device.flushDeviceSettings()
        device.loadDeviceSettings(second)
        expect(device.getDeviceSettings().messageSendKey).toBe('enter')
        expect(secondWrite).not.toHaveBeenCalled()
        device.updateDeviceSettings({ messageSendKey: 'ctrl-shift-enter' })
        await device.flushDeviceSettings()
        device.loadDeviceSettings(first)
        expect(device.getDeviceSettings().messageSendKey).toBe('button')
        expect(firstWrite).toHaveBeenCalledExactlyOnceWith('risuNestDeviceSettings', expect.any(String))
        expect(secondWrite).toHaveBeenCalledExactlyOnceWith('risuNestDeviceSettings', expect.any(String))
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

    it.each([
        ['a multiplier below one', { generationHistoryLimitMultiplier: 0 }],
        ['a non-finite multiplier', { generationHistoryLimitMultiplier: Number.POSITIVE_INFINITY }],
        ['an unknown key', { unknownSetting: true }],
        ['an unsupported send key', { messageSendKey: 'space' }],
        ['a keep count of zero', { snapshotKeepCount: 0 }],
        ['a keep count above eight', { snapshotKeepCount: 9 }],
        ['a fractional keep count', { snapshotKeepCount: 2.5 }],
        ['a non-boolean automatic snapshot switch', { automaticSnapshotsEnabled: 'off' }],
    ])('refuses %s and keeps every stored setting', async (_name, update) => {
        const { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()
        updateDeviceSettings({ performanceProfile: 'low-spec', nativeFileLogEnabled: false })
        const stored = localStorage.getItem('risuNestDeviceSettings')
        const listener = vi.fn()
        subscribeDeviceSettings(listener)

        expect(() => updateDeviceSettings(update as DeviceSettingsUpdate)).toThrow('Invalid device settings')

        expect(getDeviceSettings()).toEqual({ ...defaults, performanceProfile: 'low-spec', nativeFileLogEnabled: false })
        expect(localStorage.getItem('risuNestDeviceSettings')).toBe(stored)
        expect(listener).not.toHaveBeenCalled()
    })


    it('persists the generation history limit and its multiplier', async () => {
        const { updateDeviceSettings } = await loadDeviceSettings()
        updateDeviceSettings({
            generationHistoryLimitEnabled: true,
            generationHistoryLimitMultiplier: 1.5,
        })

        const { getDeviceSettings } = await loadDeviceSettings()
        expect(getDeviceSettings()).toEqual({
            ...defaults,
            generationHistoryLimitEnabled: true,
            generationHistoryLimitMultiplier: 1.5,
        })
    })

    it.each([
        ['below one', 0.5],
        ['not finite', null],
        ['a string', '2'],
    ])('rejects a stored multiplier that is %s', async (_case, multiplier) => {
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify({
            ...defaults,
            nativeFileLogEnabled: false,
            generationHistoryLimitMultiplier: multiplier,
        }))
        const { getDeviceSettings } = await loadDeviceSettings()
        expect(getDeviceSettings()).toEqual(defaults)
    })

    it('rejects stored settings without the generation history limit fields', async () => {
        const { generationHistoryLimitEnabled: _enabled, generationHistoryLimitMultiplier: _multiplier, ...older } = defaults
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify({
            ...older,
            nativeFileLogEnabled: false,
        }))
        const { getDeviceSettings } = await loadDeviceSettings()
        expect(getDeviceSettings()).toEqual(defaults)
    })

    it.each([
        ['the plugin output history field', ['pluginOutputCurrentChatOnly', 'automaticSnapshotsEnabled', 'snapshotKeepCount']],
        ['the snapshot fields', ['automaticSnapshotsEnabled', 'snapshotKeepCount']],
    ])('keeps settings stored before %s and takes the new defaults', async (_case, added) => {
        const released = Object.fromEntries(Object.entries(defaults).filter(([key]) => !added.includes(key)))
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify({
            ...released,
            performanceProfile: 'low-spec',
            generationHistoryLimitEnabled: true,
        }))
        const { getDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()
        expect(getDeviceSettings()).toEqual({
            ...defaults,
            performanceProfile: 'low-spec',
            generationHistoryLimitEnabled: true,
        })

        updateDeviceSettings({ pluginOutputCurrentChatOnly: true, automaticSnapshotsEnabled: false, snapshotKeepCount: 3 })
        const reloaded = await loadDeviceSettings()
        expect(reloaded.getDeviceSettings()).toMatchObject({
            performanceProfile: 'low-spec',
            pluginOutputCurrentChatOnly: true,
            automaticSnapshotsEnabled: false,
            snapshotKeepCount: 3,
        })
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
    it('refreshes restored profile and listeners before another setting update', async () => {
        const device = await loadDeviceSettings()
        device.getDeviceSettings()
        const listener = vi.fn()
        device.subscribeDeviceSettings(listener)
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify({ ...defaults, performanceProfile: 'low-spec' }))
        expect(device.reloadDeviceSettings().performanceProfile).toBe('low-spec')
        expect(listener).toHaveBeenCalledWith(expect.objectContaining({ performanceProfile: 'low-spec' }))
        const runtime = await import('../runtimePerformanceProfile')
        expect(runtime.getRuntimePerformanceProfile()).toBe('low-spec')
        device.updateDeviceSettings({ nativeFileLogEnabled: false })
        expect(JSON.parse(localStorage.getItem('risuNestDeviceSettings')!).performanceProfile).toBe('low-spec')
    })

})

    it('keeps startup exclusions outside transferable settings and retains them when settings are replaced', async () => {
        const device = await loadDeviceSettings()
        device.updateStartupExclusions(['plugins'])
        device.updateDeviceSettings({ performanceProfile: 'low-spec' })
        await device.flushDeviceSettings()
        expect(JSON.parse(localStorage.getItem('risuNestDeviceSettings')!)).not.toHaveProperty('startupExclusions')
        expect(JSON.parse(localStorage.getItem('risuNestStartupExclusions')!)).toEqual(['plugins'])
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify(defaults))
        const reloaded = await loadDeviceSettings()
        expect(reloaded.getStartupExclusions()).toEqual(['plugins'])
        expect(reloaded.getDeviceSettings().performanceProfile).toBe('normal')
    })
