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
import type { NativeFileOperationSource } from '../storage/nativeFileJobManager'

interface LegacyLocalBackupExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

export interface LegacyLocalBackupImportOptions extends NativeFileRestoreJobOptions {
    /** Receives the picked file's name and size for the progress dialog. */
    onSource?(source: NativeFileOperationSource): void
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
