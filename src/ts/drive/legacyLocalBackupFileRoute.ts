import type { NativeFileJobResult, NativeFileJobSource } from '../storage/nativeFileJobs'
import type {
    NativeBlockRestoreRuntime,
    NativeFileJobOptions,
    NativeFileRestoreJobOptions,
} from '../storage/nativeFileJobs'

interface LegacyLocalBackupExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

export interface LegacyLocalBackupFileRouteDependencies {
    runtime(): NativeBlockRestoreRuntime & LegacyLocalBackupExportRuntime
    chooseImport(): Promise<string | null>
    chooseExport(): Promise<string | null>
    runImport(
        runtime: NativeBlockRestoreRuntime,
        source: NativeFileJobSource,
        options?: NativeFileRestoreJobOptions,
    ): Promise<NativeFileJobResult>
    runExport(
        runtime: LegacyLocalBackupExportRuntime,
        destination: string,
        options?: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
    reloadPluginsAfterRestore(): void | Promise<void>
}

export async function importLegacyLocalBackupFromPicker(
    options: NativeFileRestoreJobOptions,
    dependencies: LegacyLocalBackupFileRouteDependencies,
): Promise<NativeFileJobResult | null> {
    const path = await dependencies.chooseImport()
    if (!path) return null
    return dependencies.runImport(
        dependencies.runtime(),
        { type: 'desktopPath', path },
        { ...options, afterRefresh: dependencies.reloadPluginsAfterRestore },
    )
}

export async function exportLegacyLocalBackupFromPicker(
    options: NativeFileJobOptions,
    dependencies: LegacyLocalBackupFileRouteDependencies,
): Promise<NativeFileJobResult | null> {
    const path = await dependencies.chooseExport()
    if (!path) return null
    return dependencies.runExport(dependencies.runtime(), path, {
        ...options,
        signal: options.signal,
    })
}
