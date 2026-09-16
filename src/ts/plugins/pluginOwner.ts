/**
 * Every stored plugin value belongs to the plugin whose `//@name` banner matches
 * its owner. Values that arrived from an upstream RisuAI save without an
 * ownership sidecar carry the sentinel below. The banner is parsed line by line,
 * so a plugin name can never contain NUL and plugin code cannot forge it.
 */
export const UNOWNED_PLUGIN_OWNER = '\u0000unowned'

export function isUnownedPluginOwner(owner: string): boolean {
    return owner === UNOWNED_PLUGIN_OWNER
}

export function isValidPluginOwner(owner: string): boolean {
    if (owner === UNOWNED_PLUGIN_OWNER) return true
    return owner.length > 0 && owner.length <= 512 && !owner.includes('\u0000')
}

/** Ownership as an upstream save carries it beside the flattened values. */
export interface PluginStorageMetaEntry {
    plugin: string
    updatedAt: number
}

export type PluginStorageMeta = Record<string, PluginStorageMetaEntry>

export function readPluginStorageMetaOwner(
    meta: PluginStorageMeta | undefined,
    key: string,
): string {
    const entry = meta?.[key]
    if (!entry || typeof entry.plugin !== 'string') return UNOWNED_PLUGIN_OWNER
    if (isUnownedPluginOwner(entry.plugin) || !isValidPluginOwner(entry.plugin)) {
        return UNOWNED_PLUGIN_OWNER
    }
    return entry.plugin
}
