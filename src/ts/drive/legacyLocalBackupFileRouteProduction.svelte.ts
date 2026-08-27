import { open, save } from '@tauri-apps/plugin-dialog'

import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import { isTauriAndroid } from '../platform'
import { pickAndroidLegacyBackupSource } from '../storage/androidSafBridge'
import { runSharedNativeFileOperation } from '../storage/nativeFileJobManager'
import {
    runNativeLegacyLocalBackupExport,
    runNativeLegacyLocalBackupRestore,
    type NativeFileJobOptions,
    type NativeFileRestoreJobOptions,
} from '../storage/nativeFileJobs'
import { getPersistentDataRuntime } from '../storage/persistentDataRuntime.svelte'
import {
    exportLegacyLocalBackupFromPicker,
    importLegacyLocalBackupFromPicker,
    type LegacyLocalBackupFileRouteDependencies,
} from './legacyLocalBackupFileRoute'

const productionDependencies: LegacyLocalBackupFileRouteDependencies = {
    runtime: getPersistentDataRuntime,
    chooseImport: async (options) => {
        if (isTauriAndroid) {
            return pickAndroidLegacyBackupSource({ signal: options.signal })
        }
        const selected = await open({
            multiple: false,
            directory: false,
            filters: [{ name: 'RisuAI Backup', extensions: ['bin'] }],
        })
        return typeof selected === 'string'
            ? { type: 'desktopPath', path: selected }
            : null
    },
    chooseExport: async () => {
        if (isTauriAndroid) {
            return { type: 'androidSaf', suggestedName: 'risu-backup.bin' }
        }
        const path = await save({
            defaultPath: 'risu-backup.bin',
            filters: [{ name: 'RisuAI Backup', extensions: ['bin'] }],
        })
        return path ? { type: 'desktopPath', path } : null
    },
    runImport: runNativeLegacyLocalBackupRestore,
    runExport: runNativeLegacyLocalBackupExport,
    reloadPluginsAfterRestore: loadPluginsAfterAuthoritativeRestore,
}

export const importLegacyLocalBackupFromSystemPicker = (
    options: NativeFileRestoreJobOptions = {},
) => runSharedNativeFileOperation('import', 'legacy-local-backup-import', ({ signal, onStatus, setBlocking }) =>
    importLegacyLocalBackupFromPicker({
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

export const exportLegacyLocalBackupFromSystemPicker = (
    options: NativeFileJobOptions = {},
) => runSharedNativeFileOperation('export', 'legacy-local-backup-export', ({ signal, onStatus }) =>
    exportLegacyLocalBackupFromPicker({
        ...options,
        signal,
        onStatus: (status) => {
            onStatus(status)
            options.onStatus?.(status)
        },
    }, productionDependencies))
