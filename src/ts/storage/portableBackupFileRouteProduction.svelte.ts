import { invoke } from '@tauri-apps/api/core'
import { open, save } from '@tauri-apps/plugin-dialog'
import { language } from 'src/lang'
import { alertConfirm, alertNormal } from '../alert'
import { isTauri, isTauriAndroid } from '../platform'
import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import {
    discardAndroidSafSource,
    pickAndroidBackupSource,
} from './androidSafBridge'
import {
    selectPortableBackupExport,
    selectPortableBackupRestore,
} from './deviceBackup/selectionDialog'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    NativeFileJobError,
    runNativeArchiveExport,
    runNativeArchiveRestore,
    runNativeBlockRisuSaveRestore,
    runNativeLegacyLocalBackupRestore,
    syntheticNativeFileJobStatus,
    type NativeFileJobOptions,
    type NativeFileRestoreJobOptions,
    type NativeFileJobSource,
    type NativeFileJobStatus,
    type NativeFileJobResult,
} from './nativeFileJobs'
import { describeDesktopSource } from './nativeFileSourceInfo'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import {
    getServerSyncController,
    resumeServerSyncAfterBackup,
    holdServerSyncAfterRestore,
} from './sync/serverSyncProduction'

type SourceInfo = { name: string; bytes?: number }
export interface BackupPickerContext {
    signal: AbortSignal
    onStatus(status: NativeFileJobStatus): void
    onSource(source: SourceInfo): void
}
export type BackupSourceFactory = (
    context: BackupPickerContext,
) => Promise<NativeFileJobSource | null>
export interface BackupRestoreOptions extends NativeFileRestoreJobOptions {
    onSource?(source: SourceInfo): void
}

function combineSignals(...signals: (AbortSignal | undefined)[]) {
    const controller = new AbortController()
    const cancel = () => controller.abort()
    for (const signal of signals) {
        signal?.addEventListener('abort', cancel, { once: true })
        if (signal?.aborted) cancel()
    }
    return {
        signal: controller.signal,
        dispose() {
            for (const signal of signals)
                signal?.removeEventListener('abort', cancel)
        },
    }
}
function checkSignal(signal: AbortSignal) {
    if (signal.aborted) throw new DOMException('Backup cancelled', 'AbortError')
}

export async function exportPortableBackupFromSystemPicker(
    options: NativeFileJobOptions = {},
): Promise<NativeFileJobResult | null> {
    if (!isTauri)
        throw new NativeFileJobError(
            'native-required',
            'RisuNest backup requires the native app',
        )
    getServerSyncController().assertFileOperationAvailable()
    return runSharedNativeFileOperation(
        'export',
        'portable-backup-export',
        async ({ signal, onStatus }) => {
            const joined = combineSignals(signal, options.signal)
            try {
                checkSignal(joined.signal)
                const selection = await selectPortableBackupExport()
                if (!selection) return null
                checkSignal(joined.signal)
                const suggestedName = `risunest-${new Date().toISOString().replace(/[:.]/g, '-')}.risunest`
                const path = isTauriAndroid
                    ? null
                    : await save({
                          defaultPath: suggestedName,
                          filters: [
                              {
                                  name: 'RisuNest Backup',
                                  extensions: ['risunest'],
                              },
                          ],
                      })
                if (!isTauriAndroid && !path) return null
                checkSignal(joined.signal)
                return await runNativeArchiveExport(
                    getPersistentDataRuntime(),
                    isTauriAndroid
                        ? { type: 'androidSaf', suggestedName }
                        : { type: 'desktopPath', path: path! },
                    selection,
                    {
                        ...options,
                        signal: joined.signal,
                        onStatus(status) {
                            onStatus(status)
                            options.onStatus?.(status)
                        },
                    },
                )
            } finally {
                joined.dispose()
            }
        },
        { format: 'library-backup' },
    ).finally(resumeServerSyncAfterBackup)
}

export function restoreBackupFromSystemPicker(
    options: BackupRestoreOptions = {},
) {
    return restoreBackupFromNativeSource(async (context) => {
        if (isTauriAndroid)
            return pickAndroidBackupSource({
                signal: context.signal,
                onSource: (source) =>
                    context.onSource({
                        name: source.displayName,
                        bytes: source.bytes,
                    }),
                onProgress: (progress) =>
                    context.onStatus(
                        syntheticNativeFileJobStatus(
                            {
                                jobId: progress.requestId,
                                kind: 'restore-portable-backup',
                            },
                            'copying-source',
                            {
                                stageUnit: 'bytes',
                                stageCompleted: progress.copiedBytes,
                                ...(progress.totalBytes === null
                                    ? {}
                                    : { stageTotal: progress.totalBytes }),
                            },
                        ),
                    ),
            })
        const path = await open({
            multiple: false,
            directory: false,
            filters: [
                { name: 'Backup', extensions: ['risunest', 'bin', 'risudat'] },
            ],
        })
        if (typeof path !== 'string') return null
        context.onSource(await describeDesktopSource(path))
        return { type: 'desktopPath', path }
    }, options)
}

