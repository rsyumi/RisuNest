import { language } from 'src/lang'

import type {
    NativeFileOperationOutcome,
    NativeFileOperationState,
} from '../storage/nativeFileJobManager'
import type {
    NativeFileJobResult,
    NativeFileJobStage,
    NativeFileJobStatus,
    NativeFileOperationFormat,
    NativeImportCounts,
} from '../storage/nativeFileJobs'

/**
 * Pure view model for the shared import progress dialog. Everything the
 * component renders comes from here so the stage list, number formatting,
 * cancel availability, and terminal copy can be tested without a DOM.
 */

export type NativeFileJobDialogStage = Exclude<NativeFileJobStage, 'awaiting-activation'> | 'complete'
export type NativeFileJobDialogStageState = 'done' | 'active' | 'pending' | 'stopped'

export interface NativeFileJobDialogStageRow {
    stage: NativeFileJobDialogStage
    label: string
    state: NativeFileJobDialogStageState
    detail: string
}

export interface NativeFileJobDialogCounter {
    key: string
    label: string
    value: string
}

export interface NativeFileJobDialogTerminal {
    state: 'succeeded' | 'failed' | 'cancelled'
    summary: string
    reason: string
    details: string
    restarting: boolean
}

export interface NativeFileJobDialogModel {
    compact: boolean
    open: boolean
    title: string
    subtitle: string
    sourceName: string
    sourceSize: string
    elapsed: string
    overallPercent: number | null
    overallText: string
    indeterminate: boolean
    stages: NativeFileJobDialogStageRow[]
    currentItem: string
    counters: NativeFileJobDialogCounter[]
    warnings: string[]
    terminal: NativeFileJobDialogTerminal | null
    cancelVisible: boolean
    cancelEnabled: boolean
    cancelLabel: string
    cancelNote: string
    closeVisible: boolean
}

type DialogStageId = Exclude<NativeFileJobDialogStage, 'complete'>

const STAGE_ORDER: DialogStageId[] = [
    'copying-source',
    'awaiting-reselect',
    'reading-archive',
    'preparing-attachments',
    'reading-database',
    'decoding-database',
    'staging-characters',
    'finalizing-staging',
    'assign-plugin-values',
    'activating',
    'refreshing-app',
    'reloading-plugins',
    'restarting-app',
]

/** Stages shown as pending from the start; optional stages appear only once observed. */
const EXPECTED_STAGES: Record<NativeFileOperationFormat, DialogStageId[]> = {
    'raw-recovery': ['reading-archive'],
    'library-backup': [
        'reading-database',
        'finalizing-staging',
        'activating',
        'refreshing-app',
        'reloading-plugins',
    ],
    'conflict-reference': [
        'reading-database',
        'finalizing-staging',
        'activating',
        'refreshing-app',
        'reloading-plugins',
    ],
    content: ['reading-archive', 'preparing-attachments', 'finalizing-staging'],
    'local-backup': [
        'reading-archive',
        'preparing-attachments',
        'reading-database',
        'finalizing-staging',
        'activating',
        'refreshing-app',
        'reloading-plugins',
    ],
    'risu-save': [
        'reading-database',
        'finalizing-staging',
        'activating',
        'refreshing-app',
        'reloading-plugins',
    ],
}

const UNCANCELLABLE_STAGES = new Set<DialogStageId>([
    'activating', 'refreshing-app', 'reloading-plugins', 'restarting-app',
])

const CLOSED: NativeFileJobDialogModel = {
    compact: false,
    open: false,
    title: '',
    subtitle: '',
    sourceName: '',
    sourceSize: '',
    elapsed: '',
    overallPercent: null,
    overallText: '',
    indeterminate: false,
    stages: [],
    currentItem: '',
    counters: [],
    warnings: [],
    terminal: null,
    cancelVisible: false,
    cancelEnabled: false,
    cancelLabel: '',
    cancelNote: '',
    closeVisible: false,
}

export function fillTemplate(template: string, ...values: Array<string | number>): string {
    return values.reduce<string>(
        (text, value, index) => text.split(`{${index}}`).join(String(value)),
        template,
    )
}

