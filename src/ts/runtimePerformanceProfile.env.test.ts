import { afterEach, describe, expect, it, vi } from 'vitest'

describe('runtime performance profile build flag', () => {
    afterEach(() => {
        vi.unstubAllEnvs()
        vi.resetModules()
    })

    it.each([undefined, '', 'normal', 'invalid'])('defaults %s to normal', async (value) => {
        vi.stubEnv('VITE_RUNTIME_PERFORMANCE_PROFILE', value)

        const profile = await import('./runtimePerformanceProfile')

        expect(profile.getRuntimePerformanceProfile()).toBe('normal')
        expect(profile.getRuntimePerformanceBudgets().scriptingEngineCacheEntries).toBe(16)
    })

    it('selects the low-spec budgets only for the explicit low-spec flag', async () => {
        vi.stubEnv('VITE_RUNTIME_PERFORMANCE_PROFILE', 'low-spec')

        const profile = await import('./runtimePerformanceProfile')

        expect(profile.getRuntimePerformanceProfile()).toBe('low-spec')
        expect(profile.getRuntimePerformanceBudgets().scriptingEngineCacheEntries).toBe(4)
    })
})