export async function restoreBackupFromNativeSource(
    source: NativeFileJobSource | BackupSourceFactory,
    options: BackupRestoreOptions = {},
): Promise<NativeFileJobResult | null> {
    if (!isTauri)
        throw new NativeFileJobError(
            'native-required',
            'Native backup restore requires the app',
        )
    getServerSyncController().assertFileOperationAvailable()
    return runSharedNativeFileOperation(
        'import',
        'backup-import',
        async (context) => {
            const joined = combineSignals(context.signal, options.signal)
            let input: NativeFileJobSource | null = null
            let report: NativeFileJobStatus['preservationReport']
            const onStatus = (status: NativeFileJobStatus) => {
                context.onStatus(status)
                options.onStatus?.(status)
                if (status.preservationReport)
                    report = status.preservationReport
            }
            const onSource = (value: SourceInfo) => {
                context.setSource(value)
                options.onSource?.(value)
            }
            try {
                checkSignal(joined.signal)
                input =
                    typeof source === 'function'
                        ? await source({
                              signal: joined.signal,
                              onStatus,
                              onSource,
                          })
                        : source
                if (!input) return null
                if (input.type === 'desktopPath')
                    onSource(await describeDesktopSource(input.path))
                checkSignal(joined.signal)
                const format = await invoke<
                    'portable' | 'block-risu-save' | 'local-backup'
                >('native_backup_source_format', { source: input })
                if (
                    format !== 'portable' &&
                    (!(await alertConfirm(language.backupLoadConfirm)) ||
                        !(await alertConfirm(language.backupLoadConfirm2)))
                )
                    return null
                checkSignal(joined.signal)
                const runtime = getPersistentDataRuntime()
                let replacesLibrary = format !== 'portable'
                const restoreOptions: NativeFileRestoreJobOptions = {
                    ...options,
                    signal: joined.signal,
                    onStatus,
                    onBlockingChange(blocking) {
                        context.setBlocking(blocking)
                        options.onBlockingChange?.(blocking)
                    },
                    beforeActivation: async () => {
                        await options.beforeActivation?.()
                        try {
                            await getServerSyncController().confirmReplacement()
                        } catch {
                            throw new NativeFileJobError(
                                'resolve-pending-operation-first',
                                'Confirm the synchronization outcome before restoring.',
                            )
                        }
                    },
                    onNativeStatus: async (status) => {
                        if (status.state === 'succeeded' && replacesLibrary)
                            holdServerSyncAfterRestore()
                        await options.onNativeStatus?.(status)
                    },
                    afterRefresh: async () => {
                        await loadPluginsAfterAuthoritativeRestore()
                        await options.afterRefresh?.()
                    },
                }
                const selectedInput = input
                const result = await getServerSyncController().withReplacement(
                    async () => {
                        if (format === 'portable')
                            return runNativeArchiveRestore(
                                runtime,
                                selectedInput,
                                {
                                    ...restoreOptions,
                                    choosePortableSections: async (preview) => {
                                        const selection =
                                            await selectPortableBackupRestore(
                                                preview,
                                            )
                                        replacesLibrary =
                                            selection?.library ?? false
                                        return selection
                                    },
                                },
                            )
                        if (format === 'block-risu-save')
                            return runNativeBlockRisuSaveRestore(
                                runtime,
                                selectedInput,
                                restoreOptions,
                            )
                        if (format === 'local-backup')
                            return runNativeLegacyLocalBackupRestore(
                                runtime,
                                selectedInput,
                                restoreOptions,
                            )
                        throw new NativeFileJobError(
                            'unsupported-format',
                            'Unsupported backup format',
                        )
                    },
                )
                if (report)
                    alertNormal(
                        `${language.portableBackup.preserved}: ${report.files} ${language.files}, ${report.bytes} bytes. ${language.portableBackup.preservedSourceHelp}\n${report.path}`,
                    )
                return result
            } finally {
                joined.dispose()
                // A claimed token has already moved to native ownership, so this only removes an
                // unclaimed picker result when confirmation, format probing, or admission failed.
                if (input?.type === 'androidSpool')
                    discardAndroidSafSource(input.token)
            }
        },
        { presentation: 'dialog', format: 'library-backup' },
    )
}
