import { invoke } from '@tauri-apps/api/core'
import { relaunch } from '@tauri-apps/plugin-process'
import { isTauriMobile } from '../platform'

const PERIODIC_SNAPSHOT_INTERVAL_MS = 24 * 60 * 60 * 1000

export type NativeCheckpointMode = 'passive' | 'truncate'

export interface NativeSnapshotInfo {
    path: string
    bytes: number
    modifiedAt: number
}

export interface NativeSnapshotCreated {
    path: string
    bytes: number
    durationMs: number
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
    return (window as Window & {
        RisuLifecycleBridge?: NativeRestartBridge
    }).RisuLifecycleBridge
}

export async function checkpointNativePersistentStore(
    mode: NativeCheckpointMode,
): Promise<void> {
    await invoke('pds_checkpoint', { mode })
}

export function createNativePersistentSnapshot(
    reason: string,
): Promise<NativeSnapshotCreated> {
    return invoke('pds_snapshot_create', { reason })
}

export function listNativePersistentSnapshots(): Promise<NativeSnapshotInfo[]> {
    return invoke('pds_snapshot_list')
}

export async function requestNativePersistentSnapshotRestore(
    path: string,
): Promise<void> {
    await invoke('pds_snapshot_restore_request', { path })
}

export async function restartNativeApp(): Promise<void> {
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
    if (now - newestModifiedAt < PERIODIC_SNAPSHOT_INTERVAL_MS) return null
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
}

export async function restoreNativePersistentSnapshot(
    actions: NativeSnapshotRestoreActions,
): Promise<boolean> {
    const snapshots = await listNativePersistentSnapshots()
    if (snapshots.length === 0) {
        await actions.onEmpty()
        return false
    }

    const path = await actions.choose(snapshots)
    if (path === null) return false
    if (!snapshots.some((snapshot) => snapshot.path === path)) {
        throw new Error('Selected native snapshot is not available')
    }
    if (!await actions.confirm()) return false

    await requestNativePersistentSnapshotRestore(path)
    await actions.restart()
    return true
}
