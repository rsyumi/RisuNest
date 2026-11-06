import '../androidNativeControl'
import { invoke } from '@tauri-apps/api/core'
import { relaunch } from '../desktopRelaunch'
import { isTauriIOS, isTauriMobile } from '../platform'
import { NativeFileJobError, runNativeSnapshotRestore, type NativeFileJobStatus, type NativeSnapshotBodiesStarted, type NativeSnapshotBodyResult, type NativeSnapshotRestoreActivation } from './nativeFileJobs'
import { nativeFileOperationOutcomeShown, runSharedNativeFileOperation, waitForSnapshotBodyOutcomeDismissal } from './nativeFileJobManager'
import { runWithMobileBackgroundTask } from '../mobileBackgroundTask'
import { getDeviceSettings } from './deviceSettings'
import type {
    DataHealthResult,
    RepairApplied,
    RepairCandidate,
    RepairJournalSummary,
    RepairPreview,
    RepairUndone,
} from './dataHealth'

const PERIODIC_SNAPSHOT_INTERVAL_MS = 24 * 60 * 60 * 1000
const PERIODIC_SNAPSHOT_CHECK_INTERVAL_MS = 60 * 60 * 1000

export type NativeCheckpointMode = 'passive' | 'truncate'

export interface NativeSnapshotInfo {
    id: string
    reason: string
    reclaimableBytes: number
    bytes: number
    modifiedAt: number
}

export interface NativeSnapshotCreated {
    id: string
    revision: number
    bytes: number
    durationMs: number
}

export interface NativeStorageBytes {
    count: number
    bytes: number
}

export interface NativePersistentStorageStats {
    snapshotBytes: number
    /** Database, WAL and SHM file lengths, including reusable pages. */
    databaseBytes: number
    /** Free pages of the database files, which compaction returns. */
    databaseReclaimableBytes: number
    /** Every catalogued file, including those not stored on this device. */
    assetObjects: NativeStorageBytes
    /** The asset files this device stores. */
    assetBodies: NativeStorageBytes
    /** Catalogued files this device does not store. */
    missingAssetBodies: NativeStorageBytes
    /** The chat attachment files this device stores, each counted once. */
    inlayBodies: NativeStorageBytes
    assetAliases: NativeStorageAliasStats[]
    pluginStorage: NativeStorageBytes
    characters: { active: NativeStorageBytes; trashedCount: number }
    conversations: { count: number; messageCount: number }
    assetObjectDeletions: NativeStorageDeletionStats[]
}

export interface NativeStorageAliasStats extends NativeStorageBytes {
    kind: string
    inlayType: string | null
}

export interface NativeStorageDeletionStats extends NativeStorageBytes {
    state: string
}

/** One stored file the cleanup looked at, and what it decided about it. */
export interface NativeAssetGcCandidate {
    objectHash: string
    bytes: number
    createdAtMs: number
    state: 'deletable' | 'recent' | 'held' | 'blocked'
    /** What is holding a kept file. Empty with `held` means the library still uses it. */
    holders: string[]
}

export interface NativeAssetGcResult {
    candidateCount: number
    candidateBytes: number
    deletedCount: number
    deletedBytes: number
    blockers: string[]
    candidates?: NativeAssetGcCandidate[]
    omitted?: number
}

export interface NativeSnapshotRestoreActions {
    choose(snapshots: readonly NativeSnapshotInfo[]): Promise<string | null>
    onEmpty(): void | Promise<void>
}

interface NativeRestartBridge {
    requestRestart?: () => void
}

function nativeRestartBridge(): NativeRestartBridge | undefined {
    return (
        window as Window & {
            RisuLifecycleBridge?: NativeRestartBridge
        }
    ).RisuLifecycleBridge
}

export async function checkpointNativePersistentStore(mode: NativeCheckpointMode): Promise<void> {
    await invoke('pds_checkpoint', { mode })
}

/** User-started maintenance keeps running after the app leaves the screen. */
function runUserMaintenance<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    return runWithMobileBackgroundTask('maintenance', () => args === undefined ? invoke<T>(command) : invoke<T>(command, args), undefined, true)
}

// Every snapshot request carries the keep count this device shows, which the
// native rotation applies.
function snapshotKeepCount(): number {
    return getDeviceSettings().snapshotKeepCount
}

export function createNativePersistentSnapshot(reason: string): Promise<NativeSnapshotCreated> {
    return runUserMaintenance('pds_snapshot_create', { reason, keepCount: snapshotKeepCount() })
}

export function listNativePersistentSnapshots(): Promise<NativeSnapshotInfo[]> {
    return invoke('pds_snapshot_list')
}

