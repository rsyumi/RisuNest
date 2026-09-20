import { describe, expect, it } from 'vitest'

import { languageEnglish } from 'src/lang/en'
import type {
    NativeFileOperationOutcome,
    NativeFileOperationState,
} from '../storage/nativeFileJobManager'
import {
    emptyNativeImportCounts,
    type NativeFileJobDetail,
    type NativeFileJobStage,
    type NativeFileJobStatus,
    type NativeImportCounts,
} from '../storage/nativeFileJobs'
import {
    buildNativeFileJobDialogModel,
    failureReason,
    fillTemplate,
    formatBytes,
    formatElapsed,
} from './nativeFileJobDialogModel'

const copy = languageEnglish.risuNest.importDialog

function status(patch: Partial<NativeFileJobStatus> = {}): NativeFileJobStatus {
    return {
        jobId: 'job',
        kind: 'restore-legacy-local-backup',
        state: 'running',
        phase: 'reading-source',
        progress: { completedBytes: 0, completedItems: 0 },
        ...patch,
    }
}

function detail(
    stage: NativeFileJobStage,
    patch: Partial<Omit<NativeFileJobDetail, 'stage' | 'counts'>> = {},
    counts: Partial<NativeImportCounts> = {},
): NativeFileJobDetail {
    return {
        stage,
        stageCompleted: 0,
        stageUnit: 'items',
        ...patch,
        counts: { ...emptyNativeImportCounts(), ...counts },
    }
}

function running(patch: Partial<NativeFileOperationState> = {}): NativeFileOperationState {
    return {
        kind: 'import',
        presentation: 'dialog',
        format: 'local-backup',
        startedAt: 1_000,
        observedStages: [],
        blocking: false,
        cancelRequested: false,
        partialWritesPossible: false,
        ...patch,
    }
}

function outcome(patch: Partial<NativeFileOperationOutcome> = {}): NativeFileOperationOutcome {
    return {
        kind: 'import',
        format: 'local-backup',
        startedAt: 1_000,
        finishedAt: 66_000,
        state: 'succeeded',
        observedStages: ['reading-archive', 'finalizing-staging', 'activating', 'refreshing-app', 'reloading-plugins'],
        warningCodes: [],
        partialWritesPossible: false,
        ...patch,
    }
}

function stageIds(model: ReturnType<typeof buildNativeFileJobDialogModel>): string[] {
    return model.stages.map((row) => `${row.stage}:${row.state}`)
}