export function formatBytes(bytes: number): string {
    if (!Number.isFinite(bytes) || bytes < 0) return ''
    if (bytes < 1024) return `${Math.round(bytes)} B`
    const units = ['KiB', 'MiB', 'GiB', 'TiB']
    let value = bytes / 1024
    let unit = 0
    while (value >= 1024 && unit < units.length - 1) {
        value /= 1024
        unit += 1
    }
    return `${value.toFixed(value >= 100 ? 0 : 1)} ${units[unit]}`
}

export function formatElapsed(milliseconds: number): string {
    const totalSeconds = Math.max(0, Math.floor(milliseconds / 1000))
    const seconds = totalSeconds % 60
    const minutes = Math.floor(totalSeconds / 60) % 60
    const hours = Math.floor(totalSeconds / 3600)
    const pad = (value: number) => String(value).padStart(2, '0')
    return hours > 0
        ? `${hours}:${pad(minutes)}:${pad(seconds)}`
        : `${pad(minutes)}:${pad(seconds)}`
}

function formatCount(value: number): string {
    return value.toLocaleString()
}

function normalizeStage(stage: NativeFileJobStage): DialogStageId {
    return stage === 'awaiting-activation' ? 'activating' : stage
}

/**
 * The job the native side reports names the file that is actually being
 * read. The operation's own format only says which admission rules applied
 * (one common picker admits every backup as a library backup), so it is the
 * fallback until the first status arrives.
 */
function formatOf(
    explicit: NativeFileOperationFormat | undefined,
    status: NativeFileJobStatus | undefined,
): NativeFileOperationFormat | undefined {
    switch (status?.kind) {
        case 'restore-block-risu-save':
            return 'risu-save'
        case 'restore-portable-backup':
            return 'library-backup'
        case 'restore-legacy-local-backup':
            return 'local-backup'
        default:
            return explicit
    }
}

function titleOf(format: NativeFileOperationFormat | undefined): string {
    const copy = language.risuNest.importDialog
    switch (format) {
        case 'risu-save':
            return copy.titleRisuSave
        case 'library-backup':
            return copy.titleBackup
        case 'local-backup':
            return copy.titleLocalBackup
        case 'raw-recovery':
            return language.risuNest.recovery.exportTitle
        default:
            return copy.titleImport
    }
}

function stageLabel(stage: NativeFileJobDialogStage): string {
    const copy = language.risuNest.importDialog
    switch (stage) {
        case 'copying-source': return copy.stageCopyingSource
        case 'awaiting-reselect': return copy.stageAwaitingReselect
        case 'reading-archive': return copy.stageReadingArchive
        case 'preparing-attachments': return copy.stagePreparingAttachments
        case 'reading-database': return copy.stageReadingDatabase
        case 'decoding-database': return copy.stageDecodingDatabase
        case 'staging-characters': return copy.stageStagingCharacters
        case 'finalizing-staging': return copy.stageFinalizingStaging
        case 'assign-plugin-values': return copy.stageAssignPluginValues
        case 'activating': return copy.stageActivating
        case 'refreshing-app': return copy.stageRefreshingApp
        case 'reloading-plugins': return copy.stageReloadingPlugins
        case 'restarting-app': return copy.stageRestartingApp
        case 'complete': return copy.stageComplete
    }
}

/** Observed stages since the most recent compatibility re-selection, normalized and deduplicated. */
function stageHistory(observed: NativeFileJobStage[]): DialogStageId[] {
    const lastReselect = observed.lastIndexOf('awaiting-reselect')
    const relevant = lastReselect >= 0 ? observed.slice(lastReselect) : observed
    const history: DialogStageId[] = []
    for (const stage of relevant) {
        const normalized = normalizeStage(stage)
        if (history.at(-1) !== normalized) history.push(normalized)
    }
    return history
}

function progressText(completed: number, total: number | undefined, unit: 'bytes' | 'items'): string {
    const copy = language.risuNest.importDialog
    if (unit === 'bytes') {
        if (total && total > 0) return fillTemplate(copy.overall, formatBytes(completed), formatBytes(total))
        return completed > 0 ? formatBytes(completed) : ''
    }
    if (total !== undefined && total > 0) {
        return fillTemplate(copy.itemsOf, formatCount(completed), formatCount(total))
    }
    return completed > 0 ? fillTemplate(copy.itemsCount, formatCount(completed)) : ''
}

