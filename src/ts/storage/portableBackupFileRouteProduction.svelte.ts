import { portableBackupSuggestedName } from './portableBackupName'
import { invoke } from '@tauri-apps/api/core'
import { open, save } from './fileSelection'
import { language } from 'src/lang'
import { alertCheckboxConfirm, alertConfirm, alertNormal } from '../alert'
import { pickIOSBackupSource, materializeIOSBackupSource } from './iosFiles'
import { isTauriIOS, isTauri, isTauriAndroid } from '../platform'
import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import {
    discardAndroidSafSource,
    pickAndroidPortableBackupSource,
    materializeAndroidBackupSource,
} from './androidSafBridge'
import {
    selectPortableBackupExport,
    selectPortableBackupRestore,
} from './deviceBackup/selectionDialog'
import { selectPluginValueAssignment } from './pluginValueAssignDialog'
import { recordRecoveredNativeFileOperation, runSharedNativeFileOperation } from './nativeFileJobManager'
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
import { backupKindLabel, formatBytes } from '../gui/nativeFileJobDialogModel'
import { formatPreservationReport } from './preservationReportMessage'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { nativeRestoreDeadline } from './nativeRestoreDeadline'

type SourceInfo = { name: string; bytes?: number }
export interface BackupPickerContext {
    signal: AbortSignal
    onStatus(status: NativeFileJobStatus): void
    onSource(source: SourceInfo): void
    /** Begins the background hold once a picker has returned. */
    protect(): void
}
export type BackupSourceFactory = (
    context: BackupPickerContext,
) => Promise<NativeFileJobSource | null>
export type BackupRestoreResult = NativeFileJobResult
export interface BackupRestoreOptions extends NativeFileRestoreJobOptions {
    onSource?(source: SourceInfo): void
    /** The name and size to show for a source that is a copy of the user's file, such as a dropped file's inbox copy. */
    sourceInfo?: SourceInfo
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
        cancel,
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
    return runSharedNativeFileOperation(
        'export',
        'portable-backup-export',
        async ({ signal, onStatus, protect }) => {
            const joined = combineSignals(signal, options.signal)
            try {
                checkSignal(joined.signal)
                const selection = await selectPortableBackupExport()
                if (!selection) return null
                checkSignal(joined.signal)
                const suggestedName = portableBackupSuggestedName()
                const path =
                    isTauriAndroid || isTauriIOS
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
                if (!isTauriAndroid && !isTauriIOS && !path) return null
                checkSignal(joined.signal)
                let destinationChosen = false
                return await runNativeArchiveExport(
                    getPersistentDataRuntime(),
                    isTauriIOS
                        ? { type: 'iosFiles', suggestedName }
                        : isTauriAndroid
                          ? { type: 'androidSaf', suggestedName }
                          : { type: 'desktopPath', path: path! },
                    selection,
                    {
                        ...options,
                        signal: joined.signal,
                        confirmSourcePreservation: () => alertConfirm(language.risuNest.backup.sourcePreservationConfirm),
                        onStatus(status) {
                            onStatus(status)
                            options.onStatus?.(status)
                        },
                        onDestinationChosen() {
                            destinationChosen = true
                            protect()
                            options.onDestinationChosen?.()
                        },
                    },
                ).catch((error: unknown) => {
                    // Backing out of the Android save picker ends like a dismissed
                    // desktop dialog; a cancelled export still rejects.
                    if (
                        !destinationChosen &&
                        !joined.signal.aborted &&
                        error instanceof DOMException &&
                        error.name === 'AbortError'
                    )
                        return null
                    throw error
                })
            } finally {
                joined.dispose()
            }
        },
        { presentation: 'dialog', format: 'library-backup', protectAfterPicker: true },
    )
}