export function getNativePersistentStorageStats(): Promise<NativePersistentStorageStats> {
    return invoke('pds_storage_stats')
}

export function deleteNativePersistentSnapshot(id: string): Promise<void> {
    return invoke('pds_snapshot_delete', { id })
}

export function previewNativePersistentAssetGc(): Promise<NativeAssetGcResult> {
    return invoke('pds_asset_gc_preview')
}

export function executeNativePersistentAssetGc(): Promise<NativeAssetGcResult> {
    return runUserMaintenance('pds_asset_gc_execute')
}

export function scanNativeDataHealth(): Promise<DataHealthResult> {
    return runUserMaintenance('pds_data_health_scan')
}

export function deepScanNativeDataHealth(resume: boolean): Promise<DataHealthResult> {
    return invoke('pds_data_health_deep_scan', { resume })
}

/** A deep scan runs page by page under one hold, so it keeps running off screen between pages. */
export function holdNativeDataHealthScan<T>(operation: (signal?: AbortSignal) => Promise<T>): Promise<T> {
    return runWithMobileBackgroundTask('maintenance', task => operation(task.signal), undefined, true)
}

export function getNativeDataHealthResult(): Promise<DataHealthResult | null> {
    return invoke('pds_data_health_result')
}

export async function cancelNativeDataHealthScan(): Promise<void> {
    await invoke('pds_data_health_cancel')
}

export function planNativeDataHealthRepair(): Promise<RepairCandidate[]> {
    return invoke('pds_data_health_repair_plan')
}

export function previewNativeDataHealthRepair(selection: string[], expectedScannedAt: number): Promise<RepairPreview> {
    return invoke('pds_data_health_repair_preview', { selection, expectedScannedAt })
}

export function applyNativeDataHealthRepair(
    selection: string[],
    snapshot: boolean,
    expectedRevision: number,
    expectedScannedAt: number,
): Promise<RepairApplied> {
    return runUserMaintenance('pds_data_health_repair_apply', { selection, snapshot, keepCount: snapshotKeepCount(), expectedRevision, expectedScannedAt })
}

export function discardNativeDataHealthIntent(
    finding: number,
    expectedRevision: number,
    expectedScannedAt: number,
): Promise<{ discarded: boolean; result: DataHealthResult }> {
    return invoke('pds_data_health_discard_intent', { finding, expectedRevision, expectedScannedAt })
}

export function completeNativeDataHealthIntent(
    finding: number, expectedRevision: number, expectedScannedAt: number,
): Promise<{ completed: boolean; revision: number; result: DataHealthResult }> {
    return runUserMaintenance('pds_data_health_complete_intent', { finding, expectedRevision, expectedScannedAt })
}

export function listNativeDataHealthJournals(): Promise<RepairJournalSummary[]> {
    return invoke('pds_data_health_journals')
}

export function undoNativeDataHealthRepair(journalId: string, expectedRevision: number): Promise<RepairUndone> {
    return runUserMaintenance('pds_data_health_undo', { journalId, expectedRevision })
}

export class NativeSnapshotBodiesCommittedError extends NativeFileJobError {
    constructor(public readonly receipt: Pick<NativeSnapshotBodiesStarted,'stagingId'|'activationRevision'|'bindingAuthority'> & Partial<Pick<NativeSnapshotBodiesStarted,'jobId'|'kind'>>, public readonly bodies: NativeSnapshotBodyResult | undefined, code: string, message: string) {
        super(code, message)
        this.name = 'NativeSnapshotBodiesCommittedError'
    }
}

export function snapshotBodyReceipt(status: NativeFileJobStatus): NativeSnapshotBodiesStarted {
    if (status.kind !== 'snapshot-bodies' || !status.jobId || !status.snapshotStagingId
        || !Number.isSafeInteger(status.activationRevision) || status.activationRevision! < 0
        || !status.activationAuthority || !/^(0|[1-9][0-9]*)$/.test(status.activationAuthority)) {
        throw new NativeFileJobError('snapshot-body-receipt-mismatch', 'Snapshot body transfer receipt differs')
    }
    return {jobId: status.jobId, kind: 'snapshot-bodies', stagingId: status.snapshotStagingId,
        activationRevision: String(status.activationRevision), bindingAuthority: status.activationAuthority}
}

