import { afterEach, describe, expect, it, vi } from 'vitest'
import {
    getRuntimePerformanceBudgets,
    getRuntimePerformanceProfile,
    setRuntimePerformanceProfile,
    subscribeRuntimePerformanceProfile,
} from './runtimePerformanceProfile'

describe('runtime performance profile', () => {
    afterEach(() => {
        setRuntimePerformanceProfile('normal')
    })

    it('preserves the existing normal budgets and makes every low-spec budget lower', () => {
        expect(getRuntimePerformanceProfile()).toBe('normal')
        expect(getRuntimePerformanceBudgets()).toMatchObject({
            browserAssetDataUrlCacheBytes: 16 * 1024 * 1024,
            chatMountedMessageBudget: 64,
            regexPlanCacheEntries: 32,
            scriptResultCacheBytes: 8 * 1024 * 1024,
            scriptResultCacheEntries: 1000,
            scriptingEngineCacheEntries: 16,
        })

        const normal = getRuntimePerformanceBudgets()
        setRuntimePerformanceProfile('low-spec')
        const lowSpec = getRuntimePerformanceBudgets()

        expect(lowSpec.chatMountedMessageBudget).toBe(40)

        for (const key of Object.keys(normal) as Array<keyof typeof normal>) {
            expect(lowSpec[key]).toBeGreaterThan(0)
            expect(lowSpec[key]).toBeLessThan(normal[key])
        }
    })

    it('notifies runtime consumers only when the explicit profile changes', () => {
        const listener = vi.fn()
        const unsubscribe = subscribeRuntimePerformanceProfile(listener)

        setRuntimePerformanceProfile('normal')
        setRuntimePerformanceProfile('low-spec')
        setRuntimePerformanceProfile('low-spec')

        expect(listener).toHaveBeenCalledTimes(1)
        expect(listener).toHaveBeenCalledWith('low-spec', getRuntimePerformanceBudgets())

        unsubscribe()
        setRuntimePerformanceProfile('normal')
        expect(listener).toHaveBeenCalledTimes(1)
    })
})