function doneStageDetail(stage: DialogStageId, counts: NativeImportCounts | undefined): string {
    if (!counts) return ''
    const copy = language.risuNest.importDialog
    switch (stage) {
        case 'reading-archive':
            return counts.entriesRead > 0 ? fillTemplate(copy.itemsCount, formatCount(counts.entriesRead)) : ''
        case 'preparing-attachments':
            return counts.attachmentsPrepared > 0
                ? fillTemplate(copy.itemsCount, formatCount(counts.attachmentsPrepared))
                : ''
        case 'staging-characters':
            return counts.characters > 0 ? fillTemplate(copy.itemsCount, formatCount(counts.characters)) : ''
        default:
            return ''
    }
}

function activeStageDetail(stage: DialogStageId, status: NativeFileJobStatus | undefined): string {
    if (!status) return ''
    const detail = status.detail
    if (detail && normalizeStage(detail.stage) === stage) {
        return progressText(detail.stageCompleted, detail.stageTotal, detail.stageUnit)
    }
    if (detail) return ''
    return progressText(status.progress.completedBytes, status.progress.totalBytes, 'bytes')
}

function buildStages(
    format: NativeFileOperationFormat | undefined,
    observed: NativeFileJobStage[],
    status: NativeFileJobStatus | undefined,
    terminalState: NativeFileOperationOutcome['state'] | null,
): NativeFileJobDialogStageRow[] {
    const history = stageHistory(observed)
    const observedSet = new Set(history)
    const current = history.at(-1) ?? null
    const currentIndex = current ? STAGE_ORDER.indexOf(current) : -1
    const counts = status?.detail?.counts

    const rows: NativeFileJobDialogStageRow[] = []
    for (const stage of STAGE_ORDER) {
        const index = STAGE_ORDER.indexOf(stage)
        if (!observedSet.has(stage)) {
            if (!format || !EXPECTED_STAGES[format].includes(stage)) continue
            // An expected stage the job skipped past never happened; drop it instead of faking it.
            if (index <= currentIndex) continue
            if (terminalState) continue
        }
        let state: NativeFileJobDialogStageState
        if (terminalState === 'succeeded') state = 'done'
        else if (index < currentIndex) state = 'done'
        else if (index === currentIndex) state = terminalState ? 'stopped' : 'active'
        else state = 'pending'
        const detail = state === 'done'
            ? doneStageDetail(stage, counts)
            : state === 'active' || state === 'stopped'
                ? activeStageDetail(stage, status)
                : ''
        rows.push({ stage, label: stageLabel(stage), state, detail })
    }
    if (terminalState === 'succeeded' && current !== 'restarting-app') {
        rows.push({ stage: 'complete', label: stageLabel('complete'), state: 'done', detail: '' })
    }
    return rows
}

function counterValue(
    running: number,
    total: number | undefined,
    final: number | undefined,
): string {
    if (final !== undefined) return formatCount(final)
    if (total !== undefined) return fillTemplate(language.risuNest.importDialog.itemsOf, formatCount(running), formatCount(total))
    return running > 0 ? formatCount(running) : '–'
}

function buildCounters(
    format: NativeFileOperationFormat | undefined,
    counts: NativeImportCounts | undefined,
    result: NativeFileJobResult | undefined,
    succeeded: boolean,
): NativeFileJobDialogCounter[] {
    const copy = language.risuNest.importDialog
    if (format === 'content')
        return [
            {
                key: 'assets',
                label: copy.countAssets,
                value: counts ? formatCount(counts.attachmentsPrepared) : '–',
            },
        ]
    const finalResult = succeeded ? result : undefined
    const rows: NativeFileJobDialogCounter[] = [
        {
            key: 'characters',
            label: copy.countCharacters,
            value: counterValue(
                counts?.characters ?? 0,
                counts?.charactersTotal,
                finalResult?.characterCount,
            ),
        },
        {
            key: 'presets',
            label: copy.countPresets,
            value: counterValue(
                counts?.presets ?? 0,
                undefined,
                finalResult?.presetCount,
            ),
        },
    ]
    if (format === 'local-backup') {
        const known = (value: number | undefined) =>
            counts ? formatCount(value ?? 0) : '–'
        rows.push(
            {
                key: 'assets',
                label: copy.countAssets,
                value: known(counts?.assets),
            },
            {
                key: 'inlays',
                label: copy.countInlays,
                value: known(counts?.inlays),
            },
            {
                key: 'coldStorage',
                label: copy.countColdStorage,
                value: known(counts?.coldStorage),
            },
            {
                key: 'pocketMedia',
                label: copy.countPocketMedia,
                value: known(counts?.pocketMedia),
            },
            {
                key: 'skipped',
                label: copy.countSkipped,
                value: known(counts?.skipped),
            },
        )
    }
    return rows
}

