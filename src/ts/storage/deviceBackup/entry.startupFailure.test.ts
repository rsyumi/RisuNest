import { afterEach, beforeEach, expect, test, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    checkNativeStartupStatus: vi.fn(),
    invoke: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('../../nativeStartup', () => ({
    checkNativeStartupStatus: mocks.checkNativeStartupStatus,
}))

import { deviceMaintenanceBeforeBootstrap } from './entry'

beforeEach(() => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', {
        configurable: true,
        value: {},
    })
    vi.clearAllMocks()
})

afterEach(() => {
    Reflect.deleteProperty(window, '__TAURI_INTERNALS__')
})

test('returns to the normal app entry when native setup failed before device recovery', async () => {
    mocks.checkNativeStartupStatus.mockRejectedValueOnce(
        new Error('synthetic native setup failure'),
    )

    await expect(deviceMaintenanceBeforeBootstrap()).resolves.toBeUndefined()

    expect(mocks.checkNativeStartupStatus).toHaveBeenCalledOnce()
    expect(mocks.invoke).not.toHaveBeenCalled()
    expect(document.querySelector('main[role="status"]')).toBeNull()
})
