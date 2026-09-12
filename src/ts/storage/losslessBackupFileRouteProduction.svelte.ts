import { open, save } from '@tauri-apps/plugin-dialog'

import { LoadLocalBackup, SaveLocalBackup } from '../drive/backuplocal'
import { isTauri, isTauriAndroid, isTauriDesktop } from '../platform'
import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import { pickAndroidLosslessBackupSource } from './androidSafBridge'
import {
    exportLocalBackupFromPicker,
    restoreLocalBackupFromPicker,
    type LosslessBackupFileRouteDependencies,
    type LosslessBackupRestoreOptions,
} from './losslessBackupFileRoute'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    runNativeLosslessBackupExport,
    runNativeLosslessBackupRestore,
    syntheticNativeFileJobStatus,
    type NativeFileJobOptions,
    type NativeFileJobSource,
} from './nativeFileJobs'
import { describeDesktopSource } from './nativeFileSourceInfo'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

const productionDependencies: LosslessBackupFileRouteDependencies = {
    platform: () => isTauriDesktop
        ? 'native-desktop'
        : isTauriAndroid
            ? 'native-android'
            : 'web',
    runtime: getPersistentDataRuntime,
    chooseNativeImport: async ({ signal, onStatus, onSource }) => {
        if (isTauriAndroid) {
            return pickAndroidLosslessBackupSource({
                signal,
                onSource: (source) => onSource?.({ name: source.displayName, bytes: source.bytes }),
                onProgress: (progress) => onStatus?.(syntheticNativeFileJobStatus(
                    { jobId: progress.requestId, kind: 'restore-lossless-backup' },
                    'copying-source',
                    {
                        stageUnit: 'bytes',
                        stageCompleted: progress.copiedBytes,
                        ...(progress.totalBytes === null ? {} : { stageTotal: progress.totalBytes }),
                        progress: {
                            completedBytes: progress.copiedBytes,
                            ...(progress.totalBytes === null ? {} : { totalBytes: progress.totalBytes }),
                            completedItems: 0,
                            totalItems: 1,
                        },
                    },
                )),
            })
        }
        const selected = await open({
            multiple: false,
            directory: false,
            filters: [{
                name: 'RisuNest Lossless Backup',
                extensions: ['risulossless'],
            }],
        })
        if (typeof selected !== 'string') return null
        onSource?.(await describeDesktopSource(selected))
        return { type: 'desktopPath', path: selected }
    },
    chooseDesktopExport: async (name) => save({
        defaultPath: name,
        filters: [{
            name: 'RisuNest Lossless Backup',
            extensions: ['risulossless'],
        }],
    }),
    runNativeRestore: runNativeLosslessBackupRestore,
    runNativeExport: runNativeLosslessBackupExport,
    reloadPluginsAfterRestore: loadPluginsAfterAuthoritativeRestore,
    saveLegacyBackup: async () => { await SaveLocalBackup() },
    loadLegacyBackup: LoadLocalBackup,
}

export function exportLocalBackupFromSystemPicker(
    options: NativeFileJobOptions = {},
) {
    if (!isTauri) return exportLocalBackupFromPicker(options, productionDependencies)
    return runSharedNativeFileOperation('export', 'lossless-backup-export', ({ signal, onStatus }) =>
        exportLocalBackupFromPicker({
            ...options,
            signal,
            onStatus: (status) => {
                onStatus(status)
                options.onStatus?.(status)
            },
        }, productionDependencies))
}

export function restoreLocalBackupFromSystemPicker(options: LosslessBackupRestoreOptions = {}) {
    return restoreWithDependencies(options, productionDependencies)
}

export function restoreLocalBackupFromNativeSource(
    source: NativeFileJobSource,
    options: LosslessBackupRestoreOptions = {},
) {
    return restoreWithDependencies(options, {
        ...productionDependencies,
        chooseNativeImport: async () => source,
    })
}

function restoreWithDependencies(
    options: LosslessBackupRestoreOptions,
    dependencies: LosslessBackupFileRouteDependencies,
) {
    if (!isTauri) return restoreLocalBackupFromPicker(options, dependencies)
    return runSharedNativeFileOperation(
        'import',
        'lossless-backup-import',
        ({ signal, onStatus, setBlocking, setSource }) =>
            restoreLocalBackupFromPicker(
                {
                    ...options,
                    signal,
                    onStatus: (status) => {
                        onStatus(status)
                        options.onStatus?.(status)
                    },
                    onBlockingChange: (blocking) => {
                        setBlocking(blocking)
                        options.onBlockingChange?.(blocking)
                    },
                    onSource: (source) => {
                        setSource(source)
                        options.onSource?.(source)
                    },
                },
                dependencies,
            ),
        { presentation: 'dialog', format: 'lossless-backup' },
    )
}
