import type {
    NativeStagedPluginChoice,
    NativeStagedPluginPreview,
} from './nativeFileJobs'

/**
 * A restore that answered the plugin value pass and then lost the replacement
 * fence is cancelled, and the person starts the import over. The answers are
 * held here so the pass opens with them already filled in. The identity is the
 * preview itself, because the second attempt stages the save under a new
 * generation and carries nothing the cancelled one shared. Only what the
 * preview already names is kept, never a value.
 */

const retained = new Map<string, NativeStagedPluginChoice>()
const retainedLimit = 4

/** What a save leaves unowned, which a second read of the same file repeats. */
export function pluginValuePreviewKey(
    preview: NativeStagedPluginPreview,
): string {
    const values = preview.values
        .map((value) => [value.key, value.valueType, value.byteSize] as const)
        .map((value) => value.join('\u0000'))
        .sort()
    return JSON.stringify([values, [...preview.pluginNames].sort()])
}

function copyChoice(choice: NativeStagedPluginChoice): NativeStagedPluginChoice {
    return {
        assignments: choice.assignments.map((assignment) => ({
            owner: assignment.owner,
            keys: [...assignment.keys],
        })),
        automatic: choice.automatic,
    }
}

export function rememberPluginValueAssignment(
    preview: NativeStagedPluginPreview,
    choice: NativeStagedPluginChoice,
): void {
    const key = pluginValuePreviewKey(preview)
    retained.delete(key)
    retained.set(key, copyChoice(choice))
    while (retained.size > retainedLimit) {
        const oldest = retained.keys().next()
        if (oldest.done) break
        retained.delete(oldest.value)
    }
}

export function recallPluginValueAssignment(
    preview: NativeStagedPluginPreview,
): NativeStagedPluginChoice | null {
    const choice = retained.get(pluginValuePreviewKey(preview))
    return choice ? copyChoice(choice) : null
}

export function forgetPluginValueAssignment(
    preview: NativeStagedPluginPreview,
): void {
    retained.delete(pluginValuePreviewKey(preview))
}
