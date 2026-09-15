import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'

export interface NativeAccountCredentialVault {
    read(): Promise<unknown | null>
    write(credential: unknown): Promise<void>
    clear(): Promise<void>
}

function requireTauri(): void {
    if (!isTauri) throw new Error('The account credential vault requires Tauri')
}

/** The token lives in the OS vault, so it stays out of every stored file. */
export function createNativeAccountCredentialVault(): NativeAccountCredentialVault {
    return {
        async read(): Promise<unknown | null> {
            requireTauri()
            return invoke<unknown | null>('account_credential_read')
        },
        async write(credential: unknown): Promise<void> {
            requireTauri()
            await invoke('account_credential_write', { credential })
        },
        async clear(): Promise<void> {
            requireTauri()
            await invoke('account_credential_clear')
        },
    }
}
