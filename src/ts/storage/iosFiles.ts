import { invoke } from '@tauri-apps/api/core'
import { appDataDir, join } from '@tauri-apps/api/path'
import { mkdir, remove, writeFile } from '@tauri-apps/plugin-fs'

export interface IOSPickedFile {
    path: string
    name: string
    bytes: number
}
const cancelled = () =>
    new DOMException('File operation cancelled', 'AbortError')

export async function pickIOSFile(
    signal?: AbortSignal,
): Promise<IOSPickedFile | null> {
    if (signal?.aborted) throw cancelled()
    const result = await invoke<IOSPickedFile & { cancelled: boolean }>(
        'plugin:ios-native|pick_file',
    )
    if (result.cancelled) return null
    if (signal?.aborted) {
        await discardIOSFile(result.path)
        throw cancelled()
    }
    return result
}

export const discardIOSFile = (path: string) =>
    invoke<void>('plugin:ios-native|discard_file', { path })

export async function exportIOSFile(request: {
    sourcePath: string
    suggestedName: string
    signal?: AbortSignal
    requestId?: string
}): Promise<{ bytes: number; warningCodes?: string[] }> {
    if (request.signal?.aborted) throw cancelled()
    const result = await invoke<{ cancelled: boolean; bytes: number }>(
        'plugin:ios-native|export_file',
        {
            sourcePath: request.sourcePath,
            suggestedName: request.suggestedName,
            requestId: request.requestId ?? crypto.randomUUID(),
        },
    )
    if (result.cancelled) throw cancelled()
    // Publication has completed. A late abort must not turn a published file into a reported failure.
    return { bytes: result.bytes }
}

export async function getIOSPublication(
    requestId: string,
): Promise<{ bytes: number } | null> {
    const result = await invoke<{ state: string; bytes?: number }>(
        'plugin:ios-native|publication',
        { id: requestId },
    )
    return result.state === 'succeeded' && typeof result.bytes === 'number'
        ? { bytes: result.bytes }
        : null
}

export async function downloadIOSFile(
    name: string,
    bytes: Uint8Array,
): Promise<boolean> {
    const folder = await join(
        await appDataDir(),
        'ios-file-staging',
        crypto.randomUUID(),
    )
    const path = await join(folder, 'export.bin')
    await mkdir(folder, { recursive: true })
    try {
        await writeFile(path, bytes)
        try {
            await exportIOSFile({ sourcePath: path, suggestedName: name })
            return true
        } catch (error) {
            if (error instanceof DOMException && error.name === 'AbortError')
                return false
            throw error
        }
    } finally {
        await remove(folder, { recursive: true })
    }
}
