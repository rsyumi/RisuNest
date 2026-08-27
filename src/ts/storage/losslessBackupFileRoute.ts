import type { PersistentDataRuntime } from './persistentDataRuntime.svelte'
import type {
    NativeFileJobOptions,
    NativeFileJobResult,
    NativeFileJobSource,
    NativeFileRestoreJobOptions,
    NativeLosslessBackupDestination,
} from './nativeFileJobs'

type LosslessBackupRuntime = Pick<
    PersistentDataRuntime,
    | 'revision'
    | 'flushPendingData'
    | 'capturePersistentMutationToken'
    | 'acquireDestructiveReplacementFence'
>

export type LosslessBackupPlatform = 'native-desktop' | 'native-android' | 'web'

export interface LosslessBackupFileRouteDependencies {
    platform(): LosslessBackupPlatform
    runtime(): LosslessBackupRuntime
    chooseNativeImport(options: Pick<NativeFileRestoreJobOptions, 'signal' | 'onStatus'>): Promise<NativeFileJobSource | null>
    chooseDesktopExport(defaultName: string): Promise<string | null>
    runNativeRestore(
        runtime: LosslessBackupRuntime,
        source: NativeFileJobSource,
        options: NativeFileRestoreJobOptions,
    ): Promise<NativeFileJobResult>
    runNativeExport(
        runtime: LosslessBackupRuntime,
        destination: NativeLosslessBackupDestination,
        options: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
    reloadPluginsAfterRestore(): void | Promise<void>
    saveLegacyBackup(): void | Promise<void>
    loadLegacyBackup(): void
}

export interface LosslessBackupFileRouteResult {
    mode: 'native' | 'legacy'
    warningCodes: string[]
    bytes?: number
}

function defaultExportName(): string {
    return `risunest-${new Date().toISOString().replace(/[:.]/g, '-')}.risulossless`
}

export async function exportLocalBackupFromPicker(
    options: NativeFileJobOptions,
    dependencies: LosslessBackupFileRouteDependencies,
): Promise<LosslessBackupFileRouteResult | null> {
    const platform = dependencies.platform()
    if (platform === 'web') {
        await dependencies.saveLegacyBackup()
        return { mode: 'legacy', warningCodes: [] }
    }

    const name = defaultExportName()
    let destination: NativeLosslessBackupDestination
    if (platform === 'native-desktop') {
        const path = await dependencies.chooseDesktopExport(name)
        if (!path) return null
        destination = { type: 'desktopPath', path }
    }
    else {
        destination = { type: 'androidSaf', suggestedName: name }
    }

    const result = await dependencies.runNativeExport(
        dependencies.runtime(),
        destination,
        options,
    )
    return {
        mode: 'native',
        warningCodes: result.warningCodes,
        bytes: result.sourceBytes,
    }
}

export async function restoreLocalBackupFromPicker(
    options: NativeFileRestoreJobOptions,
    dependencies: LosslessBackupFileRouteDependencies,
): Promise<LosslessBackupFileRouteResult | null> {
    const platform = dependencies.platform()
    if (platform === 'web') {
        dependencies.loadLegacyBackup()
        return { mode: 'legacy', warningCodes: [] }
    }

    const source = await dependencies.chooseNativeImport(options)
    if (!source) return null

    const result = await dependencies.runNativeRestore(
        dependencies.runtime(),
        source,
        {
            ...options,
            afterRefresh: dependencies.reloadPluginsAfterRestore,
        },
    )

    return {
        mode: 'native',
        warningCodes: result.warningCodes,
        bytes: result.sourceBytes,
    }
}
