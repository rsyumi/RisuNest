import { writable } from 'svelte/store'
import { settingsOpen, SettingsMenuIndex } from '../stores.svelte'
import { openRisuNestSettingsTab } from './risuNestSettingsTabs'

/**
 * The external storage connection a notice asked to sign in again. The sync tab's external storage
 * section opens that connection's sign-in form and clears the request.
 */
export const externalRenewalRequest = writable<string | null>(null)

/** An external storage sign-in form is open. The notices that offer it cover the top of the settings page, so
 * they step aside meanwhile and return if the form closes with sync still stopped. */
export const externalRenewalOpen = writable(false)

/** Opens the sync tab with the sign-in form of this connection. */
export function openExternalRenewal(connectionId: string): void {
    externalRenewalRequest.set(connectionId)
    openRisuNestSettingsTab('sync')
    SettingsMenuIndex.set(17)
    settingsOpen.set(true)
}
