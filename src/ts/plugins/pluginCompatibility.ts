import { Mutex } from '../mutex'
export type PluginCompatibilityProfile = 'scalable-v3' | 'maximum-compatibility'

export interface PluginCompatibilityDescriptor {
    version?: 1 | 2 | '2.1' | '3.0'
    enabled?: boolean
}

export function assertPluginFullObjectCompatibility(
    profile: PluginCompatibilityProfile,
    operation: string,
): void {
    if (profile === 'maximum-compatibility') return
    throw new Error(
        `${operation} requires maximum-compatibility. Use queryCharacters, ` +
        'queryConversations, queryConversationMessages, or the explicit complete getDatabase snapshot.',
    )
}

export function runPluginFullObjectReplacement<T>(
    profile: PluginCompatibilityProfile,
    operation: string,
    affectsActiveConversation: boolean,
    replace: () => T,
    invalidateActiveConversation: () => void,
): T {
    assertPluginFullObjectCompatibility(profile, operation)
    const result = replace()
    if (affectsActiveConversation) invalidateActiveConversation()
    return result
}

export function getManualPluginInstallVersion(
    apiVersion: string,
): '2.1' | '3.0' | null {
    return apiVersion === '2.1' || apiVersion === '3.0' ? apiVersion : null
}

export interface PluginCompatibilityController {
    readonly profile: PluginCompatibilityProfile
    readonly allowsEviction: boolean
    initialize(profile: PluginCompatibilityProfile): void
    transition(next: PluginCompatibilityProfile): Promise<PluginCompatibilityProfile>
}

export interface PluginCompatibilityLifecycleDependencies {
    persistBeforeEviction(): Promise<void>
    enterMaximumCompatibility?(): Promise<void>
    setEvictionAllowed?(allowed: boolean): void
    canReleaseWorkingSet?(): boolean | Promise<boolean>
    scheduleReleaseRetry?(retry: () => void): () => void
    onReleaseRetryError?(error: unknown): void
    releaseAfterScalable?(
        isCurrent: () => boolean,
    ): void | boolean | Promise<void | boolean>
}

interface PluginLoadRequest<T> {
    nextProfile: PluginCompatibilityProfile
    pluginV2: readonly T[]
    pluginV3: readonly T[]
}

interface PluginLoadDependencies<T> {
    controller: PluginCompatibilityController
    loadV2(plugins: readonly T[], isCurrent: () => boolean): Promise<unknown>
    loadV3(plugins: readonly T[]): Promise<unknown>
}

export function selectPluginCompatibilityProfile(
    plugins: readonly PluginCompatibilityDescriptor[],
): PluginCompatibilityProfile {
    return plugins.some((plugin) => plugin.enabled === true && plugin.version === '2.1')
        ? 'maximum-compatibility'
        : 'scalable-v3'
}

export function createFullCompatibilityPersistence<T>(
    getCompatibilitySnapshot: () => T,
    replacePersistentDatabase: (database: T, reason: string) => Promise<void>,
): () => Promise<void> {
    return () =>
        replacePersistentDatabase(getCompatibilitySnapshot(), 'plugin-profile-change')
}

export function shouldProjectScalableWorkingSet(
    controller: Pick<
        PluginCompatibilityController,
        'profile' | 'allowsEviction'
    >,
    forceScalableProjection?: boolean,
): boolean {
    // A committed replacement chooses its projection from the restored plugins,
    // before the controller has switched away from the previous database's mode.
    return (
        forceScalableProjection ??
        (controller.profile === 'scalable-v3' && controller.allowsEviction)
    )
}

export function createAwaitablePluginLoaderSource(
    body: string,
    sourceLabel?: string,
): string {
    return `return (async () => {
${body}
})();${sourceLabel === undefined ? '' : `\n//# sourceURL=risu-plugin-v2/${encodeURIComponent(sourceLabel)}.js`}`
}

export async function runAwaitablePluginLoader(source: string): Promise<void> {
    await new Function(source)()
}

export interface PluginLoadReentrancyGuard {
    runEvaluation<T>(evaluate: () => Promise<T>): Promise<T>
    settle(operation: Promise<void>): Promise<void>
}

