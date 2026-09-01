import type {
    NativeAssetGcResult,
    NativePeerBackupInfo,
    NativePeerTempUsage,
    NativePersistentStorageStats,
    NativeSnapshotCreated,
    NativeSnapshotInfo,
} from './nativePersistentMaintenance'
import type { SyncConflictBackupEntry } from './sync/syncConflictBackup'

export type RisuNestStorageCardId = 'total' | 'media' | 'inlays' | 'plugins' | 'snapshots' | 'conflictBackups'

export interface RisuNestStorageDashboardSnapshot {
    loading: boolean
    loadFailed: boolean
    busy: string | null
    stats: NativePersistentStorageStats | null
    snapshots: NativeSnapshotInfo[]
    conflictBackups: SyncConflictBackupEntry[]
    peerBackups: NativePeerBackupInfo[]
    tempUsage: NativePeerTempUsage | null
    gcPreview: NativeAssetGcResult | null
}

export interface RisuNestStorageDashboardDependencies {
    getStats(): Promise<NativePersistentStorageStats>
    listSnapshots(): Promise<NativeSnapshotInfo[]>
    listConflictBackups(): Promise<SyncConflictBackupEntry[]>
    listPeerBackups(): Promise<NativePeerBackupInfo[]>
    getTemp(): Promise<NativePeerTempUsage>
    cleanupTemp(): Promise<NativePeerTempUsage>
    previewGc(): Promise<NativeAssetGcResult>
    executeGc(): Promise<NativeAssetGcResult>
    deleteSnapshot(path: string): Promise<void>
    deleteConflictBackup(id: string): Promise<void>
    deletePeerBackup(path: string): Promise<void>
    createSnapshot(reason: string): Promise<NativeSnapshotCreated>
}

export function formatRisuNestStorageBytes(bytes: number): string {
    const mib = 1024 * 1024
    const gib = 1024 * mib
    if (bytes >= gib) return `${(bytes / gib).toFixed(1)} GiB`
    if (bytes >= mib) return `${(bytes / mib).toFixed(1)} MiB`
    if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`
    return `${Math.max(0, Math.round(bytes))} bytes`
}

function isInlay(alias: NativePersistentStorageStats['assetAliases'][number]): boolean {
    return alias.kind.toLowerCase().includes('inlay') || Boolean(alias.inlayType)
}

export function storageDashboardRollup(
    stats: NativePersistentStorageStats,
    snapshots: readonly NativeSnapshotInfo[],
    conflictBackups: readonly SyncConflictBackupEntry[],
    peerBackups: readonly NativePeerBackupInfo[],
): {
    cards: { id: RisuNestStorageCardId; bytes: number }[]
    counts: { characters: number; trashedCharacters: number; conversations: number; messages: number }
    snapshotBytes: number
    conflictBackupBytes: number
    peerBackupBytes: number
} {
    const snapshotBytes = snapshots.reduce((total, snapshot) => total + snapshot.bytes, 0)
    const conflictBackupBytes = conflictBackups.reduce((total, backup) => total + backup.byteLength, 0)
    const peerBackupBytes = peerBackups.reduce((total, backup) => total + backup.bytes, 0)
    const inlayBytes = stats.assetAliases
        .filter(isInlay)
        .reduce((total, alias) => total + alias.bytes, 0)
    return {
        cards: [
            { id: 'total', bytes: stats.databaseBytes + stats.assetObjects.bytes + snapshotBytes + conflictBackupBytes + peerBackupBytes },
            { id: 'media', bytes: stats.assetObjects.bytes },
            { id: 'inlays', bytes: inlayBytes },
            { id: 'plugins', bytes: stats.pluginStorage.bytes },
            { id: 'snapshots', bytes: snapshotBytes },
            { id: 'conflictBackups', bytes: conflictBackupBytes },
        ],
        counts: {
            characters: stats.characters.active.count,
            trashedCharacters: stats.characters.trashedCount,
            conversations: stats.conversations.count,
            messages: stats.conversations.messageCount,
        },
        snapshotBytes,
        conflictBackupBytes,
        peerBackupBytes,
    }
}

export function createRisuNestStorageDashboard(deps: RisuNestStorageDashboardDependencies) {
    let state: RisuNestStorageDashboardSnapshot = {
        loading: false, loadFailed: false, busy: null, stats: null,
        snapshots: [], conflictBackups: [], peerBackups: [], tempUsage: null, gcPreview: null,
    }
    const listeners = new Set<(snapshot: RisuNestStorageDashboardSnapshot) => void>()
    const publish = () => listeners.forEach((listener) => listener(state))
    const update = (next: Partial<RisuNestStorageDashboardSnapshot>) => {
        state = { ...state, ...next }
        publish()
    }
    const run = async <T>(busy: string, action: () => Promise<T>): Promise<T | undefined> => {
        if (state.busy || state.loading) return undefined
        update({ busy })
        try {
            return await action()
        } finally {
            update({ busy: null })
        }
    }
    const reload = async (): Promise<void> => {
        update({ loading: true, loadFailed: false })
        try {
            const [stats, snapshots, conflictBackups, peerBackups] = await Promise.all([
                deps.getStats(), deps.listSnapshots(), deps.listConflictBackups(), deps.listPeerBackups(),
            ])
            update({ stats, snapshots, conflictBackups, peerBackups, loadFailed: false })
        } catch {
            update({ loadFailed: true })
        } finally {
            update({ loading: false })
        }
    }

    return {
        snapshot: () => state,
        subscribe(listener: (snapshot: RisuNestStorageDashboardSnapshot) => void) {
            listeners.add(listener)
            listener(state)
            return () => listeners.delete(listener)
        },
        async load(): Promise<void> {
            if (state.busy || state.loading) return
            await reload()
        },
        async calculateTempSize() {
            return run('calculate-temp', async () => {
                const tempUsage = await deps.getTemp()
                update({ tempUsage })
                return tempUsage
            })
        },
        async cleanupTemp() {
            return run('cleanup-temp', async () => {
                const tempUsage = await deps.cleanupTemp()
                update({ tempUsage })
                return tempUsage
            })
        },
        async previewGc() {
            return run('preview-gc', async () => {
                const gcPreview = await deps.previewGc()
                update({ gcPreview })
                return gcPreview
            })
        },
        async executeGc() {
            return run('execute-gc', async () => {
                const result = await deps.executeGc()
                update({ gcPreview: null })
                await reload()
                return result
            })
        },
        async deleteSnapshot(path: string) {
            return run('delete-snapshot', async () => {
                await deps.deleteSnapshot(path)
                update({ snapshots: state.snapshots.filter((snapshot) => snapshot.path !== path) })
            })
        },
        async deleteConflictBackup(id: string) {
            return run('delete-conflict-backup', async () => {
                await deps.deleteConflictBackup(id)
                update({ conflictBackups: state.conflictBackups.filter((backup) => backup.id !== id) })
            })
        },
        async deletePeerBackup(path: string) {
            return run('delete-peer-backup', async () => {
                await deps.deletePeerBackup(path)
                update({ peerBackups: state.peerBackups.filter((backup) => backup.path !== path) })
            })
        },
        async createSnapshot() {
            return run('create-snapshot', async () => {
                await deps.createSnapshot('manual')
                await reload()
            })
        },
    }
}
