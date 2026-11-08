import { invoke } from '@tauri-apps/api/core'
import {
    AndroidSafSourceError,
    pickAndroidContentSources,
    type AndroidSafJavascriptBridge,
    type AndroidSpoolFailure,
    type AndroidSpoolReady,
} from './androidSafBridge'
import { importContentBatch } from './contentImportBatch'
import {
    runExternalAndroidNativeFileOperation,
    runSharedNativeFileOperation,
    type SharedNativeFileOperationContext,
} from './nativeFileJobManager'
import { syntheticNativeFileJobStatus } from './nativeFileJobs'

export function importAndroidContentFromPicker(
    destination: 'character' | 'module',
): Promise<string | null> {
    return runSharedNativeFileOperation(
        'import',
        'content-picker',
        async (context) => {
            const picked = await pickAndroidContentSources({
                destination,
                signal: context.signal,
                onProgress: (progress) => {
                    // Copying starts once the picker returns, so the copy is the first protected work.
                    context.protect()
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
                    )
                },
            })
            if (!picked) return null
            context.protect()
            return importContentBatch(
                context,
                picked.sources,
                (source) => ({ name: source.displayName, bytes: source.bytes }),
                (source) => consumeAndroidContentSpool(
                    { ...source, importDestination: destination },
                    destination,
                    context,
                ),
                {
                    failures: spoolFailures(picked.failures),
                    releaseUnstarted: (source) => discardContentSpool(source.token),
                },
            )
        },
        { presentation: 'dialog', format: 'content', protectAfterPicker: true },
    )
}

function spoolFailures(failures: readonly AndroidSpoolFailure[]) {
    return failures.map((failure) => ({
        name: failure.displayName,
        error: new AndroidSafSourceError(failure.code, `${failure.displayName}: ${failure.code}`),
    }))
}

/** The JSON converter destination of an opened spool, or null when the native classifier decides. */
export function androidSpoolReplayDestination(
    source: AndroidSpoolReady,
): 'character' | 'module' | 'auto' | null {
    return source.importDestination
        ?? (/\.lorebook$/i.test(source.displayName) ? 'module'
            : /\.json$/i.test(source.displayName) ? 'auto' : null)
}

/**
 * Imports several shared or opened content spools in one operation, after any operation already
 * running. Spools the JSON converters do not own go through `importPrepared`.
 */
export function importOpenedAndroidContentSpools(
    sources: readonly AndroidSpoolReady[],
    failures: readonly AndroidSpoolFailure[],
    importPrepared: (source: AndroidSpoolReady, context: SharedNativeFileOperationContext) => Promise<string | null>,
): Promise<string | null> {
    return runExternalAndroidNativeFileOperation(
        'import',
        (context) => importContentBatch(
            context,
            sources,
            (source) => ({ name: source.displayName, bytes: source.bytes }),
            (source) => {
                const destination = androidSpoolReplayDestination(source)
                return destination
                    ? consumeAndroidContentSpool(source, destination, context)
                    : importPrepared(source, context)
            },
            {
                failures: spoolFailures(failures),
                releaseUnstarted: (source) => discardContentSpool(source.token),
            },
        ),
        { presentation: 'dialog', format: 'content' },
    )
}

async function discardContentSpool(token: string): Promise<void> {
    const bridge = (
        window as Window & {
            RisuSafBridge?: AndroidSafJavascriptBridge
        }
    ).RisuSafBridge
    await bridge?.discardSource?.(token)
}

export function importReplayedAndroidContentSpool(
    source: AndroidSpoolReady,
    destination: 'character' | 'module' | 'auto',
): Promise<string | null> {
    return runSharedNativeFileOperation(
        'import',
        `content-spool:${source.token}`,
        (context) => {
            context.setSource({ name: source.displayName, bytes: source.bytes })
            return consumeAndroidContentSpool(source, destination, context)
        },
        { presentation: 'dialog', format: 'content' },
    )
}

async function consumeAndroidContentSpool(
    source: AndroidSpoolReady,
    destination: 'character' | 'module' | 'auto',
    context: SharedNativeFileOperationContext,
): Promise<string | null> {
    const displayName = source.displayName
    const nativeSource = { type: 'androidSpool' as const, token: source.token }
    try {
        if (/\.(json|lorebook)$/i.test(displayName)) {
            // JSON-only formats retain the upstream converters; binary content never
            // makes this IPC round trip. The native reader enforces a 128 MiB limit
            // and returns the UTF-8 bytes, which Android streams instead of evaluating.
            const response = await invoke<ArrayBuffer | number[]>(
                'native_content_source_metadata',
                { token: source.token },
            )
            context.signal.throwIfAborted()
            const data = response instanceof ArrayBuffer
                ? new Uint8Array(response)
                : Uint8Array.from(response)
            const parsedJson = destination === 'auto'
                ? { value: JSON.parse(new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(data)) }
                : undefined
            const parsed = parsedJson?.value
            const moduleJson = parsed && typeof parsed === 'object' && (
                parsed.type === 'risuModule'
                || (parsed.type === 'risu' && Array.isArray(parsed.data))
                || !!parsed.entries
                || (parsed.type === 'regex' && !!parsed.data)
            )
            if (destination === 'module' || moduleJson) {
                const { importModuleData } = await import('../process/modules')
                return await importModuleData({ name: displayName, data, ...(parsedJson && { parsedJson }) }) ? 'module' : null
            }
            const { importCharacterProcess } = await import('../characterCards')
            const { getDatabase } = await import('./database.svelte')
            const index = await importCharacterProcess({
                name: displayName,
                data,
                ...(parsedJson && { parsedJson }),
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
                      { source: nativeSource, displayName },
                      options,
                  )
                : await (
                      await import('../characterCards')
                  ).importPreparedNativeCharacterContent(
                      { source: nativeSource, displayName },
                      options,
                  )
        return result.kind === 'imported' ? result.value : null
    } finally {
        // A native job consumes the spool. Metadata-only imports release their ready spool here.
        if (/\.(json|lorebook)$/i.test(displayName)) await discardContentSpool(source.token)
    }
}
