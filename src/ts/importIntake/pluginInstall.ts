import type { ImportIntakeItem, ImportIntakePluginHeader, ImportIntakeRefusal } from './types'

// Contract placeholder; the plugin worker replaces this module.

/** Reads a plugin file's header, or says why it cannot be installed. */
export function inspectPluginFile(
    _name: string,
    _code: string,
    _isTypescript: boolean,
): { header: ImportIntakePluginHeader } | { refusal: ImportIntakeRefusal } {
    throw new Error('Plugin inspection is not wired yet')
}

/** Shows the plugin install dialog for one plugin item and installs it on confirmation. */
export function requestPluginInstall(_item: ImportIntakeItem): Promise<'installed' | 'cancelled' | 'failed'> {
    return Promise.reject(new Error('Plugin install is not wired yet'))
}
