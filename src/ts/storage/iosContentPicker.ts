import { readFile } from '@tauri-apps/plugin-fs'
import { importContentBatch } from './contentImportBatch'
import { discardIOSFile, pickIOSFiles, type IOSPickedFile } from './iosFiles'
import { runSharedNativeFileOperation, type SharedNativeFileOperationContext } from './nativeFileJobManager'
import { NativeFileJobError } from './nativeFileJobs'

// Matches the existing Android metadata-only compatibility reader.
const MAX_METADATA_BYTES = 128 * 1024 * 1024

function discardPicked(picked: IOSPickedFile): Promise<void> {
    return discardIOSFile(picked.path).catch(() => {
        console.warn('Could not remove the consumed iOS import staging file')
    })
}

export function importIOSContentFromPicker(
    destination: 'character' | 'module',
): Promise<string | null> {
    return runSharedNativeFileOperation(
        'import',
        'content-picker',
        async context => {
            const picked = await pickIOSFiles(context.signal)
            if (!picked) return null
            const failures = picked.failures.map(failure => ({
                name: failure.name,
                error: new NativeFileJobError('source-copy-failed', `${failure.name}: ${failure.message}`),
            }))
            // A single file that could not be copied keeps the single-file failure.
            if (!picked.files.length && failures.length === 1) throw failures[0].error
            return importContentBatch(
                context,
                picked.files,
                file => ({ name: file.name, bytes: file.bytes }),
                async file => {
                    try { return await importPickedContent(file, destination, context) }
                    finally { await discardPicked(file) }
                },
                { failures, releaseUnstarted: discardPicked },
            )
        },
        { presentation: 'dialog', format: 'content' },
    )
}

async function importPickedContent(
    picked: IOSPickedFile,
    destination: 'character' | 'module',
    context: SharedNativeFileOperationContext,
): Promise<string | null> {
    context.signal.throwIfAborted()
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
            return await importModuleData({ name: picked.name, data }) ? 'module' : null
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
}
