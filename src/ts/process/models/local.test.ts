import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
    fetch: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('@tauri-apps/api/path', () => ({ appDataDir: vi.fn(), join: vi.fn() }))
vi.mock('@tauri-apps/plugin-fs', () => ({ exists: vi.fn(), readTextFile: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertClear: vi.fn(), alertWait: vi.fn() }))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: vi.fn() }))
vi.mock('src/ts/util', () => ({ sleep: vi.fn() }))
vi.mock('src/ts/platform', () => ({ isTauriDesktop: false }))

import { tokenizeGGUFModel } from './local'

describe('tokenizeGGUFModel', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.fetch = vi.fn()
        vi.stubGlobal('fetch', mocks.fetch)
    })

    afterEach(() => {
        vi.unstubAllGlobals()
    })

    it('rejects local GGUF models outside the desktop app before native or localhost access', async () => {
        await expect(tokenizeGGUFModel('hello')).rejects.toThrow(
            'Local GGUF models are available only in the desktop app.',
        )
        expect(mocks.invoke).not.toHaveBeenCalled()
        expect(mocks.fetch).not.toHaveBeenCalled()
    })
})