export function createPluginLoadReentrancyGuard(
    onBackgroundError: (error: unknown) => void,
): PluginLoadReentrancyGuard {
    let evaluationDepth = 0
    return {
        async runEvaluation<T>(evaluate: () => Promise<T>): Promise<T> {
            evaluationDepth++
            try {
                return await evaluate()
            } finally {
                evaluationDepth--
            }
        },
        settle(operation: Promise<void>): Promise<void> {
            if (evaluationDepth === 0) return operation
            void operation.catch(onBackgroundError)
            return Promise.resolve()
        },
    }
}

export async function runPluginUnloadCallbacks(
    callbacks: Set<() => void | Promise<void>>,
    isCurrent: () => boolean,
    reentrancy: PluginLoadReentrancyGuard,
): Promise<boolean> {
    for (const callback of [...callbacks]) {
        callbacks.delete(callback)
        await reentrancy.runEvaluation(async () => callback())
        if (!isCurrent()) return false
    }
    return isCurrent()
}

export function createPluginLoadOrchestrator<T>(dependencies: PluginLoadDependencies<T>) {
    let loadGeneration = 0
    const operationMutex = new Mutex()

    return (request: PluginLoadRequest<T>): Promise<void> => {
        const generation = ++loadGeneration
        const isCurrent = () => generation === loadGeneration
        const maximumTransition =
            request.nextProfile === 'maximum-compatibility'
                ? dependencies.controller.transition('maximum-compatibility').then(
                    (profile) => ({ status: 'fulfilled' as const, profile }),
                    (error: unknown) => ({ status: 'rejected' as const, error }),
                )
                : null

        const operation = operationMutex.runExclusive(async () => {
            let appliedMaximumProfile: PluginCompatibilityProfile | null = null
            if (maximumTransition) {
                const outcome = await maximumTransition
                if (outcome.status === 'rejected') throw outcome.error
                appliedMaximumProfile = outcome.profile
            }
            if (!isCurrent()) return

            if (
                dependencies.controller.profile === 'maximum-compatibility' &&
                request.nextProfile === 'scalable-v3'
            ) {
                await dependencies.loadV2([], isCurrent)
                if (!isCurrent()) return
                const appliedProfile = await dependencies.controller.transition(
                    request.nextProfile,
                )
                if (!isCurrent() || appliedProfile !== request.nextProfile) return
            } else {
                const appliedProfile =
                    appliedMaximumProfile ??
                    (await dependencies.controller.transition(request.nextProfile))
                if (!isCurrent() || appliedProfile !== request.nextProfile) return
                await dependencies.loadV2(request.pluginV2, isCurrent)
                if (!isCurrent()) return
            }

            await dependencies.loadV3(request.pluginV3)
        })

        return operation
    }
}

