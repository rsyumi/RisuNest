export type PluginCompatibilityProfile = 'scalable-v3' | 'maximum-compatibility'

export interface PluginCompatibilityDescriptor {
    version?: 1 | 2 | '2.1' | '3.0'
    enabled?: boolean
}

export interface PluginCompatibilityController {
    readonly profile: PluginCompatibilityProfile
    readonly allowsEviction: boolean
    transition(next: PluginCompatibilityProfile): Promise<PluginCompatibilityProfile>
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

export function createPluginLoadOrchestrator<T>(dependencies: PluginLoadDependencies<T>) {
    let loadGeneration = 0
    let operationTail = Promise.resolve()

    return (request: PluginLoadRequest<T>): Promise<void> => {
        const generation = ++loadGeneration
        const isCurrent = () => generation === loadGeneration
        const maximumTransition =
            request.nextProfile === 'maximum-compatibility'
                ? dependencies.controller.transition('maximum-compatibility')
                : null

        const operation = operationTail.then(async () => {
            let appliedMaximumProfile: PluginCompatibilityProfile | null = null
            if (maximumTransition) {
                appliedMaximumProfile = await maximumTransition
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

        operationTail = operation.then(
            () => undefined,
            () => undefined,
        )
        return operation
    }
}

export function createPluginCompatibilityController(
    persistBeforeEviction: () => Promise<void>,
): PluginCompatibilityController {
    let profile: PluginCompatibilityProfile = 'scalable-v3'
    let transitionGeneration = 0
    let pendingPersistence: Promise<void> | null = null

    return {
        get profile() {
            return profile
        },
        get allowsEviction() {
            return profile === 'scalable-v3'
        },
        async transition(next) {
            const generation = ++transitionGeneration
            if (next === 'maximum-compatibility') {
                profile = next
                if (pendingPersistence) {
                    await pendingPersistence.catch(() => undefined)
                }
                return profile
            }
            if (next === profile) return profile

            const persistence = (pendingPersistence ??= persistBeforeEviction())
            try {
                await persistence
            } finally {
                if (pendingPersistence === persistence) {
                    pendingPersistence = null
                }
            }
            if (generation === transitionGeneration) {
                profile = next
            }
            return profile
        },
    }
}
