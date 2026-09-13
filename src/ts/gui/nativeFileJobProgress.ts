import { language } from 'src/lang'

import type { NativeFileOperationState } from '../storage/nativeFileJobManager'
import type { NativeFileJobStatus } from '../storage/nativeFileJobs'

type NativeFileJobPhase = NativeFileJobStatus['phase']

const TRANSFERRING_PHASES = new Set<NativeFileJobPhase>([
    'reading-source',
    'writing-export',
    'uploading-database',
])

const FINALIZING_PHASES = new Set<NativeFileJobPhase>([
    'activating-database',
    'publishing-destination',
    'finalizing-publication',
    'finalizing-export',
    'complete',
])

/** Localized phase word for a running native file job. Never exposes the internal phase code. */
export function nativeFileJobPhaseLabel(
    status: NativeFileJobStatus | undefined,
): string {
    if (!status) return ''
    // Imports read the selected file; only exports and uploads move data somewhere else.
    if (
        status.phase === 'reading-source' &&
        status.kind.startsWith('restore-')
    ) {
        return language.risuNest.backup.progressReading
    }
    if (TRANSFERRING_PHASES.has(status.phase))
        return language.risuNest.backup.progressTransferring
    if (FINALIZING_PHASES.has(status.phase))
        return language.risuNest.backup.progressFinalizing
    return language.risuNest.backup.progressPreparing
}

/** Localized phase word plus the transferred share when the job reports byte progress. */
export function nativeFileJobProgressText(
    status: NativeFileJobStatus | undefined,
): string {
    const label = nativeFileJobPhaseLabel(status)
    if (!status) return label
    const total = status.progress.totalBytes
    if (total && total > 0) {
        return `${label}: ${Math.min(100, Math.round((status.progress.completedBytes * 100) / total))}%`
    }
    const bytes = status.progress.completedBytes
    return bytes > 0
        ? `${label}: ${(bytes / (1024 * 1024)).toFixed(1)} MiB`
        : label
}

/**
 * Names the operation the user actually started. The shared operation store only carries the
 * import/export direction, so the job kind decides between RisuSave and local backup wording.
 */
export function nativeFileJobTitle(
    kind: NativeFileOperationState['kind'],
    status: NativeFileJobStatus | undefined,
): string {
    switch (status?.kind) {
        case 'restore-portable-backup':
            return language.portableBackup.restore
        case 'export-portable-backup':
            return language.portableBackup.export
        case 'export-compatible-local-backup':
            return language.portableBackup.report
        case 'restore-legacy-local-backup':
            return language.loadBackupLocal
        case 'export-legacy-local-backup':
            return language.saveBackupLocal
        default:
            return kind === 'import'
                ? language.importRisuSave
                : language.exportRisuSave
    }
}
