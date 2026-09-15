// @vitest-environment happy-dom

import { describe, expect, it, vi } from 'vitest'

vi.mock('./ts/polyfill', () => ({}))
vi.mock('core-js/actual', () => ({}))
vi.mock('katex/dist/katex.min.css', () => ({}))
vi.mock('./ts/storage/deviceSettingsStartup', () => ({}))
vi.mock('./ts/storage/database.svelte', () => ({}))
vi.mock('./App.svelte', () => ({ default: {} }))
vi.mock('./ts/bootstrap', () => ({ loadData: vi.fn() }))
vi.mock('./ts/hotkey', () => ({ initHotkey: vi.fn() }))
vi.mock('./preload', () => ({ preLoadCheck: vi.fn() }))
vi.mock('svelte', () => ({ mount: vi.fn(() => ({})) }))
vi.mock('./ts/storage/deviceBackup/jobRecovery', () => ({
    resumePortableExportsAfterBootstrap: vi.fn(async () => {}),
}))
vi.mock('./ts/storage/recoveryMode.svelte', () => ({
    decideBoot: vi.fn(async () => 'recovery'),
}))
// Importing the real store module would evaluate the application graph the mocks above
// exist to keep out, so only the carrier this case reads stands in for it.
vi.mock('./ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { recoveryStart: writable<(() => void) | null>(null) }
})

/**
 * One case per file: the application graph is evaluated once, so a second case here would
 * observe the deferred start this one began.
 */
describe('the recovery shell holds the ordinary start', () => {
    it(
        'never loads the library until the shell hands over',
        async () => {
            document.body.innerHTML =
                '<div id="app"></div><div id="preloading"></div>'
            await import('./normalMain')
            await new Promise((resolve) => setTimeout(resolve, 0))

            const { loadData } = await import('./ts/bootstrap')
            expect(loadData).not.toHaveBeenCalled()

            const { recoveryStart } = await import('./ts/stores.svelte')
            const { get } = await import('svelte/store')
            const start = get(recoveryStart)
            expect(start).toBeTypeOf('function')

            start?.()
            // The ordinary start yields to the UI first, so give that a few turns to land.
            for (let index = 0; index < 40; index += 1) {
                if (vi.mocked(loadData).mock.calls.length > 0) break
                await new Promise((resolve) => setTimeout(resolve, 5))
            }
            expect(loadData).toHaveBeenCalledTimes(1)
        },
        30_000,
    )
})
