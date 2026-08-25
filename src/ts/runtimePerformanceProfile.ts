export type RuntimePerformanceProfile = 'normal' | 'low-spec'

export interface RuntimePerformanceBudgets {
    browserAssetDataUrlCacheBytes: number
    regexPlanCacheEntries: number
    scriptResultCacheBytes: number
    scriptResultCacheEntries: number
    scriptingEngineCacheEntries: number
}

const runtimePerformanceBudgets: Record<RuntimePerformanceProfile, RuntimePerformanceBudgets> = {
    normal: {
        browserAssetDataUrlCacheBytes: 16 * 1024 * 1024,
        regexPlanCacheEntries: 32,
        scriptResultCacheBytes: 8 * 1024 * 1024,
        scriptResultCacheEntries: 1000,
        scriptingEngineCacheEntries: 16,
    },
    'low-spec': {
        browserAssetDataUrlCacheBytes: 8 * 1024 * 1024,
        regexPlanCacheEntries: 8,
        scriptResultCacheBytes: 2 * 1024 * 1024,
        scriptResultCacheEntries: 250,
        scriptingEngineCacheEntries: 4,
    },
}

type RuntimePerformanceProfileListener = (
    profile: RuntimePerformanceProfile,
    budgets: Readonly<RuntimePerformanceBudgets>,
) => void

let currentProfile: RuntimePerformanceProfile = 'normal'
const listeners = new Set<RuntimePerformanceProfileListener>()

export function getRuntimePerformanceProfile(): RuntimePerformanceProfile {
    return currentProfile
}

export function getRuntimePerformanceBudgets(): Readonly<RuntimePerformanceBudgets> {
    return runtimePerformanceBudgets[currentProfile]
}

export function setRuntimePerformanceProfile(profile: RuntimePerformanceProfile): void {
    if (profile === currentProfile) {
        return
    }

    currentProfile = profile
    const budgets = getRuntimePerformanceBudgets()
    for (const listener of listeners) {
        listener(profile, budgets)
    }
}

export function subscribeRuntimePerformanceProfile(
    listener: RuntimePerformanceProfileListener,
): () => void {
    listeners.add(listener)
    return () => listeners.delete(listener)
}
