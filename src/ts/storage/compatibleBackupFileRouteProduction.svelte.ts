import { save } from '@tauri-apps/plugin-dialog'

import { isTauri, isTauriAndroid } from '../platform'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    NativeFileJobError,
    runNativeCompatibleLocalBackupExport,
    type NativeCompatibilityReport,
    type NativeCompatibilityTarget,
    type NativeFileJobOptions,
    type NativeFileJobResult,
    type NativeLegacyLocalBackupDestination,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import {
    getServerSyncController,
    resumeServerSyncAfterBackup,
} from './sync/serverSyncProduction'

export interface CompatibilityBackupExportOptions extends NativeFileJobOptions {
    /** The completed export's report, also available on the returned result. */
    onReport?(report: NativeCompatibilityReport): void
}

export interface CompatibilityBackupExportResult extends NativeFileJobResult {
    compatibilityReport?: NativeCompatibilityReport
}

export async function exportCompatibilityBackupFromSystemPicker(
    target: NativeCompatibilityTarget,
    options: CompatibilityBackupExportOptions = {},
): Promise<CompatibilityBackupExportResult | null> {
    if (!isTauri) {
        throw new NativeFileJobError(
            'native-required',
            'Compatibility backup export requires the desktop or Android app.',
        )
    }
    getServerSyncController().assertFileOperationAvailable()
    return runSharedNativeFileOperation(
        'export',
        `compatibility-backup-export:${target}`,
        async ({ signal: managedSignal, onStatus }) => {
            const controller = new AbortController()
            const cancel = () => controller.abort()
            const signals = [
                managedSignal,
                ...(options.signal ? [options.signal] : []),
            ]
            for (const signal of signals) {
                signal.addEventListener('abort', cancel, { once: true })
                if (signal.aborted) cancel()
            }
            const checkCancelled = () => {
                if (controller.signal.aborted) {
                    throw new DOMException(
                        'Compatibility export was cancelled',
                        'AbortError',
                    )
                }
            }
            try {
                checkCancelled()
                const suggestedName = `${target}-backup.bin`
                let destination: NativeLegacyLocalBackupDestination
                if (isTauriAndroid) {
                    destination = { type: 'androidSaf', suggestedName }
                } else {
                    const path = await save({
                        defaultPath: suggestedName,
                        filters: [
                            {
                                name:
                                    target === 'risuai'
                                        ? 'RisuAI Backup'
                                        : 'PocketRisu Backup',
                                extensions: ['bin'],
                            },
                        ],
                    })
                    checkCancelled()
                    if (!path) return null
                    destination = { type: 'desktopPath', path }
                }
                let report: NativeCompatibilityReport | undefined
                const result = await runNativeCompatibleLocalBackupExport(
                    getPersistentDataRuntime(),
                    target,
                    destination,
                    {
                        ...options,
                        signal: controller.signal,
                        onStatus: (status) => {
                            if (status.compatibilityReport)
                                report = status.compatibilityReport
                            onStatus(status)
                            options.onStatus?.(status)
                        },
                    },
                )
                if (report) options.onReport?.(report)
                return {
                    ...result,
                    ...(report ? { compatibilityReport: report } : {}),
                }
            } finally {
                for (const signal of signals)
                    signal.removeEventListener('abort', cancel)
            }
        },
        // Full-library admission prevents sync while the snapshot is captured.
        { presentation: 'dialog', format: 'library-backup' },
    ).finally(resumeServerSyncAfterBackup)
}