export function createPluginCompatibilityController(
    input: (() => Promise<void>) | PluginCompatibilityLifecycleDependencies,
): PluginCompatibilityController {
    const dependencies: PluginCompatibilityLifecycleDependencies =
        typeof input === 'function'
            ? { persistBeforeEviction: input }
            : input
    let profile: PluginCompatibilityProfile = 'scalable-v3'
    let transitionGeneration = 0
    let pendingPersistence: Promise<void> | null = null
    let pendingMaximum: Promise<void> | null = null
    let pendingMaximumRollback: {
        profile: PluginCompatibilityProfile
        evictionAllowed: boolean
    } | null = null
    let maximumReady = false
    let evictionAllowed = true
    let cancelReleaseRetry: (() => void) | null = null
    let releaseFailureRetryCount = 0

    const MAX_RELEASE_FAILURE_RETRIES = 3

    const cancelScheduledReleaseRetry = () => {
        const cancel = cancelReleaseRetry
        cancelReleaseRetry = null
        cancel?.()
    }
    const scheduleReleaseRetry = (bounded = false) => {
        if (cancelReleaseRetry || !dependencies.scheduleReleaseRetry) return
        if (bounded) {
            if (releaseFailureRetryCount >= MAX_RELEASE_FAILURE_RETRIES) return
            releaseFailureRetryCount++
        }
        let active = true
        let cancelSubscription = () => undefined
        cancelReleaseRetry = () => {
            if (!active) return
            active = false
            cancelSubscription()
        }
        cancelSubscription = dependencies.scheduleReleaseRetry(() => {
            queueMicrotask(() => {
                if (!active) return
                active = false
                cancelReleaseRetry = null
                void transition('scalable-v3').catch((error) =>
                    dependencies.onReleaseRetryError?.(error),
                )
            })
        })
    }
    const transition = async (
        next: PluginCompatibilityProfile,
    ): Promise<PluginCompatibilityProfile> => {
        const generation = ++transitionGeneration
        if (next === 'maximum-compatibility') {
            const previousProfile = profile
            const previousEvictionAllowed = evictionAllowed
            cancelScheduledReleaseRetry()
            evictionAllowed = false
            dependencies.setEvictionAllowed?.(false)
            if (pendingPersistence) {
                await pendingPersistence.catch(() => undefined)
            }
            if (!maximumReady) {
                let materialization = pendingMaximum
                if (!materialization) {
                    pendingMaximumRollback = {
                        profile: previousProfile,
                        evictionAllowed: previousEvictionAllowed,
                    }
                    materialization =
                        dependencies.enterMaximumCompatibility?.() ?? Promise.resolve()
                    pendingMaximum = materialization
                }
                try {
                    await materialization
                    maximumReady = true
                    profile = next
                    releaseFailureRetryCount = 0
                } catch (error) {
                    if (pendingMaximumRollback) {
                        profile = pendingMaximumRollback.profile
                        evictionAllowed = pendingMaximumRollback.evictionAllowed
                        dependencies.setEvictionAllowed?.(evictionAllowed)
                        if (profile === 'scalable-v3' && !evictionAllowed) {
                            scheduleReleaseRetry()
                        }
                    }
                    throw error
                } finally {
                    if (pendingMaximum === materialization) {
                        pendingMaximum = null
                        pendingMaximumRollback = null
                    }
                }
            }
            if (maximumReady) profile = next
            return profile
        }
        if (next === profile && evictionAllowed) return profile

        if (pendingMaximum) await pendingMaximum

        const retryingDeferredScalableRelease = next === profile && !evictionAllowed
        if (!retryingDeferredScalableRelease) {
            const previousProfile = profile
            const previousMaximumReady = maximumReady
            profile = next
            try {
                const persistence = (pendingPersistence ??=
                    dependencies.persistBeforeEviction())
                try {
                    await persistence
                } finally {
                    if (pendingPersistence === persistence) {
                        pendingPersistence = null
                    }
                }
            } catch (error) {
                if (generation === transitionGeneration) {
                    profile = previousProfile
                    maximumReady = previousMaximumReady
                }
                throw error
            }
        }
        if (generation === transitionGeneration) {
            profile = next
            maximumReady = false
            let canRelease: boolean
            try {
                canRelease = await (dependencies.canReleaseWorkingSet?.() ?? true)
            } catch (error) {
                if (generation !== transitionGeneration || profile !== next) return profile
                scheduleReleaseRetry(true)
                throw error
            }
            if (generation !== transitionGeneration) return profile
            if (!canRelease) {
                scheduleReleaseRetry()
                return profile
            }
            cancelScheduledReleaseRetry()
            const releaseIsCurrent = () =>
                generation === transitionGeneration && profile === next
            try {
                const released = await dependencies.releaseAfterScalable?.(releaseIsCurrent)
                if (!releaseIsCurrent()) return profile
                if (released === false) {
                    evictionAllowed = false
                    dependencies.setEvictionAllowed?.(false)
                    scheduleReleaseRetry(true)
                    return profile
                }
                evictionAllowed = true
                dependencies.setEvictionAllowed?.(true)
                releaseFailureRetryCount = 0
            } catch (error) {
                if (!releaseIsCurrent()) return profile
                evictionAllowed = false
                dependencies.setEvictionAllowed?.(false)
                scheduleReleaseRetry(true)
                throw error
            }
        }
        return profile
    }

    return {
        get profile() {
            return profile
        },
        get allowsEviction() {
            return evictionAllowed
        },
        initialize(next) {
            cancelScheduledReleaseRetry()
            releaseFailureRetryCount = 0
            transitionGeneration++
            profile = next
            maximumReady = next === 'maximum-compatibility'
            evictionAllowed = next === 'scalable-v3'
            dependencies.setEvictionAllowed?.(evictionAllowed)
        },
        transition,
    }
}
