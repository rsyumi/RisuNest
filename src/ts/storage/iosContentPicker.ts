import { readFile } from '@tauri-apps/plugin-fs'
import { discardIOSFile, pickIOSFile } from './iosFiles'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import { NativeFileJobError } from './nativeFileJobs'

// Matches the existing Android metadata-only compatibility reader.
const MAX_METADATA_BYTES = 128 * 1024 * 1024

export function importIOSContentFromPicker(
    destination: 'character' | 'module',
): Promise<string | null> {
    return runSharedNativeFileOperation(
        'import',
        'content-picker',
        async context => {
            const picked = await pickIOSFile(context.signal)
            if (!picked) return null
            try {
                context.signal.throwIfAborted()
                context.setSource({ name: picked.name, bytes: picked.bytes })
                if (/\.(json|lorebook)$/i.test(picked.name)) {
                    if (!Number.isSafeInteger(picked.bytes) || picked.bytes < 0 || picked.bytes > MAX_METADATA_BYTES) {
                        throw new NativeFileJobError('source-too-large', 'Content metadata exceeds the compatibility reader limit')
                    }
                    const data = await readFile(picked.path)
                    context.signal.throwIfAborted()
                    if (data.byteLength > MAX_METADATA_BYTES) {
                        throw new NativeFileJobError('source-too-large', 'Content metadata exceeds the compatibility reader limit')
                    }
                    if (destination === 'module') {
                        const { importModuleData } = await import('../process/modules')
                        const result = await importModuleData({ name: picked.name, data })
                        return result === false ? null : 'module'
                    }
                    const { importCharacterProcess } = await import('../characterCards')
                    const index = await importCharacterProcess({ name: picked.name, data })
                    const { getDatabase } = await import('./database.svelte')
                    return typeof index === 'number'
                        ? getDatabase().characters[index]?.chaId ?? null
                        : null
                }
                const input = {
                    source: { type: 'desktopPath' as const, path: picked.path },
                    displayName: picked.name,
                }
                const options = { signal: context.signal, onStatus: context.onStatus }
                const result = destination === 'module' || /\.risum$/i.test(picked.name)
                    ? await (await import('../process/modules')).importPreparedNativeModuleContent(input, options)
                    : await (await import('../characterCards')).importPreparedNativeCharacterContent(input, options)
                return result.kind === 'imported' ? result.value : null
            } finally {
                await discardIOSFile(picked.path).catch(() => {
                    console.warn('Could not remove the consumed iOS import staging file')
                })
            }
        },
        { presentation: 'dialog', format: 'content' },
    )
}