export async function attachNativeSnapshotRestoreBodies(receipt: NativeSnapshotBodiesStarted, onProgress?: (percent: number | null) => void): Promise<NativeSnapshotBodyResult> {
    return runSharedNativeFileOperation('import', `snapshot-bodies:${receipt.stagingId}`, async context => {
        let cancelled = false
        while (true) {
            const status = await invoke<NativeFileJobStatus>('native_snapshot_restore_bodies_status', {receipt})
            const actual = snapshotBodyReceipt(status)
            if (Object.keys(receipt).some(key => receipt[key as keyof typeof receipt] !== actual[key as keyof typeof actual])) {
                throw new NativeSnapshotBodiesCommittedError(receipt, status.snapshotBodies, 'snapshot-body-receipt-mismatch', 'Snapshot library is restored, but the body transfer receipt differs')
            }
            context.onStatus(status)
            if (status.state === 'succeeded') {
                const bodies = status.snapshotBodies
                if (!bodies?.settled || bodies.stageId !== receipt.stagingId || String(bodies.activatedRevision) !== receipt.activationRevision || bodies.bindingAuthority !== receipt.bindingAuthority) {
                    throw new NativeSnapshotBodiesCommittedError(receipt, bodies, 'snapshot-body-result-mismatch', 'Snapshot library is restored, but body completion could not be confirmed')
                }
                return bodies
            }
            if (status.state === 'cancelled') throw new DOMException('Snapshot body transfer cancelled', 'AbortError')
            if (status.state === 'failed') {
                throw new NativeSnapshotBodiesCommittedError(receipt, status.snapshotBodies, status.error?.code ?? 'snapshot-bodies-incomplete', status.error?.message ?? 'Snapshot library is restored, but body transfer is incomplete')
            }
            if (context.signal.aborted && !cancelled) {
                cancelled = true
                await invoke('native_file_job_cancel', {jobId: receipt.jobId})
            }
            await new Promise(resolve => setTimeout(resolve, 100))
        }
    }, {presentation: 'dialog', format: 'library-backup', snapshotBodyOwner: receipt, onProgress})
}

const reattachedSnapshotJobs = new Set<string>()
export async function reattachNativeSnapshotRestoreBodies(jobs: NativeFileJobStatus[]): Promise<void> {
    // Native retry admission replaces the earlier terminal owner for the same stage.
    const latest = new Map<string, NativeFileJobStatus>()
    for (const job of jobs) if (job.snapshotStagingId) latest.set(job.snapshotStagingId, job)
    for (const job of latest.values()) {
        if (reattachedSnapshotJobs.has(job.jobId)) continue
        const receipt = snapshotBodyReceipt(job)
        reattachedSnapshotJobs.add(job.jobId)
        while (true) {
            await waitForSnapshotBodyOutcomeDismissal()
            try { await attachNativeSnapshotRestoreBodies(receipt); break }
            catch (error) {
                if (error instanceof Error && error.name === 'NativeFileOperationBusyError') continue
                if (!(error instanceof NativeSnapshotBodiesCommittedError) && !(error instanceof DOMException && error.name === 'AbortError')) {
                    console.error('Snapshot body receipt could not be reattached', error)
                }
                break
            }
        }
    }
}

export async function completeNativeSnapshotRestoreBodies(stagingId: string, activationRevision: number, bindingAuthority: string, previousJobId?: string, onProgress?: (percent: number | null) => void): Promise<NativeSnapshotBodyResult> {
    const identity = {stagingId, activationRevision: String(activationRevision), bindingAuthority}
    let receipt: NativeSnapshotBodiesStarted
    try { receipt = await invoke<NativeSnapshotBodiesStarted>('native_snapshot_restore_bodies_start', {...identity, ...(previousJobId ? {previousJobId} : {})}) }
    catch {
        // A lost start response does not authorize another restore or another worker.
        const jobs = await invoke<NativeFileJobStatus[]>('native_file_job_list').catch(() => [])
        const job = jobs.filter(job => job.kind === 'snapshot-bodies' && job.snapshotStagingId === stagingId
            && job.activationRevision === activationRevision && job.activationAuthority === bindingAuthority).at(-1)
        if (!job) throw new NativeSnapshotBodiesCommittedError(identity, undefined, 'snapshot-body-start-unknown', 'Snapshot library is restored, but body transfer could not be confirmed')
        receipt = snapshotBodyReceipt(job)
    }
    if (receipt.kind !== 'snapshot-bodies' || !receipt.jobId || receipt.stagingId !== stagingId
        || receipt.activationRevision !== identity.activationRevision || receipt.bindingAuthority !== bindingAuthority) {
        throw new NativeSnapshotBodiesCommittedError(identity, undefined, 'snapshot-body-receipt-mismatch', 'Snapshot library is restored, but the body transfer receipt differs')
    }
    return attachNativeSnapshotRestoreBodies(receipt, onProgress)
}

