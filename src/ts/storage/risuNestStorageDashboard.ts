import { formatBytes } from '../gui/nativeFileJobDialogModel'
import type {
    NativeAssetGcResult,
    NativePersistentStorageStats,
    NativeSnapshotCreated,
    NativeSnapshotInfo,
} from './nativePersistentMaintenance'
import type { SyncConflictBackupEntry } from './sync/syncConflictBackup'
import type { AssetResidencyStatus } from './sync/serverAssetResidency'
import type {
    ServerSyncCacheUsage,
} from './sync/serverSyncProduction'

export type RisuNestStorageCardId =
    | 'total'
    | 'media'
    | 'inlays'
    | 'plugins'
    | 'snapshots'
    | 'conflictBackups'

export type StorageDashboardSource = 'stats' | 'snapshots' | 'conflictBackups' | 'tempUsage'

export interface RisuNestStorageDashboardSnapshot {
    failedSources: StorageDashboardSource[]
    loadedSources: StorageDashboardSource[]
    loading: boolean
    loadFailed: boolean
    busy: string[]
    stats: NativePersistentStorageStats | null
    snapshots: NativeSnapshotInfo[]
    conflictBackups: SyncConflictBackupEntry[]
    tempUsage: ServerSyncCacheUsage | null
    /** Where the files this device does not store are held, read after the totals. */
    residency: AssetResidencyStatus | null
    residencyLoading: boolean
    gcPreview: NativeAssetGcResult | null
    gcResult: NativeAssetGcResult | null
}

export interface RisuNestStorageDashboardDependencies {
    getStats(): Promise<NativePersistentStorageStats>
    listSnapshots(): Promise<NativeSnapshotInfo[]>
    listConflictBackups(): Promise<SyncConflictBackupEntry[]>
    getTemp(): Promise<ServerSyncCacheUsage>
    getResidency(): Promise<AssetResidencyStatus>
    cleanupTemp(): Promise<ServerSyncCacheUsage>
    previewGc(): Promise<NativeAssetGcResult>
    executeGc(): Promise<NativeAssetGcResult>
    deleteSnapshot(id: string): Promise<void>
    deleteConflictBackup(id: string): Promise<void>
    createSnapshot(reason: string): Promise<NativeSnapshotCreated>
}

export function formatRisuNestStorageBytes(bytes: number): string {
    return formatBytes(Math.max(0, bytes))
}

export type StorageOffDevicePart = 'server' | 'external' | 'unavailable'

/**
 * The places the asset residency status names for files this device does not
 * store, split the way the asset residency group splits them. Bytes for the
 * server and external storage, a file count for files no source holds.
 */
export function storageOffDeviceParts(
    residency: AssetResidencyStatus,
): { part: StorageOffDevicePart; value: number }[] {
    const externalBytes =
        residency.remoteObjects > residency.serverObjects
            ? residency.remoteBytes - residency.serverBytes
            : 0
    const parts: { part: StorageOffDevicePart; value: number }[] = [
        { part: 'server', value: residency.serverBytes },
        { part: 'external', value: externalBytes },
        { part: 'unavailable', value: residency.unavailableObjects },
    ]
    return parts.filter((entry) => entry.value > 0)
}

