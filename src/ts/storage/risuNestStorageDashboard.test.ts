import { describe, expect, it, vi } from 'vitest'

import {
    createRisuNestStorageDashboard,
    formatRisuNestStorageBytes,
    storageDashboardRollup,
} from './risuNestStorageDashboard'

const stats = {
    snapshotBytes: 2 * 1024 * 1024,
    databaseBytes: 2 * 1024 * 1024,
    assetObjects: { count: 4, bytes: 3 * 1024 * 1024 },
    assetAliases: [
        { kind: 'inlay', inlayType: null, count: 1, bytes: 1024 * 1024 },
        { kind: 'image', inlayType: 'image', count: 1, bytes: 2 * 1024 * 1024 },
    ],
    pluginStorage: { count: 2, bytes: 512 * 1024 },
    characters: { active: { count: 3, bytes: 0 }, trashedCount: 1 },
    conversations: { count: 4, messageCount: 5 },
    assetObjectDeletions: [],
}

const snapshots = [
    {
        id: 'snapshot.db',
        reason: 'manual',
        reclaimableBytes: 0,
        bytes: 2 * 1024 * 1024,
        modifiedAt: 1,
    },
]
const conflictBackups = [
    {
        id: 'conflict',
        createdAt: 2,
        side: 'local' as const,
        characterCount: 1,
        byteLength: 1024 * 1024,
        scope: 'database-only' as const,
    },
]
const serverBackups = {
    items: [],
    next: null,
    completeCount: 105,
    completeBytes: 2 * 1024 * 1024,
    incompleteCount: 1,
    incompleteBytes: 1024 * 1024,
    diskBytes: 3 * 1024 * 1024,
}
const cacheUsage = (totalBytes: number) => ({
    totalBytes,
    protectedBytes: 0,
    reclaimableBytes: totalBytes,
    blockedReason: null,
})

