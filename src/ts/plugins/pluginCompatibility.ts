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

export function selectPluginCompatibilityProfile(
    plugins: readonly PluginCompatibilityDescriptor[],
): PluginCompatibilityProfile {
    return plugins.some((plugin) => plugin.enabled === true && plugin.version === '2.1')
        ? 'maximum-compatibility'
        : 'scalable-v3'
}

export function createPluginCompatibilityController(
    flushBeforeEviction: () => Promise<void>,
): PluginCompatibilityController {
    let profile: PluginCompatibilityProfile = 'scalable-v3'

    return {
        get profile() {
            return profile
        },
        get allowsEviction() {
            return profile === 'scalable-v3'
        },
        async transition(next) {
            if (next === profile) return profile
            if (next === 'maximum-compatibility') {
                profile = next
                return profile
            }
            await flushBeforeEviction()
            profile = next
            return profile
        },
    }
}