export function storageDashboardRollup(
    stats: NativePersistentStorageStats,
    conflictBackups: readonly SyncConflictBackupEntry[],
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
    cacheBytes: number
    ledgerBytes: number
} {
    const snapshotBytes = stats.snapshotBytes
    const conflictBackupBytes = conflictBackups.reduce(
        (total, backup) => total + backup.byteLength,
        0,
    )
    const cacheBytes = cache?.cacheBytes ?? 0
    const ledgerBytes = cache?.ledgerBytes ?? 0
    return {
        cards: [
            {
                id: 'total',
                bytes:
                    stats.databaseBytes +
                    stats.assetBodies.bytes +
                    snapshotBytes +
                    conflictBackupBytes +
                    cacheBytes +
                    ledgerBytes,
            },
            { id: 'media', bytes: stats.assetBodies.bytes },
            { id: 'inlays', bytes: stats.inlayBodies.bytes },
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
        cacheBytes,
        ledgerBytes,
    }
}

export function createRisuNestStorageDashboard(
    deps: RisuNestStorageDashboardDependencies,
) {
    let state: RisuNestStorageDashboardSnapshot = {
        loading: false,
        loadFailed: false,
        failedSources: [],
        loadedSources: [],
        busy: [],
        stats: null,
        snapshots: [],
        conflictBackups: [],
        tempUsage: null,
        residency: null,
        residencyLoading: false,
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
    // The residency status walks the whole library, so reloads share one request.
    let residencyRequest: Promise<AssetResidencyStatus> | null = null
    let latestResidency = 0
    const loadResidency = (stats: NativePersistentStorageStats) => {
        const residencyId = ++latestResidency
        if (stats.missingAssetBodies.count === 0) {
            update({ residency: null, residencyLoading: false })
            return
        }
        update({ residency: null, residencyLoading: true })
        residencyRequest ??= deps.getResidency().finally(() => {
            residencyRequest = null
        })
        residencyRequest.then(
            (residency) => {
                if (residencyId === latestResidency) update({ residency, residencyLoading: false })
            },
            () => {
                if (residencyId === latestResidency) update({ residency: null, residencyLoading: false })
            },
        )
    }
    const publishBusy = () => update({ busy: [...activeActions] })
    /** Returns whether a pending reload was set aside. */
    const invalidatePendingReloads = (): boolean => {
        if (pendingReloads === 0) return false
        latestReload += 1
        update({ loadFailed: true })
        return true
    }
    const run = async <T>(
        busy: string,
        action: () => Promise<T>,
        // An action on one list can run as soon as that list is shown, before the totals.
        source?: StorageDashboardSource,
    ): Promise<T | undefined> => {
        const ready = !(state.loading && !state.stats)
            || (source !== undefined && state.loadedSources.includes(source))
        if (activeActions.has(busy) || !ready)
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
        const failed: StorageDashboardSource[] = []
        // Each source shows as soon as it arrives, so the totals never wait for the snapshot list.
        const settle = <K extends StorageDashboardSource>(
            source: K,
            request: () => Promise<NonNullable<RisuNestStorageDashboardSnapshot[K]>>,
        ) => Promise.resolve(request()).then(
            (value) => {
                if (reloadId !== latestReload) return
                update({
                    ...({ [source]: value } as Partial<RisuNestStorageDashboardSnapshot>),
                    loadedSources: state.loadedSources.includes(source)
                        ? state.loadedSources
                        : [...state.loadedSources, source],
                })
                if (source === 'stats') loadResidency(value as NativePersistentStorageStats)
            },
            () => { failed.push(source) },
        )
        try {
            await Promise.all([
                settle('stats', () => deps.getStats()),
                settle('snapshots', () => deps.listSnapshots()),
                settle('conflictBackups', () => deps.listConflictBackups()),
                settle('tempUsage', () => deps.getTemp()),
            ])
            if (reloadId === latestReload) update({ failedSources: failed, loadFailed: failed.length > 0 })
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
            }, 'tempUsage')
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
            }, 'snapshots')
        },
        async deleteConflictBackup(id: string) {
            return run(`delete-conflict-backup:${id}`, async () => {
                await deps.deleteConflictBackup(id)
                const setAside = invalidatePendingReloads()
                update({
                    conflictBackups: state.conflictBackups.filter(
                        (backup) => backup.id !== id,
                    ),
                })
                // The reload set aside may not have delivered every source yet.
                if (setAside) await reload()
            }, 'conflictBackups')
        },
        async createSnapshot() {
            return run('create-snapshot', async () => {
                await deps.createSnapshot('manual')
                await reload()
            })
        },
    }
}
