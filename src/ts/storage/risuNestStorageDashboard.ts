import type {
    NativeAssetGcResult,
    NativePersistentStorageStats,
    NativeSnapshotCreated,
    NativeSnapshotInfo,
} from './nativePersistentMaintenance'
import type { SyncConflictBackupEntry } from './sync/syncConflictBackup'
import type {
    ServerSyncBackupCursor,
    ServerSyncBackupInventory,
    ServerSyncCacheUsage,
} from './sync/serverSyncProduction'

export interface ServerSyncBackupDeleteResult {
    localDeleted: true
    cleanup: 'complete' | 'pending'
}

export type RisuNestStorageCardId =
    | 'total'
    | 'media'
    | 'inlays'
    | 'plugins'
    | 'snapshots'
    | 'conflictBackups'

export interface RisuNestStorageDashboardSnapshot {
    loading: boolean
    loadFailed: boolean
    busy: string[]
    stats: NativePersistentStorageStats | null
    snapshots: NativeSnapshotInfo[]
    conflictBackups: SyncConflictBackupEntry[]
    serverBackups: ServerSyncBackupInventory | null
    tempUsage: ServerSyncCacheUsage | null
    gcPreview: NativeAssetGcResult | null
    gcResult: NativeAssetGcResult | null
}

export interface RisuNestStorageDashboardDependencies {
    getStats(): Promise<NativePersistentStorageStats>
    listSnapshots(): Promise<NativeSnapshotInfo[]>
    listConflictBackups(): Promise<SyncConflictBackupEntry[]>
    /** The newest page, or the page before `before` when a cursor is given. */
    getServerBackups(before?: ServerSyncBackupCursor): Promise<ServerSyncBackupInventory>
    getTemp(): Promise<ServerSyncCacheUsage>
    cleanupTemp(): Promise<ServerSyncCacheUsage>
    previewGc(): Promise<NativeAssetGcResult>
    executeGc(): Promise<NativeAssetGcResult>
    deleteSnapshot(id: string): Promise<void>
    deleteConflictBackup(id: string): Promise<void>
    deleteServerBackup(id: string): Promise<ServerSyncBackupDeleteResult>
    exportServerBackup(id: string, side: 'local' | 'remote'): Promise<void>
    restoreServerBackup(id: string, side: 'local' | 'remote'): Promise<void>
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

function isInlay(
    alias: NativePersistentStorageStats['assetAliases'][number],
): boolean {
    return (
        alias.kind.toLowerCase().includes('inlay') || Boolean(alias.inlayType)
    )
}

export function storageDashboardRollup(
    stats: NativePersistentStorageStats,
    _snapshots: readonly NativeSnapshotInfo[],
    conflictBackups: readonly SyncConflictBackupEntry[],
    serverBackups: ServerSyncBackupInventory | null,
    cache: ServerSyncCacheUsage | null = null,
): {
    cards: { id: RisuNestStorageCardId; bytes: number }[]
    counts: {
        characters: number
        trashedCharacters: number
        conversations: number
        messages: number
    }
    snapshotBytes: number
    conflictBackupBytes: number
    serverBackupBytes: number
    cacheBytes: number
} {
    const snapshotBytes = stats.snapshotBytes
    const conflictBackupBytes = conflictBackups.reduce(
        (total, backup) => total + backup.byteLength,
        0,
    )
    const serverBackupBytes = serverBackups?.diskBytes ?? 0
    const cacheBytes = cache?.totalBytes ?? 0
    const inlayBytes = stats.assetAliases
        .filter(isInlay)
        .reduce((total, alias) => total + alias.bytes, 0)
    return {
        cards: [
            {
                id: 'total',
                bytes:
                    stats.databaseBytes +
                    stats.assetObjects.bytes +
                    snapshotBytes +
                    conflictBackupBytes +
                    serverBackupBytes +
                    cacheBytes,
            },
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
        serverBackupBytes,
        cacheBytes,
    }
}

export function createRisuNestStorageDashboard(
    deps: RisuNestStorageDashboardDependencies,
) {
    let state: RisuNestStorageDashboardSnapshot = {
        loading: false,
        loadFailed: false,
        busy: [],
        stats: null,
        snapshots: [],
        conflictBackups: [],
        serverBackups: null,
        tempUsage: null,
        gcPreview: null,
        gcResult: null,
    }
    const listeners = new Set<
        (snapshot: RisuNestStorageDashboardSnapshot) => void
    >()
    const publish = () => listeners.forEach((listener) => listener(state))
    const update = (next: Partial<RisuNestStorageDashboardSnapshot>) => {
        state = { ...state, ...next }
        publish()
    }
    const activeActions = new Set<string>()
    let latestReload = 0
    let pendingReloads = 0
    const publishBusy = () => update({ busy: [...activeActions] })
    const invalidatePendingReloads = () => {
        if (pendingReloads === 0) return
        latestReload += 1
        update({ loadFailed: true })
    }
    const run = async <T>(
        busy: string,
        action: () => Promise<T>,
    ): Promise<T | undefined> => {
        if (activeActions.has(busy) || (state.loading && !state.stats))
            return undefined
        activeActions.add(busy)
        publishBusy()
        try {
            return await action()
        } finally {
            activeActions.delete(busy)
            publishBusy()
        }
    }
    const reload = async (): Promise<void> => {
        const reloadId = ++latestReload
        pendingReloads += 1
        update({ loading: true })
        try {
            const [
                stats,
                snapshots,
                conflictBackups,
                serverBackups,
                tempUsage,
            ] = await Promise.all([
                deps.getStats(),
                deps.listSnapshots(),
                deps.listConflictBackups(),
                deps.getServerBackups(),
                deps.getTemp(),
            ])
            if (reloadId === latestReload)
                update({
                    stats,
                    snapshots,
                    conflictBackups,
                    serverBackups,
                    tempUsage,
                    loadFailed: false,
                })
        } catch {
            if (reloadId === latestReload) update({ loadFailed: true })
        } finally {
            pendingReloads -= 1
            update({ loading: pendingReloads > 0 })
        }
    }

    return {
        snapshot: () => state,
        subscribe(
            listener: (snapshot: RisuNestStorageDashboardSnapshot) => void,
        ) {
            listeners.add(listener)
            listener(state)
            return () => listeners.delete(listener)
        },
        async load(): Promise<void> {
            if (state.loading) return
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
                update({ tempUsage: null })
                await deps.cleanupTemp()
                const tempUsage = await deps.getTemp()
                update({ tempUsage })
                return tempUsage
            })
        },
        async previewGc() {
            return run('preview-gc', async () => {
                const gcPreview = await deps.previewGc()
                update({ gcPreview, gcResult: null })
                return gcPreview
            })
        },
        async executeGc() {
            return run('execute-gc', async () => {
                const result = await deps.executeGc()
                update({ gcPreview: null, gcResult: result })
                await reload()
                return result
            })
        },
        async deleteSnapshot(id: string) {
            return run(`delete-snapshot:${id}`, async () => {
                await deps.deleteSnapshot(id)
                invalidatePendingReloads()
                update({
                    snapshots: state.snapshots.filter(
                        (snapshot) => snapshot.id !== id,
                    ),
                })
                await reload()
            })
        },
        async deleteConflictBackup(id: string) {
            return run(`delete-conflict-backup:${id}`, async () => {
                await deps.deleteConflictBackup(id)
                invalidatePendingReloads()
                update({
                    conflictBackups: state.conflictBackups.filter(
                        (backup) => backup.id !== id,
                    ),
                })
            })
        },
        async deleteServerBackup(id: string) {
            return run(`delete-server-backup:${id}`, async () => {
                const result = await deps.deleteServerBackup(id)
                invalidatePendingReloads()
                await reload()
                return result
            })
        },
        async restoreServerBackup(id: string, side: 'local' | 'remote') {
            return run(`restore-server-backup:${id}:${side}`, async () => {
                await deps.restoreServerBackup(id, side)
                invalidatePendingReloads()
                await reload()
            })
        },
        async exportServerBackup(id: string, side: 'local' | 'remote') {
            return run(`export-server-backup:${id}:${side}`, () =>
                deps.exportServerBackup(id, side),
            )
        },
        /** Appends the page before the last loaded one to the server backup list. */
        async loadMoreServerBackups() {
            return run('more-server-backups', async () => {
                const current = state.serverBackups
                if (!current?.next) return
                const older = await deps.getServerBackups(current.next)
                if (state.serverBackups !== current) return
                update({
                    serverBackups: {
                        ...older,
                        items: [...current.items, ...older.items],
                    },
                })
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
