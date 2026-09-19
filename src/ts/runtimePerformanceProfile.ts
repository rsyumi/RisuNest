export type RuntimePerformanceProfile = 'normal' | 'low-spec'

export interface RuntimePerformanceBudgets {
    inlayAnimationDecodeBytes: number
    browserAssetDataUrlCacheBytes: number
    providerImageCacheBytes: number
    hypaCacheBatchEntries: number
    localEmbeddingBatchEntries: number
    chatMountedMessageBudget: number
    regexPlanCacheEntries: number
    scriptResultCacheBytes: number
    scriptResultCacheEntries: number
    scriptingEngineCacheEntries: number
}

const runtimePerformanceBudgets: Record<RuntimePerformanceProfile, RuntimePerformanceBudgets> = {
    normal: {
        inlayAnimationDecodeBytes: 256 * 1024 * 1024,
        browserAssetDataUrlCacheBytes: 16 * 1024 * 1024,
        providerImageCacheBytes: 16 * 1024 * 1024,
        hypaCacheBatchEntries: 1024,
        localEmbeddingBatchEntries: Number.POSITIVE_INFINITY,
        chatMountedMessageBudget: 64,
        regexPlanCacheEntries: 32,
        scriptResultCacheBytes: 8 * 1024 * 1024,
        scriptResultCacheEntries: 1000,
        scriptingEngineCacheEntries: 16,
    },
    'low-spec': {
        inlayAnimationDecodeBytes: 64 * 1024 * 1024,
        browserAssetDataUrlCacheBytes: 8 * 1024 * 1024,
        providerImageCacheBytes: 4 * 1024 * 1024,
        hypaCacheBatchEntries: 64,
        localEmbeddingBatchEntries: 8,
        chatMountedMessageBudget: 40,
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

const configuredProfile = import.meta.env.VITE_RUNTIME_PERFORMANCE_PROFILE
let currentProfile: RuntimePerformanceProfile = configuredProfile === 'low-spec' ? 'low-spec' : 'normal'
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
