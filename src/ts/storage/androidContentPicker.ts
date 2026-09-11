import { invoke } from '@tauri-apps/api/core'
import {
    pickAndroidContentSource,
    type AndroidSafJavascriptBridge,
} from './androidSafBridge'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import { syntheticNativeFileJobStatus } from './nativeFileJobs'

export function importAndroidContentFromPicker(
    destination: 'character' | 'module',
): Promise<string | null> {
    return runSharedNativeFileOperation(
        'import',
        'content-picker',
        async (context) => {
            let displayName = ''
            const source = await pickAndroidContentSource({
                signal: context.signal,
                onSource: (file) => {
                    displayName = file.displayName
                    context.setSource({ name: displayName, bytes: file.bytes })
                },
                onProgress: (progress) =>
                    context.onStatus(
                        syntheticNativeFileJobStatus(
                            { kind: 'prepare-content-import' },
                            'copying-source',
                            {
                                stageUnit: 'bytes',
                                stageCompleted: progress.copiedBytes,
                                stageTotal: progress.totalBytes ?? undefined,
                            },
                        ),
                    ),
            })
            if (!source || source.type !== 'androidSpool') return null
            try {
                if (/\.(json|lorebook)$/i.test(displayName)) {
                    // JSON-only formats retain the upstream converters; binary content never
                    // makes this IPC round trip. The native reader enforces a 128 MiB limit.
                    const text = await invoke<string>(
                        'native_content_source_metadata',
                        { token: source.token },
                    )
                    context.signal.throwIfAborted()
                    const data = new TextEncoder().encode(text)
                    if (destination === 'module') {
                        const { importModuleData } = await import(
                            '../process/modules'
                        )
                        await importModuleData({ name: displayName, data })
                        return 'module'
                    }
                    const { importCharacterProcess } = await import(
                        '../characterCards'
                    )
                    const { getDatabase } = await import('./database.svelte')
                    const index = await importCharacterProcess({
                        name: displayName,
                        data,
                    })
                    return typeof index === 'number'
                        ? (getDatabase().characters[index]?.chaId ?? null)
                        : null
                }
                const options = {
                    signal: context.signal,
                    onStatus: context.onStatus,
                }
                const result =
                    destination === 'module' || /\.risum$/i.test(displayName)
                        ? await (
                              await import('../process/modules')
                          ).importPreparedNativeModuleContent(
                              { source, displayName },
                              options,
                          )
                        : await (
                              await import('../characterCards')
                          ).importPreparedNativeCharacterContent(
                              { source, displayName },
                              options,
                          )
                return result.kind === 'imported' ? result.value : null
            } finally {
                // A native job consumes the spool. Metadata-only imports release their ready spool here.
                if (/\.(json|lorebook)$/i.test(displayName)) {
                    const bridge = (
                        window as Window & {
                            RisuSafBridge?: AndroidSafJavascriptBridge
                        }
                    ).RisuSafBridge
                    bridge?.discardSource?.(source.token)
                }
            }
        },
        { presentation: 'dialog', format: 'content' },
    )
}