function warningText(code: string): string {
    const copy = language.risuNest.importDialog
    switch (code) {
        case 'cleanup-failed':
            return copy.warningCleanupFailed
        case 'pocket-inlay-failed':
            return copy.warningPocketInlayFailed
        case 'partial-destination-may-remain':
            return language.screenshotPartialDestinationMayRemain
        default:
            return fillTemplate(copy.warningUnknown, code)
    }
}

export function failureReason(code: string): string {
    const copy = language.risuNest.importDialog
    switch (code) {
        case 'unsupported-format':
            return copy.reasonUnsupportedFormat
        case 'invalid-source':
        case 'invalid-input':
            return copy.reasonInvalidSource
        case 'truncated-input':
            return copy.reasonTruncated
        case 'corrupt-input':
            return copy.reasonCorrupt
        case 'revision-conflict':
            return copy.reasonRevisionConflict
        case 'store-error':
        case 'missing-result':
        case 'missing-activation-fence':
            return copy.reasonStoreError
        default:
            return copy.reasonUnknown
    }
}

function subtitleOf(
    format: NativeFileOperationFormat | undefined,
    counts: NativeImportCounts | undefined,
    terminal: boolean,
): string {
    if (format !== 'local-backup' || !counts) return ''
    const copy = language.risuNest.importDialog
    if (counts.pocketMedia + counts.pocketMetadata > 0) return copy.formatPocketRisu
    // A RisuAI backup is only certain once every entry has been classified.
    if (counts.entriesRead > 0 && (counts.entriesTotal !== undefined || terminal)) return copy.formatRisuAi
    return ''
}

function overallOf(
    status: NativeFileJobStatus | undefined,
    terminalState: NativeFileOperationOutcome['state'] | null,
): Pick<NativeFileJobDialogModel, 'overallPercent' | 'overallText' | 'indeterminate'> {
    const copy = language.risuNest.importDialog
    const progress = status?.progress
    const total = progress?.totalBytes
    const completed = progress?.completedBytes ?? 0
    if (terminalState === 'succeeded') {
        return {
            overallPercent: 100,
            overallText: total && total > 0 ? fillTemplate(copy.overall, formatBytes(total), formatBytes(total)) : '',
            indeterminate: false,
        }
    }
    if (total && total > 0) {
        return {
            overallPercent: Math.min(100, Math.round(completed * 100 / total)),
            overallText: fillTemplate(copy.overall, formatBytes(completed), formatBytes(total)),
            indeterminate: false,
        }
    }
    return {
        overallPercent: null,
        overallText: completed > 0 ? formatBytes(completed) : '',
        indeterminate: terminalState === null,
    }
}

function contentOverall(
    status?: NativeFileJobStatus,
): Pick<
    NativeFileJobDialogModel,
    'overallPercent' | 'overallText' | 'indeterminate'
> {
    const detail = status?.detail
    const total = detail?.stageTotal
    if (!total)
        return { overallPercent: null, overallText: '', indeterminate: true }
    const percent = Math.round((detail.stageCompleted / total) * 100)
    return {
        overallPercent: Math.min(99, percent),
        overallText:
            detail.stageUnit === 'items'
                ? fillTemplate(
                      language.risuNest.importDialog.itemsOf,
                      formatCount(detail.stageCompleted),
                      formatCount(total),
                  )
                : '',
        indeterminate: false,
    }
}

