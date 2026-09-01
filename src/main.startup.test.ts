import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const startup = vi.hoisted(() => ({ cacheBudget: 0 }))

vi.mock('./ts/polyfill', () => ({}))
vi.mock('core-js/actual', () => ({}))
vi.mock('./ts/storage/database.svelte', () => ({}))
vi.mock('./App.svelte', () => ({ default: {} }))
vi.mock('./ts/bootstrap', async () => {
    const { getRuntimePerformanceBudgets } = await import('./ts/runtimePerformanceProfile')
    startup.cacheBudget = getRuntimePerformanceBudgets().browserAssetDataUrlCacheBytes
    return { loadData: vi.fn() }
})
vi.mock('./ts/hotkey', () => ({ initHotkey: vi.fn() }))
vi.mock('./preload', () => ({ preLoadCheck: vi.fn() }))
vi.mock('svelte', () => ({ mount: vi.fn(() => ({})) }))

const deviceSettings = {
    schema: 'risunest.device-settings/v1',
    performanceProfile: 'low-spec',
    androidKeepAliveDuringGeneration: false,
    nativeFileLogEnabled: true,
    syncAutoListen: false,
    syncListenMethod: 'lan',
    syncFixedPort: 32145,
    syncPublicBaseUrl: '',
}

describe('application startup performance profile', () => {
    beforeEach(() => {
        localStorage.clear()
        document.body.innerHTML = '<div id="app"></div><div id="preloading"></div>'
        startup.cacheBudget = 0
    })

    afterEach(() => {
        vi.unstubAllEnvs()
        vi.resetModules()
        localStorage.clear()
    })

    it('loads a persisted low-spec profile before bootstrap constructs runtime caches', async () => {
        localStorage.setItem('risuNestDeviceSettings', JSON.stringify(deviceSettings))

        await import('./main')

        expect(startup.cacheBudget).toBe(8 * 1024 * 1024)
    })

})
