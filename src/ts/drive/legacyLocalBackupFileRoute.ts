import type {
    NativeFileJobResult,
    NativeFileJobSource,
    NativeLegacyLocalBackupDestination,
} from '../storage/nativeFileJobs'
import type {
    NativeBlockRestoreRuntime,
    NativeFileJobOptions,
    NativeFileRestoreJobOptions,
} from '../storage/nativeFileJobs'
import type {
    NativeFileOperationSource,
    SharedNativeFileOperationContext,
} from '../storage/nativeFileJobManager'

interface LegacyLocalBackupExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

export interface LegacyLocalBackupImportOptions extends NativeFileRestoreJobOptions {
    /** Receives the picked file's name and size for the progress dialog. */
    onSource?(source: NativeFileOperationSource): void
}

/** What the WebView importer needs from the shared operation when the native job hands over. */
export type LegacyLocalBackupFallbackContext = Pick<
    SharedNativeFileOperationContext,
    'signal' | 'onStatus' | 'setSource' | 'setPartialWritesPossible'
>

/**
 * Native legacy backup jobs give up on inputs they cannot read (no native
 * capability, or a format only the JavaScript importer understands). Those
 * are the only failures that continue in the WebView instead of failing.
 */
export function isNativeLegacyBackupFallback(error: unknown): boolean {
    if (typeof error !== 'object' || error === null || !('code' in error)) return false
    return error.code === 'capability-unavailable' || error.code === 'unsupported-format'
}

export interface LegacyLocalBackupFileRouteDependencies {
    runtime(): NativeBlockRestoreRuntime & LegacyLocalBackupExportRuntime
    chooseImport(options: LegacyLocalBackupImportOptions): Promise<NativeFileJobSource | null>
    chooseExport(options: NativeFileJobOptions): Promise<NativeLegacyLocalBackupDestination | null>
    runImport(
        runtime: NativeBlockRestoreRuntime,
        source: NativeFileJobSource,
        options?: NativeFileRestoreJobOptions,
    ): Promise<NativeFileJobResult>
    runExport(
        runtime: LegacyLocalBackupExportRuntime,
        destination: NativeLegacyLocalBackupDestination,
        options?: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
    reloadPluginsAfterRestore(): void | Promise<void>
}

export async function importLegacyLocalBackupFromPicker(
    options: LegacyLocalBackupImportOptions,
    dependencies: LegacyLocalBackupFileRouteDependencies,
): Promise<NativeFileJobResult | null> {
    const source = await dependencies.chooseImport(options)
    if (!source) return null
    const { onSource: _onSource, ...jobOptions } = options
    return dependencies.runImport(
        dependencies.runtime(),
        source,
        { ...jobOptions, afterRefresh: dependencies.reloadPluginsAfterRestore },
    )
}

export async function exportLegacyLocalBackupFromPicker(
    options: NativeFileJobOptions,
    dependencies: LegacyLocalBackupFileRouteDependencies,
): Promise<NativeFileJobResult | null> {
    const destination = await dependencies.chooseExport(options)
    if (!destination) return null
    return dependencies.runExport(dependencies.runtime(), destination, {
        ...options,
        signal: options.signal,
    })
}