describe('nativeFileJobDialogModel', () => {
    it('is closed when nothing is running and nothing finished', () => {
        expect(buildNativeFileJobDialogModel(null, null, 0).open).toBe(false)
        expect(
            buildNativeFileJobDialogModel(
                running({ presentation: 'inline' }),
                null,
                0,
            ).open,
        ).toBe(false)
    })

    it('opens with the expected pending stages before the first status arrives', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                source: { name: 'risu-backup.bin', bytes: 3 * 1024 * 1024 },
            }),
            null,
            31_000,
        )
        expect(model.open).toBe(true)
        expect(model.title).toBe(copy.titleLocalBackup)
        expect(model.subtitle).toBe('')
        expect(model.sourceName).toBe('risu-backup.bin')
        expect(model.sourceSize).toBe('3.0 MiB')
        expect(model.elapsed).toBe(fillTemplate(copy.elapsed, '00:30'))
        expect(model.indeterminate).toBe(true)
        expect(model.overallPercent).toBeNull()
        expect(stageIds(model)).toEqual([
            'reading-archive:pending',
            'preparing-attachments:pending',
            'reading-database:pending',
            'finalizing-staging:pending',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
        expect(
            model.counters.map((counter) => `${counter.key}=${counter.value}`),
        ).toEqual([
            'characters=–',
            'presets=–',
            'assets=–',
            'inlays=–',
            'coldStorage=–',
            'pocketMedia=–',
            'skipped=–',
        ])
        expect(model.cancelVisible).toBe(true)
        expect(model.cancelEnabled).toBe(true)
        expect(model.cancelLabel).toBe(copy.cancel)
        expect(model.closeVisible).toBe(false)
        expect(model.terminal).toBeNull()
    })

    it('falls back to job phases when a status carries no detail and drops skipped expected stages', () => {
        const reading = buildNativeFileJobDialogModel(
            running({
                status: status({
                    progress: {
                        completedBytes: 512 * 1024,
                        totalBytes: 1024 * 1024,
                        completedItems: 0,
                    },
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            1_000,
        )
        expect(stageIds(reading)[0]).toBe('reading-archive:active')
        expect(reading.stages[0].detail).toBe(
            fillTemplate(copy.overall, '512 KiB', '1.0 MiB'),
        )
        expect(reading.overallPercent).toBe(50)
        expect(reading.indeterminate).toBe(false)

        const staging = buildNativeFileJobDialogModel(
            running({
                status: status({
                    phase: 'staging-database',
                    progress: {
                        completedBytes: 1024 * 1024,
                        totalBytes: 1024 * 1024,
                        completedItems: 2,
                    },
                }),
                observedStages: ['reading-archive', 'finalizing-staging'],
            }),
            null,
            1_000,
        )
        expect(stageIds(staging)).toEqual([
            'reading-archive:done',
            'finalizing-staging:active',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
    })

    it('renders detail stages with their own progress, counters, current item, and format subtitle', () => {
        const counts = {
            entriesRead: 12,
            assets: 5,
            inlays: 2,
            coldStorage: 1,
            pocketMedia: 3,
            pocketMetadata: 3,
            skipped: 1,
        }
        const model = buildNativeFileJobDialogModel(
            running({
                status: status({
                    progress: {
                        completedBytes: 40,
                        totalBytes: 200,
                        completedItems: 12,
                    },
                    detail: detail(
                        'reading-archive',
                        {
                            stageCompleted: 40,
                            stageTotal: 100,
                            stageUnit: 'bytes',
                            currentItem: 'inlay/abc.webp',
                        },
                        counts,
                    ),
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            1_000,
        )
        expect(model.subtitle).toBe(copy.formatPocketRisu)
        expect(model.stages[0]).toMatchObject({
            stage: 'reading-archive',
            state: 'active',
            detail: fillTemplate(copy.overall, '40 B', '100 B'),
        })
        expect(model.currentItem).toBe(
            fillTemplate(copy.currentItem, 'inlay/abc.webp'),
        )
        expect(
            model.counters.map((counter) => `${counter.key}=${counter.value}`),
        ).toEqual([
            'characters=–',
            'presets=–',
            'assets=5',
            'inlays=2',
            'coldStorage=1',
            'pocketMedia=3',
            'skipped=1',
        ])

        const preparing = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'preparing-attachments',
                        {
                            stageCompleted: 3,
                            stageTotal: 11,
                            currentItem: 'assets/a.png',
                        },
                        { ...counts, entriesTotal: 12, attachmentsPrepared: 3 },
                    ),
                }),
                observedStages: ['reading-archive', 'preparing-attachments'],
            }),
            null,
            1_000,
        )
        expect(stageIds(preparing).slice(0, 2)).toEqual([
            'reading-archive:done',
            'preparing-attachments:active',
        ])
        expect(preparing.stages[0].detail).toBe(
            fillTemplate(copy.itemsCount, '12'),
        )
        expect(preparing.stages[1].detail).toBe(
            fillTemplate(copy.itemsOf, '3', '11'),
        )
        expect(preparing.currentItem).toBe(
            fillTemplate(copy.currentItem, 'assets/a.png'),
        )

        const staging = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'staging-characters',
                        { stageCompleted: 4, stageTotal: 9 },
                        {
                            ...counts,
                            entriesTotal: 12,
                            characters: 4,
                            charactersTotal: 9,
                            presets: 2,
                        },
                    ),
                }),
                observedStages: [
                    'reading-archive',
                    'preparing-attachments',
                    'reading-database',
                    'decoding-database',
                    'staging-characters',
                ],
            }),
            null,
            1_000,
        )
        expect(stageIds(staging)).toEqual([
            'reading-archive:done',
            'preparing-attachments:done',
            'reading-database:done',
            'decoding-database:done',
            'staging-characters:active',
            'finalizing-staging:pending',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
        expect(staging.currentItem).toBe('')
        expect(staging.counters[0].value).toBe(
            fillTemplate(copy.itemsOf, '4', '9'),
        )
        expect(staging.counters[1].value).toBe('2')
    })

    it('labels a RisuAI backup only once every entry has been classified', () => {
        const partial = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'reading-archive',
                        {},
                        { entriesRead: 3, assets: 3 },
                    ),
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            0,
        )
        expect(partial.subtitle).toBe('')
        const complete = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'preparing-attachments',
                        {},
                        { entriesRead: 3, entriesTotal: 3, assets: 3 },
                    ),
                }),
                observedStages: ['reading-archive', 'preparing-attachments'],
            }),
            null,
            0,
        )
        expect(complete.subtitle).toBe(copy.formatRisuAi)
    })

    it('names the format after the reported job when the picker admitted it as a library backup', () => {
        const counts = { entriesRead: 3, pocketMedia: 1, pocketMetadata: 1 }
        const archive = buildNativeFileJobDialogModel(
            running({
                format: 'library-backup',
                status: status({
                    kind: 'restore-legacy-local-backup',
                    detail: detail('reading-archive', {}, counts),
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            1_000,
        )
        expect(archive.title).toBe(copy.titleLocalBackup)
        expect(archive.subtitle).toBe(copy.formatPocketRisu)
        expect(archive.counters.map((counter) => counter.key)).toEqual([
            'characters',
            'presets',
            'assets',
            'inlays',
            'coldStorage',
            'pocketMedia',
            'skipped',
        ])
        expect(stageIds(archive)).toContain('preparing-attachments:pending')

        const portable = buildNativeFileJobDialogModel(
            running({
                format: 'library-backup',
                status: status({ kind: 'restore-portable-backup' }),
                observedStages: ['reading-database'],
            }),
            null,
            1_000,
        )
        expect(portable.title).toBe(copy.titleBackup)
        expect(portable.counters.map((counter) => counter.key)).toEqual([
            'characters',
            'presets',
        ])

        const unknown = buildNativeFileJobDialogModel(
            running({ format: 'library-backup' }),
            null,
            1_000,
        )
        expect(unknown.title).toBe(copy.titleBackup)
    })

    it('uses the RisuSave template and inserts optional stages only when observed', () => {
        const base = running({
            format: 'risu-save',
            status: status({ kind: 'restore-block-risu-save' }),
        })
        const initial = buildNativeFileJobDialogModel(base, null, 0)
        expect(initial.title).toBe(copy.titleRisuSave)
        expect(stageIds(initial)).toEqual([
            'reading-database:pending',
            'finalizing-staging:pending',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
        expect(initial.counters.map((counter) => counter.key)).toEqual([
            'characters',
            'presets',
        ])

        const decoding = buildNativeFileJobDialogModel(
            running({
                ...base,
                status: status({
                    kind: 'restore-block-risu-save',
                    detail: detail('decoding-database'),
                }),
                observedStages: ['reading-database', 'decoding-database'],
            }),
            null,
            0,
        )
        expect(stageIds(decoding).slice(0, 3)).toEqual([
            'reading-database:done',
            'decoding-database:active',
            'finalizing-staging:pending',
        ])
    })

    it('starts the stage list over after a compatibility re-selection', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail('reading-archive', { stageCompleted: 1 }),
                }),
                observedStages: [
                    'reading-archive',
                    'awaiting-reselect',
                    'reading-archive',
                ],
            }),
            null,
            0,
        )
        expect(stageIds(model).slice(0, 2)).toEqual([
            'awaiting-reselect:done',
            'reading-archive:active',
        ])
    })

    it('disables cancellation once data is being applied and reflects a pending cancel request', () => {
        const activating = buildNativeFileJobDialogModel(
            running({
                status: status({ phase: 'activating-database' }),
                observedStages: [
                    'reading-archive',
                    'finalizing-staging',
                    'awaiting-activation',
                    'activating',
                ],
            }),
            null,
            0,
        )
        expect(activating.cancelEnabled).toBe(false)
        expect(activating.cancelNote).toBe(copy.cancelUnavailable)
        expect(stageIds(activating)).toContain('activating:active')

        const awaiting = buildNativeFileJobDialogModel(
            running({
                status: status({
                    phase: 'awaiting-activation',
                    state: 'waitingForInput',
                }),
                observedStages: [
                    'reading-archive',
                    'finalizing-staging',
                    'awaiting-activation',
                ],
            }),
            null,
            0,
        )
        expect(awaiting.cancelEnabled).toBe(true)

        const refreshing = buildNativeFileJobDialogModel(
            running({
                status: status({
                    phase: 'complete',
                    state: 'succeeded',
                    detail: detail('refreshing-app'),
                }),
                observedStages: [
                    'reading-archive',
                    'activating',
                    'refreshing-app',
                ],
            }),
            null,
            0,
        )
        expect(refreshing.cancelEnabled).toBe(false)

        const cancelling = buildNativeFileJobDialogModel(
            running({ cancelRequested: true }),
            null,
            0,
        )
        expect(cancelling.cancelEnabled).toBe(false)
        expect(cancelling.cancelLabel).toBe(copy.cancelling)
        expect(cancelling.cancelNote).toBe('')
    })

    it('summarizes a successful import with final counts, a complete row, and mapped warnings', () => {
        const model = buildNativeFileJobDialogModel(
            null,
            outcome({
                source: { name: 'risu-backup.bin', bytes: 2048 },
                status: status({
                    state: 'succeeded',
                    phase: 'complete',
                    progress: {
                        completedBytes: 4096,
                        totalBytes: 4096,
                        completedItems: 9,
                    },
                    detail: detail(
                        'activating',
                        {},
                        {
                            entriesRead: 9,
                            entriesTotal: 9,
                            assets: 4,
                            inlays: 3,
                            coldStorage: 2,
                            characters: 7,
                            presets: 2,
                        },
                    ),
                    result: {
                        revision: 3,
                        sourceBytes: 2048,
                        sourceSha256: 'x',
                        characterCount: 7,
                        presetCount: 2,
                        warningCodes: ['cleanup-failed'],
                    },
                }),
                result: {
                    revision: 3,
                    sourceBytes: 2048,
                    sourceSha256: 'x',
                    characterCount: 7,
                    presetCount: 2,
                    warningCodes: ['cleanup-failed'],
                },
                warningCodes: ['cleanup-failed', 'mystery-code'],
            }),
            0,
        )
        expect(model.open).toBe(true)
        expect(model.terminal).toMatchObject({
            state: 'succeeded',
            summary: copy.resultSucceeded,
            reason: '',
            details: '',
            restarting: false,
        })
        expect(model.subtitle).toBe(copy.formatRisuAi)
        expect(model.elapsed).toBe(fillTemplate(copy.elapsed, '01:05'))
        expect(model.overallPercent).toBe(100)
        expect(model.indeterminate).toBe(false)
        expect(stageIds(model)).toEqual([
            'reading-archive:done',
            'finalizing-staging:done',
            'activating:done',
            'refreshing-app:done',
            'reloading-plugins:done',
            'complete:done',
        ])
        expect(
            model.counters.map((counter) => `${counter.key}=${counter.value}`),
        ).toEqual([
            'characters=7',
            'presets=2',
            'assets=4',
            'inlays=3',
            'coldStorage=2',
            'pocketMedia=0',
            'skipped=0',
        ])
        expect(model.warnings).toEqual([
            copy.warningCleanupFailed,
            fillTemplate(copy.warningUnknown, 'mystery-code'),
        ])
        expect(model.cancelVisible).toBe(false)
        expect(model.closeVisible).toBe(true)
    })

    it('keeps the dialog open without a close button while the app restarts', () => {
        const model = buildNativeFileJobDialogModel(
            null,
            outcome({
                observedStages: [
                    'reading-archive',
                    'decoding-database',
                    'activating',
                    'restarting-app',
                ],
            }),
            0,
        )
        expect(model.terminal?.summary).toBe(copy.resultRestarting)
        expect(model.terminal?.restarting).toBe(true)
        expect(model.closeVisible).toBe(false)
        expect(stageIds(model).at(-1)).toBe('restarting-app:done')
    })

    it('explains cancellations and failures without leaking raw codes into the summary', () => {
        const cancelled = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'cancelled',
                observedStages: ['reading-archive'],
                status: status({
                    progress: {
                        completedBytes: 10,
                        totalBytes: 100,
                        completedItems: 1,
                    },
                }),
            }),
            0,
        )
        expect(cancelled.terminal).toMatchObject({
            state: 'cancelled',
            summary: copy.resultCancelled,
            details: '',
        })
        expect(cancelled.overallPercent).toBe(10)
        expect(stageIds(cancelled)).toEqual(['reading-archive:stopped'])

        const partial = buildNativeFileJobDialogModel(
            null,
            outcome({ state: 'cancelled', partialWritesPossible: true }),
            0,
        )
        expect(partial.terminal?.summary).toBe(copy.resultCancelledPartial)

        const failed = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'failed',
                observedStages: ['reading-archive', 'preparing-attachments'],
                error: {
                    code: 'unsupported-format',
                    message:
                        'PocketRisu legacy JSON Inlays require the compatibility importer',
                    recoveryRequired: false,
                },
            }),
            0,
        )
        expect(failed.terminal).toMatchObject({
            state: 'failed',
            summary: copy.resultFailed,
            reason: copy.reasonUnsupportedFormat,
            details:
                '[unsupported-format] PocketRisu legacy JSON Inlays require the compatibility importer',
        })
        expect(stageIds(failed)).toEqual([
            'reading-archive:done',
            'preparing-attachments:stopped',
        ])
        expect(failed.closeVisible).toBe(true)

        const committed = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'failed',
                error: {
                    code: 'activation-committed-refresh-failed',
                    message: 'refresh exploded',
                    recoveryRequired: true,
                },
            }),
            0,
        )
        expect(committed.terminal?.summary).toBe(copy.resultFailedAfterCommit)
        expect(committed.terminal?.reason).toBe('')
        expect(committed.terminal?.details).toBe(
            '[activation-committed-refresh-failed] refresh exploded',
        )
    })

    it('maps failure codes to user copy and never shows the code itself', () => {
        const codes = [
            'unsupported-format',
            'invalid-source',
            'invalid-input',
            'truncated-input',
            'corrupt-input',
            'revision-conflict',
            'store-error',
            'missing-result',
            'missing-activation-fence',
            'import-error',
            'whatever',
        ]
        for (const code of codes) {
            const reason = failureReason(code)
            expect(reason.length).toBeGreaterThan(0)
            expect(reason).not.toContain(code)
        }
        expect(failureReason('whatever')).toBe(copy.reasonUnknown)
    })

    it('never exposes stage identifiers in labels', () => {
        const stages: NativeFileJobStage[] = [
            'copying-source',
            'awaiting-reselect',
            'reading-archive',
            'preparing-attachments',
            'reading-database',
            'decoding-database',
            'staging-characters',
            'finalizing-staging',
            'awaiting-activation',
            'activating',
            'refreshing-app',
            'reloading-plugins',
            'restarting-app',
        ]
        const model = buildNativeFileJobDialogModel(
            running({
                status: status({ detail: detail('restarting-app') }),
                observedStages: stages,
            }),
            null,
            0,
        )
        for (const row of model.stages) {
            expect(row.label).not.toContain(row.stage)
            expect(row.label.length).toBeGreaterThan(0)
        }
    })

    it('shows an export as its reported phases with export wording', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({
                    kind: 'export-portable-backup',
                    phase: 'writing-export',
                    progress: { completedBytes: 512, totalBytes: 2048, completedItems: 0 },
                }),
                observedStages: ['preparing-export', 'writing-export'],
            }),
            null,
            5_000,
        )
        expect(model.open).toBe(true)
        expect(model.title).toBe(copy.titleExportBackup)
        expect(model.subtitle).toBe('')
        expect(stageIds(model)).toEqual([
            'preparing-export:done',
            'writing-export:active',
            'finalizing-export:pending',
        ])
        expect(model.overallPercent).toBe(25)
        expect(model.counters).toEqual([])
        expect(model.cancelVisible).toBe(true)
        expect(model.cancelEnabled).toBe(true)
        expect(model.cancelLabel).toBe(copy.cancelExport)
    })

    it('names the export after the file it writes before the first status arrives', () => {
        const risuSave = buildNativeFileJobDialogModel(
            running({ kind: 'export', format: 'risu-save' }),
            null,
            0,
        )
        expect(risuSave.title).toBe(copy.titleExportRisuSave)
        expect(stageIds(risuSave)).toEqual([
            'preparing-export:pending',
            'writing-export:pending',
            'finalizing-export:pending',
        ])
        const compatible = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-compatible-local-backup', phase: 'reading-source' }),
                observedStages: ['preparing-export'],
            }),
            null,
            0,
        )
        expect(compatible.title).toBe(copy.titleExportCompatible)
    })

    it('adds the save-to-location step only when a mobile export reports it', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-portable-backup', phase: 'publishing-destination' }),
                observedStages: ['preparing-export', 'writing-export', 'finalizing-export', 'publishing-destination'],
            }),
            null,
            0,
        )
        expect(model.stages.map((row) => row.stage)).toEqual([
            'preparing-export',
            'writing-export',
            'publishing-destination',
            'finalizing-export',
        ])
        expect(model.stages.find((row) => row.stage === 'publishing-destination')?.state).toBe('active')
    })

    it('summarizes how an export ended with export wording', () => {
        const succeeded = buildNativeFileJobDialogModel(
            null,
            outcome({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-portable-backup', phase: 'complete', state: 'succeeded' }),
                observedStages: ['preparing-export', 'writing-export', 'finalizing-export'],
            }),
            0,
        )
        expect(succeeded.title).toBe(copy.titleExportBackup)
        expect(succeeded.terminal?.summary).toBe(copy.resultExportSucceeded)
        expect(succeeded.terminal?.restarting).toBe(false)
        expect(stageIds(succeeded)).toEqual([
            'preparing-export:done',
            'writing-export:done',
            'finalizing-export:done',
            'complete:done',
        ])
        expect(succeeded.closeVisible).toBe(true)
        expect(succeeded.counters).toEqual([])

        const cancelled = buildNativeFileJobDialogModel(
            null,
            outcome({ kind: 'export', format: 'risu-save', state: 'cancelled', observedStages: ['preparing-export'] }),
            0,
        )
        expect(cancelled.terminal?.summary).toBe(copy.resultExportCancelled)

        const failed = buildNativeFileJobDialogModel(
            null,
            outcome({
                kind: 'export',
                format: 'risu-save',
                state: 'failed',
                observedStages: ['preparing-export', 'writing-export'],
                error: { code: 'store-error', message: 'disk full', recoveryRequired: false },
            }),
            0,
        )
        expect(failed.terminal?.summary).toBe(copy.resultExportFailed)
        expect(failed.terminal?.reason).toBe(copy.reasonStoreError)
        expect(failed.terminal?.summary).not.toContain('import')
    })

    it('keeps the rescue archive on its archive presentation', () => {
        const model = buildNativeFileJobDialogModel(
            running({ kind: 'export', format: 'raw-recovery', observedStages: ['reading-archive'] }),
            null,
            0,
        )
        expect(model.title).toBe(languageEnglish.risuNest.recovery.exportTitle)
        expect(model.stages.map((row) => row.stage)).toEqual(['reading-archive'])
    })

    it('formats bytes and elapsed time', () => {
        expect(formatBytes(0)).toBe('0 B')
        expect(formatBytes(1023)).toBe('1023 B')
        expect(formatBytes(1536)).toBe('1.5 KiB')
        expect(formatBytes(150 * 1024 * 1024)).toBe('150 MiB')
        expect(formatBytes(1.25 * 1024 ** 3)).toBe('1.3 GiB')
        expect(formatBytes(-1)).toBe('')
        expect(formatElapsed(0)).toBe('00:00')
        expect(formatElapsed(65_000)).toBe('01:05')
        expect(formatElapsed(3_725_000)).toBe('1:02:05')
        expect(fillTemplate('{0} / {1}', 'a', 2)).toBe('a / 2')
    })
})

it('uses compact content progress and keeps cancellation available during metadata mapping', () => {
    const state = running({
        format: 'content',
        observedStages: ['preparing-attachments'],
        status: status({
            kind: 'prepare-content-import',
            detail: detail(
                'preparing-attachments',
                { stageCompleted: 1250, stageTotal: 2500 },
                { assets: 1250, attachmentsPrepared: 1250 },
            ),
        }),
    })
    const model = buildNativeFileJobDialogModel(state, null, 5000)
    expect(model.compact).toBe(true)
    expect(model.overallPercent).toBe(50)
    expect(model.counters.map((row) => row.key)).toEqual(['assets'])
    state.status!.state = 'succeeded'
    state.status!.detail = detail('finalizing-staging')
    state.observedStages.push('finalizing-staging')
    expect(buildNativeFileJobDialogModel(state, null, 6000).cancelEnabled).toBe(
        true,
    )
})
