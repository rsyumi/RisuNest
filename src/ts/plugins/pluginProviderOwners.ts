// The plugin that last registered each provider name in this session.
const owners = new Map<string, string>()

export function recordPluginProvider(name: string, owner: string): void {
    owners.set(name, owner)
}

/**
 * Provider names a preset may hold for a plugin: the ones it registered, and its own name unless
 * another plugin registered that name.
 */
export function pluginProviderNames(owner: string): string[] {
    const names = [...owners].filter(([, value]) => value === owner).map(([name]) => name)
    if (!owners.has(owner)) names.push(owner)
    return names
}