export async function retrySnapshotRestoreBodiesFromOutcome(): Promise<NativeSnapshotBodyResult> {
    const {get} = await import('svelte/store')
    const {nativeFileOperationOutcome} = await import('./nativeFileJobManager')
    const status = get(nativeFileOperationOutcome)?.status
    if (!status || !['failed', 'cancelled'].includes(status.state)) throw new NativeFileJobError('snapshot-body-retry-refused', 'Snapshot body retry is unavailable')
    const receipt = snapshotBodyReceipt(status)
    return completeNativeSnapshotRestoreBodies(receipt.stagingId, Number(receipt.activationRevision), receipt.bindingAuthority, receipt.jobId)
}

export async function requestNativePersistentSnapshotRestore(id: string): Promise<boolean> {
    const {getPersistentDataRuntime} = await import('./persistentDataRuntime.svelte')
    const runtime = getPersistentDataRuntime()
    // One hold spans both phases, so the body copy still starts after the app leaves the screen.
    return runWithMobileBackgroundTask('restore', async task => {
        const copyBodies = async (activation: NativeSnapshotRestoreActivation): Promise<void> => {
            task.progress(null)
            await completeNativeSnapshotRestoreBodies(activation.stagingId, activation.activationRevision, activation.bindingAuthority, undefined, task.progress)
        }
        let activation: NativeSnapshotRestoreActivation
        try {
            activation = await runSharedNativeFileOperation('import', `snapshot-restore:${id}`, context => runNativeSnapshotRestore(runtime, {snapshotId: id}, {
                signal: context.signal,
                onStatus: context.onStatus,
                onBlockingChange: context.setBlocking,
                afterActivationRecovery: copyBodies,
            }), {presentation: 'dialog', format: 'library-backup', onProgress: task.progress})
        } catch (error) {
            // The operation dialog already shows how an admitted restore ended.
            if ((error instanceof DOMException && error.name === 'AbortError') || nativeFileOperationOutcomeShown('import')) return false
            throw error
        }
        await copyBodies(activation)
        return true
    }, undefined, true)
}

export async function restartNativeApp(): Promise<void> {
    if (isTauriIOS) {
        await invoke('ios_prepare_restart')
        window.location.reload()
        return
    }
    if (!isTauriMobile) {
        await relaunch()
        return
    }

    const bridge = nativeRestartBridge()
    if (typeof bridge?.requestRestart !== 'function') {
        throw new Error('Android restart bridge is unavailable')
    }
    bridge.requestRestart()
}

export async function createPeriodicNativeSnapshotIfDue(
    now = Date.now(),
): Promise<NativeSnapshotCreated | null> {
    if (!getDeviceSettings().automaticSnapshotsEnabled) return null
    const snapshots = await listNativePersistentSnapshots()
    const newestModifiedAt = snapshots.reduce(
        (newest, snapshot) => Math.max(newest, snapshot.modifiedAt),
        Number.NEGATIVE_INFINITY,
    )
    if (
        newestModifiedAt <= now
        && now - newestModifiedAt < PERIODIC_SNAPSHOT_INTERVAL_MS
    ) return null
    // The switch may have been turned off while the list was read.
    const settings = getDeviceSettings()
    if (!settings.automaticSnapshotsEnabled) return null
    // Periodic work runs only while the app is visible, so it holds nothing.
    return invoke<NativeSnapshotCreated>('pds_snapshot_create', { reason: 'periodic', keepCount: settings.snapshotKeepCount })
}

export function sweepNativeMessageObjects(): Promise<void> {
    return invoke('pds_message_object_sweep')
}

export function schedulePeriodicNativeSnapshot(): void {
    const run = () => {
        void createPeriodicNativeSnapshotIfDue()
            .catch((error) => {
                console.error('Periodic native snapshot failed', error)
            })
            .then(() => sweepNativeMessageObjects())
            .catch((error) => {
                console.error('Periodic native object sweep failed', error)
            })
    }
    if (typeof globalThis.requestIdleCallback === 'function') {
        globalThis.requestIdleCallback(run)
    } else {
        globalThis.setTimeout(run, 0)
    }
    globalThis.setInterval(run, PERIODIC_SNAPSHOT_CHECK_INTERVAL_MS)
}

export async function restoreNativePersistentSnapshot(
    actions: NativeSnapshotRestoreActions,
): Promise<boolean> {
    const snapshots = await listNativePersistentSnapshots()
    if (snapshots.length === 0) {
        await actions.onEmpty()
        return false
    }

    const id = await actions.choose(snapshots)
    if (id === null) return false
    if (!snapshots.some((snapshot) => snapshot.id === id)) {
        throw new Error('Selected native snapshot is not available')
    }
    return requestNativePersistentSnapshotRestore(id)
}
