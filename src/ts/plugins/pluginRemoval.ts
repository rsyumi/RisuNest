import { DBState } from '../stores.svelte'
import { presetEditorDBState } from '../storage/presetEditor.svelte'
import { deletePluginDataForOwner } from './pluginDataInventory'
import { loadPlugins } from './plugins.svelte'

/**
 * Removes an installed plugin by name. With `deleteData`, the plugin's data is deleted
 * after the plugin has stopped, so nothing it saves while unloading survives the removal.
 */
export async function removeInstalledPlugin(owner: string, deleteData: boolean): Promise<void> {
    const index = DBState.db.plugins.findIndex((installed) => installed.name === owner)
    if (index !== -1) {
        // The applied preset, the preset the settings window edits and every stored preset
        // stop naming the removed provider.
        if (DBState.db.currentPluginProvider === owner) DBState.db.currentPluginProvider = ''
        if (presetEditorDBState.db.currentPluginProvider === owner) presetEditorDBState.db.currentPluginProvider = ''
        for (const preset of DBState.db.botPresets ?? []) {
            if (preset?.currentPluginProvider === owner) preset.currentPluginProvider = ''
        }
        DBState.db.plugins.splice(index, 1)
    }
    await loadPlugins()
    if (deleteData) await deletePluginDataForOwner(owner)
}
