import { open, save } from '@tauri-apps/plugin-dialog'

import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import { isTauriAndroid } from '../platform'
import { pickAndroidLegacyBackupSource } from '../storage/androidSafBridge'
import { runSharedNativeFileOperation } from '../storage/nativeFileJobManager'
import {
    runNativeLegacyLocalBackupExport,
    runNativeLegacyLocalBackupRestore,
    syntheticNativeFileJobStatus,
    type NativeFileJobOptions,
    type NativeFileJobResult,
} from '../storage/nativeFileJobs'
import { describeDesktopSource } from '../storage/nativeFileSourceInfo'
import { getPersistentDataRuntime } from '../storage/persistentDataRuntime.svelte'
import {
    exportLegacyLocalBackupFromPicker,
    importLegacyLocalBackupFromPicker,
    isNativeLegacyBackupFallback,
    type LegacyLocalBackupFallbackContext,
    type LegacyLocalBackupFileRouteDependencies,
    type LegacyLocalBackupImportOptions,
} from './legacyLocalBackupFileRoute'

const productionDependencies: LegacyLocalBackupFileRouteDependencies = {
    runtime: getPersistentDataRuntime,
    chooseImport: async (options) => {
        if (isTauriAndroid) {
            return pickAndroidLegacyBackupSource({
                signal: options.signal,
                onSource: (source) => options.onSource?.({ name: source.displayName, bytes: source.bytes }),
                onProgress: (progress) => options.onStatus?.(syntheticNativeFileJobStatus(
                    { jobId: progress.requestId, kind: 'restore-legacy-local-backup' },
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
            filters: [{ name: 'RisuNest Backup', extensions: ['bin'] }],
        })
        if (typeof selected !== 'string') return null
        options.onSource?.(await describeDesktopSource(selected))
        return { type: 'desktopPath', path: selected }
    },
    chooseExport: async () => {
        if (isTauriAndroid) {
            return { type: 'androidSaf', suggestedName: 'risu-backup.bin' }
        }
        const path = await save({
            defaultPath: 'risu-backup.bin',
            filters: [{ name: 'RisuNest Backup', extensions: ['bin'] }],
        })
        return path ? { type: 'desktopPath', path } : null
    },
    runImport: runNativeLegacyLocalBackupRestore,
    runExport: runNativeLegacyLocalBackupExport,
    reloadPluginsAfterRestore: loadPluginsAfterAuthoritativeRestore,
}

export interface LegacyLocalBackupSystemImportOptions extends LegacyLocalBackupImportOptions {
    /**
     * Continues inside the same operation with the WebView importer when the
     * native job cannot read the file, so the progress dialog stays open.
     */
    onNativeFallback?(context: LegacyLocalBackupFallbackContext): Promise<{ warningCodes: string[] } | null>
}

export const importLegacyLocalBackupFromSystemPicker = (
    options: LegacyLocalBackupSystemImportOptions = {},
): Promise<NativeFileJobResult | { warningCodes: string[] } | null> => runSharedNativeFileOperation(
    'import',
    'legacy-local-backup-import',
    async ({ signal, onStatus, setBlocking, setSource, setPartialWritesPossible }) => {
        const { onNativeFallback, ...importOptions } = options
        try {
            return await importLegacyLocalBackupFromPicker({
                ...importOptions,
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
            }, productionDependencies)
        }
        catch (error) {
            if (!onNativeFallback || !isNativeLegacyBackupFallback(error)) throw error
            onStatus(syntheticNativeFileJobStatus({ kind: 'restore-legacy-local-backup' }, 'awaiting-reselect'))
            return await onNativeFallback({ signal, onStatus, setSource, setPartialWritesPossible })
        }
    },
    { presentation: 'dialog', format: 'local-backup' },
)

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
