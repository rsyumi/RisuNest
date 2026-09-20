import { writable } from 'svelte/store'

/** The tabs of the RisuNest settings page, in display order. */
export type RisuNestSettingsTab = 'settings' | 'storage' | 'sync' | 'plugin-data'

export const RISUNEST_SETTINGS_TABS: readonly RisuNestSettingsTab[] = [
    'settings',
    'storage',
    'sync',
    'plugin-data',
]

/**
 * A tab another screen asked the page to open, such as the data check after a storage failure or
 * the sync server from a deep link. The page reads it when it mounts or while it is open, then
 * clears it, so the next plain visit starts on the first tab again.
 */
export const risuNestSettingsTabRequest = writable<RisuNestSettingsTab | null>(null)

export function openRisuNestSettingsTab(tab: RisuNestSettingsTab): void {
    risuNestSettingsTabRequest.set(tab)
}
