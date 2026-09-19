import '../androidNativeControl'
import { invoke } from '@tauri-apps/api/core'
import { relaunch } from '@tauri-apps/plugin-process'
import { isTauriIOS, isTauriMobile } from '../platform'
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
    databaseBytes: number
    assetObjects: NativeStorageBytes
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
    state: 'deletable' | 'recent' | 'held'
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
    confirm(): Promise<boolean>
    restart(): Promise<void>
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

export function createNativePersistentSnapshot(reason: string): Promise<NativeSnapshotCreated> {
    return invoke('pds_snapshot_create', { reason })
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
    return invoke('pds_asset_gc_execute')
}

export function scanNativeDataHealth(): Promise<DataHealthResult> {
    return invoke('pds_data_health_scan')
}

export function deepScanNativeDataHealth(resume: boolean): Promise<DataHealthResult> {
    return invoke('pds_data_health_deep_scan', { resume })
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

export function previewNativeDataHealthRepair(selection: string[]): Promise<RepairPreview> {
    return invoke('pds_data_health_repair_preview', { selection })
}

export function applyNativeDataHealthRepair(
    selection: string[],
    snapshot: boolean,
): Promise<RepairApplied> {
    return invoke('pds_data_health_repair_apply', { selection, snapshot })
}

export function listNativeDataHealthJournals(): Promise<RepairJournalSummary[]> {
    return invoke('pds_data_health_journals')
}

export function undoNativeDataHealthRepair(journalId: string): Promise<RepairUndone> {
    return invoke('pds_data_health_undo', { journalId })
}

export async function requestNativePersistentSnapshotRestore(id: string): Promise<void> {
    await invoke('pds_snapshot_restore_request', { id })
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
    const snapshots = await listNativePersistentSnapshots()
    const newestModifiedAt = snapshots.reduce(
        (newest, snapshot) => Math.max(newest, snapshot.modifiedAt),
        Number.NEGATIVE_INFINITY,
    )
    if (
        newestModifiedAt <= now
        && now - newestModifiedAt < PERIODIC_SNAPSHOT_INTERVAL_MS
    ) return null
    return createNativePersistentSnapshot('periodic')
}

export function schedulePeriodicNativeSnapshot(): void {
    const run = () => {
        void createPeriodicNativeSnapshotIfDue().catch((error) => {
            console.error('Periodic native snapshot failed', error)
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
    if (!(await actions.confirm())) return false

    await requestNativePersistentSnapshotRestore(id)
    await actions.restart()
    return true
}