export function restoreBackupFromSystemPicker(
    options: BackupRestoreOptions = {},
) {
    return restoreBackupFromSource(async (context) => {
        if (isTauriIOS) {
            const picked = await pickIOSBackupSource(context.signal)
            if (!picked) return null
            context.onSource({ name: picked.name, bytes: picked.bytes })
            return { type: 'iosScoped', token: picked.token }
        }
        if (isTauriAndroid)
            return pickAndroidPortableBackupSource({
                signal: context.signal,
                onSource: (source) =>
                    context.onSource({
                        name: source.displayName,
                        bytes: source.bytes,
                    }),
                onProgress: (progress) => {
                    // Copying starts once the picker returns, so the copy is the first protected work.
                    context.protect()
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
                    )
                },
            })
        const path = await open({
            multiple: false,
            directory: false,
            filters: [
                { name: 'Backup', extensions: ['risunest', 'bin', 'risudat'] },
            ],
        })
        if (typeof path !== 'string') return null
        return { type: 'desktopPath', path }
    }, options, true)
}

export function restoreBackupFromNativeSource(
    source: NativeFileJobSource | BackupSourceFactory,
    options: BackupRestoreOptions = {},
): Promise<BackupRestoreResult | null> {
    return restoreBackupFromSource(source, options, false)
}

let givenSourceRestores = 0

