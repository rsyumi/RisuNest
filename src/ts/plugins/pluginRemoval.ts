import { DBState } from '../stores.svelte'
import { clearPresetPluginProvider } from '../storage/database.svelte'
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
        // The applied preset and the preset the settings window edits stop showing it at once.
        if (DBState.db.currentPluginProvider === owner) DBState.db.currentPluginProvider = ''
        if (presetEditorDBState.db.currentPluginProvider === owner) presetEditorDBState.db.currentPluginProvider = ''
        DBState.db.plugins.splice(index, 1)
    }
    await loadPlugins()
    if (deleteData) await deletePluginDataForOwner(owner)
    // Presets that are not loaded keep their bodies in the store.
    if (index !== -1) await clearPresetPluginProvider(owner)
}