export function buildNativeFileJobDialogModel(
    state: NativeFileOperationState | null,
    outcome: NativeFileOperationOutcome | null,
    now: number,
): NativeFileJobDialogModel {
    const copy = language.risuNest.importDialog
    if (state?.presentation === 'dialog') {
        const format = formatOf(state.format, state.status)
        const history = stageHistory(state.observedStages)
        const current = history.at(-1) ?? null
        const counts = state.status?.detail?.counts
        // Waiting for activation is still cancellable; the native job only commits after finalize.
        const rawCurrent = state.observedStages.at(-1)
        const uncancellable =
            (current !== null &&
                rawCurrent !== 'awaiting-activation' &&
                UNCANCELLABLE_STAGES.has(current)) ||
            state.status?.phase === 'activating-database' ||
            (state.status?.state === 'succeeded' && format !== 'content')
        const cancelEnabled = !state.cancelRequested && !uncancellable
        const currentItem = state.status?.detail?.currentItem
        return {
            compact: format === 'content',
            open: true,
            title: titleOf(format),
            subtitle: subtitleOf(format, counts, false),
            sourceName: state.source?.name ?? '',
            sourceSize:
                state.source?.bytes !== undefined
                    ? formatBytes(state.source.bytes)
                    : '',
            elapsed: fillTemplate(
                copy.elapsed,
                formatElapsed(now - state.startedAt),
            ),
            ...overallOf(state.status, null),
            ...(format === 'content' ? contentOverall(state.status) : {}),
            stages: buildStages(
                format,
                state.observedStages,
                state.status,
                null,
            ),
            currentItem:
                currentItem &&
                (current === 'reading-archive' ||
                    current === 'preparing-attachments')
                    ? fillTemplate(copy.currentItem, currentItem)
                    : '',
            counters: buildCounters(
                format,
                counts,
                state.status?.result,
                false,
            ),
            warnings: (state.status?.warningCodes ?? []).map(warningText),
            terminal: null,
            cancelVisible: true,
            cancelEnabled,
            cancelLabel: state.cancelRequested ? copy.cancelling : copy.cancel,
            cancelNote:
                !cancelEnabled && !state.cancelRequested
                    ? copy.cancelUnavailable
                    : '',
            closeVisible: false,
        }
    }
    if (outcome) {
        const format = formatOf(outcome.format, outcome.status)
        const history = stageHistory(outcome.observedStages)
        const restarting =
            outcome.state === 'succeeded' && history.at(-1) === 'restarting-app'
        const counts = outcome.status?.detail?.counts
        let summary: string
        let reason = ''
        if (outcome.state === 'succeeded') {
            summary = restarting ? copy.resultRestarting : copy.resultSucceeded
        } else if (outcome.state === 'cancelled') {
            summary = outcome.partialWritesPossible
                ? copy.resultCancelledPartial
                : copy.resultCancelled
        } else if (outcome.error?.recoveryRequired) {
            summary = copy.resultFailedAfterCommit
        } else {
            summary = copy.resultFailed
            reason = failureReason(outcome.error?.code ?? '')
        }
        return {
            compact: format === 'content',
            open: true,
            title: titleOf(format),
            subtitle: subtitleOf(format, counts, true),
            sourceName: outcome.source?.name ?? '',
            sourceSize:
                outcome.source?.bytes !== undefined
                    ? formatBytes(outcome.source.bytes)
                    : '',
            elapsed: fillTemplate(
                copy.elapsed,
                formatElapsed(outcome.finishedAt - outcome.startedAt),
            ),
            ...overallOf(outcome.status, outcome.state),
            stages: buildStages(
                format,
                outcome.observedStages,
                outcome.status,
                outcome.state,
            ),
            currentItem: '',
            counters: buildCounters(
                format,
                counts,
                outcome.result,
                outcome.state === 'succeeded',
            ),
            warnings: outcome.warningCodes.map(warningText),
            terminal: {
                state: outcome.state,
                summary,
                reason,
                details: outcome.error
                    ? `[${outcome.error.code}] ${outcome.error.message}`
                    : '',
                restarting,
            },
            cancelVisible: false,
            cancelEnabled: false,
            cancelLabel: '',
            cancelNote: '',
            closeVisible: !restarting,
        }
    }
    return CLOSED
}