async function restoreBackupFromSource(
    source: NativeFileJobSource | BackupSourceFactory,
    options: BackupRestoreOptions,
    sourceFromPicker: boolean,
): Promise<BackupRestoreResult | null> {
    if (!isTauri)
        throw new NativeFileJobError(
            'native-required',
            'Native backup restore requires the app',
        )
    return runSharedNativeFileOperation(
        'import',
        // A second picker press joins the restore it opened; a given source is a restore of its own.
        sourceFromPicker ? 'backup-import' : `backup-import:${++givenSourceRestores}`,
        async (context) => {
            const joined = combineSignals(context.signal, options.signal)
            let input: NativeFileJobSource | null = null
            let selectedCustody: NativeFileJobSource | undefined
            let completedResult: NativeFileJobResult | undefined
            const cleanupSource = () => {
                const cleanup: Promise<unknown>[] = []
                // Every restore job claims its spool, so a completed restore leaves none to remove.
                if (input?.type === 'androidSpool' && !completedResult) cleanup.push(discardAndroidSafSource(input.token))
                if (selectedCustody) cleanup.push(invoke('native_portable_source_discard', { source: selectedCustody }))
                return Promise.allSettled(cleanup)
            }
            const onPageHide = () => {
                joined.cancel()
                void cleanupSource()
            }
            window.addEventListener('pagehide', onPageHide, { once: true })
            let report: NativeFileJobStatus['preservationReport']
            const onStatus = (status: NativeFileJobStatus) => {
                context.onStatus(status)
                options.onStatus?.(status)
                if (status.preservationReport)
                    report = status.preservationReport
            }
            let sourceInfo: SourceInfo | undefined
            const onSource = (value: SourceInfo) => {
                sourceInfo = value
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
                              protect: context.protect,
                          })
                        : source
                if (!input) return null
                // The source is chosen, so the restore holds the background task from here.
                context.protect()
                // A saved conflict backup is named by its date, not by a file the user chose.
                const fromFile = input.type !== 'savedConflict'
                if (input.type === 'androidSeekable' || input.type === 'iosScoped' || input.type === 'savedConflict') selectedCustody = input
                if (options.sourceInfo) onSource(options.sourceInfo)
                else if (input.type === 'desktopPath')
                    onSource(await nativeRestoreDeadline(describeDesktopSource(input.path), 'source metadata', undefined, undefined, undefined, joined.signal))
                checkSignal(joined.signal)
                const format = await nativeRestoreDeadline(invoke<
                    | 'portable'
                    | 'block-risu-save'
                    | 'local-backup'
                >('native_backup_source_format', { source: input }), 'source format', undefined, undefined, undefined, joined.signal)
                checkSignal(joined.signal)
                if (format !== 'portable') {
                    if (input.type === 'androidSeekable') input = await materializeAndroidBackupSource(input.token, {
                        signal: joined.signal,
                        onProgress: progress => onStatus(syntheticNativeFileJobStatus(
                            { jobId: progress.requestId, kind: format === 'local-backup' ? 'restore-legacy-local-backup' : 'restore-block-risu-save' },
                            'copying-source',
                            {
                                stageUnit: 'bytes', stageCompleted: progress.copiedBytes,
                                ...(progress.totalBytes === null ? {} : { stageTotal: progress.totalBytes }),
                            },
                        )),
                    })
                    else if (input.type === 'iosScoped') {
                        const token = input.token
                        const materialized = await materializeIOSBackupSource(token, {
                            signal: joined.signal,
                            onProgress: progress => onStatus(syntheticNativeFileJobStatus(
                                { jobId: token, kind: format === 'local-backup' ? 'restore-legacy-local-backup' : 'restore-block-risu-save' },
                                'copying-source',
                                { stageUnit: 'bytes', stageCompleted: progress.copiedBytes, stageTotal: progress.totalBytes },
                            )),
                        })
                        input = { type: 'desktopPath', path: materialized.path }
                    }
                    checkSignal(joined.signal)
                }
                const runtime = getPersistentDataRuntime()
                const restoredFile = fromFile ? sourceInfo : undefined
                const restoreOptions: NativeFileRestoreJobOptions = {
                    ...options,
                    ...(restoredFile ? { confirmationDetails: [
                        {
                            label: language.risuNest.importIntake.detailFile,
                            value: restoredFile.bytes === undefined ? restoredFile.name : `${restoredFile.name} · ${formatBytes(restoredFile.bytes)}`,
                        },
                        { label: language.risuNest.importIntake.detailKind, value: backupKindLabel(format) },
                    ] } : {}),
                    signal: joined.signal,
                    onStatus,
                    assignPluginValues: selectPluginValueAssignment,
                    onBlockingChange(blocking) {
                        context.setBlocking(blocking)
                        options.onBlockingChange?.(blocking)
                    },
                    async afterPortableAdoption(status) {
                        await context.releaseLibraryAfterPortableAdoption(status)
                        await options.afterPortableAdoption?.(status)
                    },
                    beforeActivation: options.beforeActivation,
                    onNativeStatus: options.onNativeStatus,
                    onRecoveredStatus(status) {
                        recordRecoveredNativeFileOperation(status)
                        options.onRecoveredStatus?.(status)
                    },
                    afterRefresh: async (alreadyRestarted) => {
                        await loadPluginsAfterAuthoritativeRestore(alreadyRestarted)
                        await options.afterRefresh?.(alreadyRestarted)
                    },
                }
                const selectedInput = input
                    const result = await (async () => {
                        if (
                            format === 'portable'
                        )
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
                        if (format === 'local-backup') {
                            return runNativeLegacyLocalBackupRestore(runtime, selectedInput, restoreOptions)
                        }
                        throw new NativeFileJobError(
                            'unsupported-format',
                            'Unsupported backup format',
                        )
                    })()
                    if (report) alertNormal(formatPreservationReport(report))
                    completedResult = result
                    return result
            } finally {
                window.removeEventListener('pagehide', onPageHide)
                joined.dispose()
                // A claimed token has already moved to native ownership, so this only removes an
                // unclaimed picker result when confirmation, format probing, or admission failed.
                const settled = cleanupSource()
                if (joined.signal.aborted) void settled
                else {
                    try {
                        const outcomes = await nativeRestoreDeadline(settled, 'source cleanup')
                        if (outcomes.some(outcome => outcome.status === 'rejected' || outcome.value === false)) throw new Error('Source cleanup failed')
                    } catch {
                        if (completedResult && !completedResult.warningCodes.includes('cleanup-failed')) completedResult.warningCodes.push('cleanup-failed')
                    }
                }
            }
        },
        { presentation: 'dialog', format: 'library-backup', protectAfterPicker: sourceFromPicker },
    )
}
