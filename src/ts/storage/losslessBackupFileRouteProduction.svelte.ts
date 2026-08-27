import { open, save } from '@tauri-apps/plugin-dialog'

import { LoadLocalBackup, SaveLocalBackup } from '../drive/backuplocal'
import { isTauri, isTauriAndroid, isTauriDesktop } from '../platform'
import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import { pickAndroidLosslessBackupSource } from './androidSafBridge'
import {
    exportLocalBackupFromPicker,
    restoreLocalBackupFromPicker,
    type LosslessBackupFileRouteDependencies,
} from './losslessBackupFileRoute'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    runNativeLosslessBackupExport,
    runNativeLosslessBackupRestore,
    type NativeFileJobOptions,
    type NativeFileRestoreJobOptions,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

const productionDependencies: LosslessBackupFileRouteDependencies = {
    platform: () => isTauriDesktop
        ? 'native-desktop'
        : isTauriAndroid
            ? 'native-android'
            : 'web',
    runtime: getPersistentDataRuntime,
    chooseNativeImport: async ({ signal, onStatus }) => {
        if (isTauriAndroid) {
            return pickAndroidLosslessBackupSource({
                signal,
                onProgress: (progress) => onStatus?.({
                    jobId: progress.requestId,
                    kind: 'restore-lossless-backup',
                    state: 'running',
                    phase: 'reading-source',
                    progress: {
                        completedBytes: progress.copiedBytes,
                        ...(progress.totalBytes === null ? {} : { totalBytes: progress.totalBytes }),
                        completedItems: 0,
                        totalItems: 1,
                    },
                }),
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
        return typeof selected === 'string'
            ? { type: 'desktopPath', path: selected }
            : null
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
    saveLegacyBackup: SaveLocalBackup,
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

export function restoreLocalBackupFromSystemPicker(
    options: NativeFileRestoreJobOptions = {},
) {
    if (!isTauri) return restoreLocalBackupFromPicker(options, productionDependencies)
    return runSharedNativeFileOperation('import', 'lossless-backup-import', ({ signal, onStatus, setBlocking }) =>
        restoreLocalBackupFromPicker({
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
        }, productionDependencies))
}
