import { invoke } from '@tauri-apps/api/core'
import type { DialogFilter } from '@tauri-apps/plugin-dialog'
import { isTauriAndroid, isTauriIOS } from '../platform'

/** What a native export command reports: the handoff it wrote on Android and iOS, or no path on desktop. */
export interface NativeExportResult {
    path?: string | null
    bytes: number
}

/**
 * Saves a file that native code writes, so its bytes never cross the WebView bridge. On desktop the
 * command writes to the path the user picked; on Android and iOS it writes an app-owned handoff that
 * the platform copies to the user's choice. Resolves false when the user cancels the picker.
 */
export async function saveNativeExportFile(
    name: string,
    filters: DialogFilter[],
    write: (destination?: string) => Promise<NativeExportResult>,
): Promise<boolean> {
    if (!isTauriAndroid && !isTauriIOS) {
        const { save } = await import('@tauri-apps/plugin-dialog')
        const destination = await save({ defaultPath: name, filters })
        if (!destination) return false
        await write(destination)
        return true
    }
    const android = isTauriAndroid ? await import('./androidSafBridge') : undefined
    let reservation
    try { reservation = await android?.reserveAndroidSafDestination(name, 'download') }
    catch (error) {
        if ((error as { name?: string })?.name === 'AbortError') return false
        throw error
    }
    let exported: NativeExportResult | undefined
    try {
        const publish = async () => {
            exported = await write()
            if (!exported.path) throw new Error('native-export-handoff-missing')
            const published = isTauriAndroid
                ? await android!.copyNativeExportToAndroidSaf({ sourcePath: exported.path, suggestedName: name, reservation })
                : await (await import('./iosFiles')).exportIOSFile({ sourcePath: exported.path, suggestedName: name })
            if (published.bytes !== exported.bytes) throw new Error('native-export-incomplete')
            return true
        }
        return await (android ? android.withAndroidSafDestination(reservation, publish) : publish())
    } catch (error) {
        if ((error as { name?: string })?.name === 'AbortError') return false
        throw error
    } finally {
        if (exported?.path) await invoke('native_download_handoff_cleanup', { path: exported.path })
    }
}
