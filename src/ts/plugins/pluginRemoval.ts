import { DBState } from '../stores.svelte'
import { clearPresetPluginProvider } from '../storage/database.svelte'
import { presetEditorDBState } from '../storage/presetEditor.svelte'
import { deletePluginDataForOwner } from './pluginDataInventory'
import { pluginProviderNames } from './pluginProviderOwners'
import { loadPlugins } from './plugins.svelte'

/**
 * Removes an installed plugin by name. With `deleteData`, the plugin's data is deleted
 * after the plugin has stopped, so nothing it saves while unloading survives the removal.
 */
export async function removeInstalledPlugin(owner: string, deleteData: boolean): Promise<void> {
    if (DBState.db.plugins.some((installed) => installed.name === owner)) {
        const names = pluginProviderNames(owner)
        // The applied preset and the preset the settings window edits stop showing it at once.
        for (const db of [DBState.db, presetEditorDBState.db]) {
            if (names.includes(db.currentPluginProvider)) db.currentPluginProvider = ''
        }
        // Presets that are not loaded keep their bodies in the store. The plugin stays installed
        // until they are cleared, so removing it again after a failure clears them again.
        await clearPresetPluginProvider(names)
        const index = DBState.db.plugins.findIndex((installed) => installed.name === owner)
        if (index !== -1) DBState.db.plugins.splice(index, 1)
    }
    await loadPlugins()
    if (deleteData) await deletePluginDataForOwner(owner)
}