describe('RisuNest storage dashboard view model', () => {
    it('counts archive storage once even when multiple snapshots restore the same large database', () => {
        const shared = [
            { ...snapshots[0], id: 'first', bytes: 100 * 1024 * 1024 },
            { ...snapshots[0], id: 'second', bytes: 100 * 1024 * 1024 },
        ]
        const rollup = storageDashboardRollup(
            { ...stats, snapshotBytes: 1024 },
            shared,
            [],
            null,
        )
        expect(rollup.snapshotBytes).toBe(1024)
        expect(rollup.cards.find((card) => card.id === 'total')?.bytes).toBe(
            stats.databaseBytes + stats.assetObjects.bytes + 1024,
        )
    })

    it('rolls up six overlapping logical cards and formats MiB and GiB', () => {
        const rollup = storageDashboardRollup(
            stats,
            snapshots,
            conflictBackups,
            serverBackups,
        )

        expect(rollup.cards).toEqual([
            { id: 'total', bytes: 11 * 1024 * 1024 },
            { id: 'media', bytes: 3 * 1024 * 1024 },
            { id: 'inlays', bytes: 3 * 1024 * 1024 },
            { id: 'plugins', bytes: 512 * 1024 },
            { id: 'snapshots', bytes: 2 * 1024 * 1024 },
            { id: 'conflictBackups', bytes: 1024 * 1024 },
        ])
        expect(rollup.counts).toEqual({ characters: 3, trashedCharacters: 1, conversations: 4, messages: 5 })
        expect(formatRisuNestStorageBytes(1024 * 1024)).toBe('1.0 MiB')
        expect(formatRisuNestStorageBytes(1024 * 1024 * 1024)).toBe('1.0 GiB')
    })

    it('loads complete backup and cache totals, and retries failures', async () => {
        const getStats = vi
            .fn()
            .mockRejectedValueOnce(new Error('offline'))
            .mockResolvedValue(stats)
        const getTemp = vi.fn().mockResolvedValue(cacheUsage(1024))
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi.fn().mockResolvedValue(snapshots),
            listConflictBackups: vi.fn().mockResolvedValue(conflictBackups),
            getServerBackups: vi.fn().mockResolvedValue(serverBackups),
            getTemp,
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        await dashboard.load()
        expect(dashboard.snapshot()).toMatchObject({
            loading: false,
            loadFailed: true,
        })
        expect(getTemp).toHaveBeenCalledOnce()

        await dashboard.load()
        expect(dashboard.snapshot()).toMatchObject({
            loadFailed: false,
            snapshots,
            conflictBackups,
            serverBackups,
        })

        await dashboard.calculateTempSize()
        expect(dashboard.snapshot().tempUsage).toEqual(cacheUsage(1024))
    })

    it('excludes duplicate loads and actions until a pending load settles', async () => {
        let resolveStats: ((value: typeof stats) => void) | undefined
        const getStats = vi.fn(() => new Promise<typeof stats>((resolve) => { resolveStats = resolve }))
        const getTemp = vi.fn()
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi.fn().mockResolvedValue(snapshots),
            listConflictBackups: vi.fn().mockResolvedValue(conflictBackups),
            getServerBackups: vi.fn().mockResolvedValue(serverBackups),
            getTemp,
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        const firstLoad = dashboard.load()
        await dashboard.load()
        await dashboard.calculateTempSize()
        expect(getStats).toHaveBeenCalledOnce()
        expect(getTemp).toHaveBeenCalledOnce()

        resolveStats?.(stats)
        await firstLoad
        expect(dashboard.snapshot()).toMatchObject({ loading: false, stats, loadFailed: false })
    })

    it('cleans temp storage and previews then executes garbage collection one operation at a time', async () => {
        let resolveCleanup: (() => void) | undefined
        const cleanupTemp = vi.fn(
            () =>
                new Promise<ReturnType<typeof cacheUsage>>((resolve) => {
                    resolveCleanup = () => resolve(cacheUsage(0))
                }),
        )
        const previewGc = vi.fn().mockResolvedValue({
            candidateCount: 2,
            candidateBytes: 1024,
            deletedCount: 0,
            deletedBytes: 0,
            blockers: [],
        })
        const executeGc = vi.fn().mockResolvedValue({
            candidateCount: 2,
            candidateBytes: 1024,
            deletedCount: 2,
            deletedBytes: 1024,
            blockers: [],
        })
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats),
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp: vi
                .fn()
                .mockResolvedValueOnce(cacheUsage(1024))
                .mockResolvedValueOnce(cacheUsage(0)),
            cleanupTemp,
            previewGc,
            executeGc,
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })
        await dashboard.load()

        const cleanup = dashboard.cleanupTemp()
        expect(dashboard.snapshot().busy).toContain('cleanup-temp')
        await dashboard.previewGc()
        expect(previewGc).toHaveBeenCalledOnce()
        resolveCleanup?.()
        await cleanup
        expect(dashboard.snapshot().tempUsage).toEqual(cacheUsage(0))

        await dashboard.previewGc()
        expect(dashboard.snapshot().gcPreview).toMatchObject({ candidateCount: 2, candidateBytes: 1024 })
        await dashboard.executeGc()
        expect(executeGc).toHaveBeenCalledOnce()
        expect(dashboard.snapshot()).toMatchObject({
            gcPreview: null,
            gcResult: { deletedCount: 2, deletedBytes: 1024 },
        })
    })

    it('refreshes remaining temp usage after cleanup instead of displaying removed bytes as current usage', async () => {
        const getTemp = vi
            .fn()
            .mockResolvedValueOnce(cacheUsage(4096))
            .mockResolvedValueOnce(cacheUsage(512))
        const cleanupTemp = vi.fn().mockResolvedValue(cacheUsage(3584))
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats),
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp,
            cleanupTemp,
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        await dashboard.calculateTempSize()
        await dashboard.cleanupTemp()

        expect(cleanupTemp).toHaveBeenCalledOnce()
        expect(getTemp).toHaveBeenCalledTimes(2)
        expect(dashboard.snapshot().tempUsage).toEqual(cacheUsage(512))
    })

    it('does not retain pre-cleanup usage when refreshing remaining temp usage fails', async () => {
        const getTemp = vi
            .fn()
            .mockResolvedValueOnce(cacheUsage(4096))
            .mockRejectedValueOnce(new Error('usage refresh failed'))
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats),
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp,
            cleanupTemp: vi.fn().mockResolvedValue(cacheUsage(4096)),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        await dashboard.calculateTempSize()
        await expect(dashboard.cleanupTemp()).rejects.toThrow('usage refresh failed')

        expect(dashboard.snapshot().tempUsage).toBeNull()
    })

    it('clears pre-cleanup usage when native cleanup fails after a possible partial deletion', async () => {
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats),
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp: vi.fn().mockResolvedValue(cacheUsage(4096)),
            cleanupTemp: vi
                .fn()
                .mockRejectedValue(new Error('partial cleanup failed')),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        await dashboard.calculateTempSize()
        await expect(dashboard.cleanupTemp()).rejects.toThrow('partial cleanup failed')

        expect(dashboard.snapshot().tempUsage).toBeNull()
    })

    it('blocks duplicate actions while allowing a different action to run', async () => {
        let resolveTemp:
            | ((value: ReturnType<typeof cacheUsage>) => void)
            | undefined
        const getTemp = vi.fn(
            () =>
                new Promise<ReturnType<typeof cacheUsage>>((resolve) => {
                    resolveTemp = resolve
                }),
        )
        const previewGc = vi.fn().mockResolvedValue({
            candidateCount: 0,
            candidateBytes: 0,
            deletedCount: 0,
            deletedBytes: 0,
            blockers: [],
        })
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats),
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp,
            cleanupTemp: vi.fn(),
            previewGc,
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        const calculation = dashboard.calculateTempSize()
        await dashboard.calculateTempSize()
        await dashboard.previewGc()

        expect(getTemp).toHaveBeenCalledOnce()
        expect(previewGc).toHaveBeenCalledOnce()
        expect(dashboard.snapshot().busy).toEqual(['calculate-temp'])
        resolveTemp?.(cacheUsage(1024))
        await calculation
        expect(dashboard.snapshot().busy).toEqual([])
    })

    it('marks retained totals stale when a post-action reload fails', async () => {
        const getStats = vi.fn().mockResolvedValueOnce(stats).mockRejectedValueOnce(new Error('reload failed'))
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn().mockResolvedValue({}),
        })
        await dashboard.load()
        await dashboard.createSnapshot()

        expect(dashboard.snapshot()).toMatchObject({ stats, loadFailed: true, loading: false })
    })

    it('keeps the stale marker visible while a retry is pending', async () => {
        let resolveRetry: ((value: typeof stats) => void) | undefined
        const getStats = vi.fn()
            .mockResolvedValueOnce(stats)
            .mockRejectedValueOnce(new Error('reload failed'))
            .mockImplementationOnce(() => new Promise<typeof stats>((resolve) => { resolveRetry = resolve }))
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn().mockResolvedValue({}),
        })
        await dashboard.load()
        await dashboard.createSnapshot()

        const retry = dashboard.load()
        expect(dashboard.snapshot()).toMatchObject({ stats, loadFailed: true, loading: true })
        resolveRetry?.(stats)
        await retry
        expect(dashboard.snapshot()).toMatchObject({ stats, loadFailed: false, loading: false })
    })

    it('keeps concurrent reloads loading and ignores an older response that finishes last', async () => {
        const pendingReloads: Array<(value: typeof stats) => void> = []
        const getStats = vi.fn()
            .mockResolvedValueOnce(stats)
            .mockImplementation(() => new Promise<typeof stats>((resolve) => { pendingReloads.push(resolve) }))
        const newerStats = { ...stats, databaseBytes: 9 * 1024 * 1024 }
        const olderStats = { ...stats, databaseBytes: 4 * 1024 * 1024 }
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi.fn().mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn().mockResolvedValue({}),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn().mockResolvedValue({}),
        })
        await dashboard.load()

        const snapshotAction = dashboard.createSnapshot()
        const gcAction = dashboard.executeGc()
        await vi.waitFor(() => expect(pendingReloads).toHaveLength(2))

        pendingReloads[1](newerStats)
        await vi.waitFor(() => expect(dashboard.snapshot().stats).toBe(newerStats))
        expect(dashboard.snapshot().loading).toBe(true)

        pendingReloads[0](olderStats)
        await Promise.all([snapshotAction, gcAction])
        expect(dashboard.snapshot()).toMatchObject({ stats: newerStats, loading: false, loadFailed: false })
    })

    it('does not restore a deleted row from a reload that was already pending', async () => {
        let resolveReload: ((value: typeof stats) => void) | undefined
        const getStats = vi.fn()
            .mockResolvedValueOnce(stats)
            .mockImplementationOnce(() => new Promise<typeof stats>((resolve) => { resolveReload = resolve }))
        getStats.mockResolvedValue({ ...stats, snapshotBytes: 0 })
        const deleteSnapshot = vi.fn().mockResolvedValue(undefined)
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi
                .fn()
                .mockResolvedValueOnce(snapshots)
                .mockResolvedValueOnce(snapshots)
                .mockResolvedValue([]),
            listConflictBackups: vi.fn().mockResolvedValue([]),
            getServerBackups: vi.fn().mockResolvedValue({
                ...serverBackups,
                diskBytes: 0,
                items: [],
            }),
            getTemp: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot,
            deleteConflictBackup: vi.fn(),
            deleteServerBackup: vi.fn(),
            exportServerBackup: vi.fn(),
            restoreServerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })
        await dashboard.load()

        const reload = dashboard.load()
        await vi.waitFor(() => expect(dashboard.snapshot().loading).toBe(true))
        await dashboard.deleteSnapshot('snapshot.db')
        expect(dashboard.snapshot().snapshots).toEqual([])

        resolveReload?.(stats)
        await reload
        expect(dashboard.snapshot()).toMatchObject({ snapshots: [], loadFailed: false, loading: false })
        expect(dashboard.snapshot().stats?.snapshotBytes).toBe(0)
    })
})
