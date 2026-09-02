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
    androidKeepAliveDuringGeneration: false,
    nativeFileLogEnabled: true,
    syncAutoListen: false,
    syncListenMethod: 'lan',
    syncFixedPort: 32145,
    syncPublicBaseUrl: '',
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

    it.each([0, 65536, -1, 1.5])('recovers exact defaults from an invalid stored port %s', async (syncFixedPort) => {
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify({ ...defaults, syncFixedPort }))

        const deviceSettings = await loadDeviceSettings()

        expect(deviceSettings.getDeviceSettings()).toEqual(defaults)
    })

    it('recovers defaults from malformed stored JSON', async () => {
        localStorage.setItem('risuNestDeviceSettings', '{not json')
        const deviceSettings = await loadDeviceSettings()
        expect(deviceSettings.getDeviceSettings()).toEqual(defaults)
    })

    it('keeps the configured build profile when no valid stored profile exists', async () => {
        vi.stubEnv('VITE_RUNTIME_PERFORMANCE_PROFILE', 'low-spec')
        localStorage.setItem('risuNestDeviceSettings', '{not json')

        await loadDeviceSettings()
        const { getRuntimePerformanceProfile } = await import('../runtimePerformanceProfile')

        expect(getRuntimePerformanceProfile()).toBe('low-spec')
    })

    it('guards storage read and write failures', async () => {
        const getItem = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
            throw new Error('blocked read')
        })
        const deviceSettings = await loadDeviceSettings()
        expect(deviceSettings.getDeviceSettings()).toEqual(defaults)

        getItem.mockRestore()
        const setItem = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
            throw new Error('blocked write')
        })
        expect(() => deviceSettings.updateDeviceSettings({ syncAutoListen: true })).not.toThrow()
        expect(deviceSettings.getDeviceSettings().syncAutoListen).toBe(true)
        setItem.mockRestore()
    })

    it('validates and persists complete updates', async () => {
        const { getDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()

        updateDeviceSettings({
            performanceProfile: 'low-spec',
            androidKeepAliveDuringGeneration: true,
            nativeFileLogEnabled: false,
            syncAutoListen: true,
            syncListenMethod: 'fixed-url',
            syncFixedPort: 43000,
            syncPublicBaseUrl: 'https://sync.example.test',
        })

        expect(getDeviceSettings()).toEqual({
            ...defaults,
            performanceProfile: 'low-spec',
            androidKeepAliveDuringGeneration: true,
            nativeFileLogEnabled: false,
            syncAutoListen: true,
            syncListenMethod: 'fixed-url',
            syncFixedPort: 43000,
            syncPublicBaseUrl: 'https://sync.example.test',
        })
        expect(JSON.parse(localStorage.getItem('risuNestDeviceSettings') ?? '')).toEqual(getDeviceSettings())
    })

    it.each([0, 65536])('recovers exact defaults from an invalid updated port %s', async (syncFixedPort) => {
        const { getDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()
        updateDeviceSettings({ syncAutoListen: true })

        updateDeviceSettings({ syncFixedPort })

        expect(getDeviceSettings()).toEqual(defaults)
        expect(JSON.parse(localStorage.getItem('risuNestDeviceSettings') ?? '')).toEqual(defaults)
    })

    it('ignores a runtime schema override while applying valid settings', async () => {
        const { getDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()

        updateDeviceSettings({
            schema: 'not-a-device-settings-schema',
            syncAutoListen: true,
        } as never)

        expect(getDeviceSettings()).toEqual({ ...defaults, syncAutoListen: true })
    })

    it('returns an isolated normalized snapshot after persistence and notification', async () => {
        const { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()
        const listener = vi.fn(() => {
            expect(JSON.parse(localStorage.getItem('risuNestDeviceSettings') ?? '')).toEqual({
                ...defaults,
                syncFixedPort: 43000,
            })
        })
        subscribeDeviceSettings(listener)

        const updated = updateDeviceSettings({ syncFixedPort: 43000 })

        expect(listener).toHaveBeenCalledOnce()
        expect(updated).toEqual({ ...defaults, syncFixedPort: 43000 })
        updated.syncFixedPort = 1
        expect(getDeviceSettings().syncFixedPort).toBe(43000)
    })

    it('returns immutable snapshots and notifies only active subscribers', async () => {
        const { getDeviceSettings, subscribeDeviceSettings, updateDeviceSettings } = await loadDeviceSettings()
        const listener = vi.fn()
        const unsubscribe = subscribeDeviceSettings(listener)
        const snapshot = getDeviceSettings()
        ;(snapshot as { syncFixedPort: number }).syncFixedPort = 1

        updateDeviceSettings({ syncFixedPort: 43000 })
        unsubscribe()
        updateDeviceSettings({ syncFixedPort: 43001 })

        expect(getDeviceSettings().syncFixedPort).toBe(43001)
        expect(listener).toHaveBeenCalledTimes(1)
        expect(listener).toHaveBeenCalledWith({ ...defaults, syncFixedPort: 43000 })
    })

    it('applies a changed performance profile immediately', async () => {
        const { updateDeviceSettings } = await loadDeviceSettings()
        const { getRuntimePerformanceProfile } = await import('../runtimePerformanceProfile')

        updateDeviceSettings({ performanceProfile: 'low-spec' })

        expect(getRuntimePerformanceProfile()).toBe('low-spec')
    })
})
