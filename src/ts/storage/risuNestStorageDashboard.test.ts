import { describe, expect, it, vi } from 'vitest'

import { formatBytes } from '../gui/nativeFileJobDialogModel'
import {
    createRisuNestStorageDashboard,
    formatRisuNestStorageBytes,
    storageDashboardRollup,
    storageOffDeviceParts,
} from './risuNestStorageDashboard'
import type { AssetResidencyStatus } from './sync/serverAssetResidency'
import { assetResidencyStatus } from './sync/serverAssetResidency.testSupport'

const stats = {
    snapshotBytes: 2 * 1024 * 1024,
    databaseBytes: 2 * 1024 * 1024,
    databaseReclaimableBytes: 0,
    assetObjects: { count: 4, bytes: 3 * 1024 * 1024 },
    assetBodies: { count: 4, bytes: 3 * 1024 * 1024 },
    missingAssetBodies: { count: 0, bytes: 0 },
    inlayBodies: { count: 2, bytes: 3 * 1024 * 1024 },
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
const cacheUsage = (totalBytes: number) => ({
    totalBytes,
    cacheBytes: totalBytes,
    protectedBytes: 0,
    reclaimableBytes: totalBytes,
    ledgerBytes: 0,
    databaseBytes: 0,
    blockedReason: null,
})

describe('RisuNest storage dashboard view model', () => {
    it('rolls up six overlapping logical cards and formats MiB and GiB', () => {
        const rollup = storageDashboardRollup(
            stats,
            conflictBackups,
        )

        expect(rollup.cards).toEqual([
            { id: 'total', bytes: 8 * 1024 * 1024 },
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

    it('counts only the files this device stores as images and media and in the total', () => {
        const offloaded = {
            ...stats,
            assetObjects: { count: 4, bytes: 1024 * 1024 * 1024 },
            assetBodies: { count: 1, bytes: 1024 * 1024 },
            missingAssetBodies: { count: 3, bytes: 1023 * 1024 * 1024 },
            inlayBodies: { count: 1, bytes: 512 * 1024 },
        }
        const rollup = storageDashboardRollup(offloaded, [], null)
        const card = (id: string) => rollup.cards.find((entry) => entry.id === id)?.bytes
        expect(card('media')).toBe(1024 * 1024)
        expect(card('total')).toBe(offloaded.databaseBytes + 1024 * 1024 + offloaded.snapshotBytes)
        // Chat attachments stay a part of images and media.
        expect(card('inlays')).toBe(512 * 1024)
        expect(card('inlays')!).toBeLessThanOrEqual(card('media')!)
    })

    it('names the server, external storage and unfound files the way the asset residency group does', () => {
        expect(storageOffDeviceParts(assetResidencyStatus({ remoteBytes: 900, remoteObjects: 3, serverBytes: 900, serverObjects: 3 })))
            .toEqual([{ part: 'server', value: 900 }])
        expect(storageOffDeviceParts(assetResidencyStatus({
            remoteBytes: 1000, remoteObjects: 4, serverBytes: 900, serverObjects: 3, unavailableObjects: 2,
            externalObjects: [{ connectionId: 'synthetic', objects: 1 }],
        }))).toEqual([
            { part: 'server', value: 900 },
            { part: 'external', value: 100 },
            { part: 'unavailable', value: 2 },
        ])
        expect(storageOffDeviceParts(assetResidencyStatus({ localBytes: 500 }))).toEqual([])
    })

    it('prints sizes in the shared file size format', () => {
        for (const value of [0, 512, 1536, 150 * 1024 * 1024, 2.5 * 1024 ** 4]) {
            expect(formatRisuNestStorageBytes(value)).toBe(formatBytes(value))
        }
        expect(formatRisuNestStorageBytes(0)).toBe('0 B')
        expect(formatRisuNestStorageBytes(-1)).toBe('0 B')
    })

    it('keeps the asset storage records apart from the temporary files but in the total', () => {
        const rollup = storageDashboardRollup(stats, [], {
            ...cacheUsage(3072),
            totalBytes: 3072 + 1024,
            ledgerBytes: 1024,
        })

        expect(rollup.cacheBytes).toBe(3072)
        expect(rollup.ledgerBytes).toBe(1024)
        expect(rollup.cards.find((card) => card.id === 'total')?.bytes).toBe(
            stats.databaseBytes + stats.assetBodies.bytes + stats.snapshotBytes + 3072 + 1024,
        )
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
            getTemp,
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp,
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp: vi
                .fn()
                .mockResolvedValueOnce(cacheUsage(1024))
                .mockResolvedValueOnce(cacheUsage(0)),
            getResidency: vi.fn(),
            cleanupTemp,
            previewGc,
            executeGc,
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp,
            getResidency: vi.fn(),
            cleanupTemp,
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp,
            getResidency: vi.fn(),
            cleanupTemp: vi.fn().mockResolvedValue(cacheUsage(4096)),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp: vi.fn().mockResolvedValue(cacheUsage(4096)),
            getResidency: vi.fn(),
            cleanupTemp: vi
                .fn()
                .mockRejectedValue(new Error('partial cleanup failed')),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp,
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc,
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp: vi.fn(),
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp: vi.fn(),
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp: vi.fn(),
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn().mockResolvedValue({}),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
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
            getTemp: vi.fn(),
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot,
            deleteConflictBackup: vi.fn(),
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

    it('runs a list action once its list has arrived, before the totals', async () => {
        const deleteSnapshot = vi.fn().mockResolvedValue(undefined)
        const deleteConflictBackup = vi.fn().mockResolvedValue(undefined)
        const cleanupTemp = vi.fn().mockResolvedValue(cacheUsage(0))
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn(() => new Promise<typeof stats>(() => {})),
            listSnapshots: vi.fn().mockResolvedValue(snapshots),
            listConflictBackups: vi.fn().mockResolvedValue(conflictBackups),
            getTemp: vi.fn().mockResolvedValue(cacheUsage(1024)),
            getResidency: vi.fn(),
            cleanupTemp,
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot,
            deleteConflictBackup,
            createSnapshot: vi.fn(),
        })
        void dashboard.load()
        await vi.waitFor(() => expect(dashboard.snapshot().loadedSources).toHaveLength(3))

        await dashboard.cleanupTemp()
        // Both deletions reload, and the totals in this test never arrive.
        void dashboard.deleteConflictBackup('conflict')
        void dashboard.deleteSnapshot('snapshot.db')
        await vi.waitFor(() => expect(deleteSnapshot).toHaveBeenCalledWith('snapshot.db'))
        expect(deleteConflictBackup).toHaveBeenCalledWith('conflict')
        expect(cleanupTemp).toHaveBeenCalledOnce()
    })

    it('finishes loading the other sources when a conflict backup is deleted during a load', async () => {
        let resolveFirstList: ((value: typeof snapshots) => void) | undefined
        const listSnapshots = vi.fn()
            .mockImplementationOnce(() => new Promise<typeof snapshots>((resolve) => { resolveFirstList = resolve }))
            .mockResolvedValue(snapshots)
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats),
            listSnapshots,
            listConflictBackups: vi.fn().mockResolvedValueOnce(conflictBackups).mockResolvedValue([]),
            getTemp: vi.fn().mockResolvedValue(cacheUsage(1024)),
            getResidency: vi.fn(),
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn().mockResolvedValue(undefined),
            createSnapshot: vi.fn(),
        })
        const load = dashboard.load()
        await vi.waitFor(() => expect(dashboard.snapshot().loadedSources).toContain('conflictBackups'))

        await dashboard.deleteConflictBackup('conflict')
        resolveFirstList?.(snapshots)
        await load
        await vi.waitFor(() => expect(dashboard.snapshot().loading).toBe(false))
        expect(dashboard.snapshot()).toMatchObject({ snapshots, conflictBackups: [], loadFailed: false })
        expect(dashboard.snapshot().loadedSources).toContain('snapshots')
    })

    describe('files this device does not store', () => {
        const offloaded = { ...stats, missingAssetBodies: { count: 3, bytes: 3 * 1024 } }
        const residency = assetResidencyStatus({ remoteBytes: 3 * 1024, remoteObjects: 3, serverBytes: 3 * 1024, serverObjects: 3 })
        const create = (getStats: () => Promise<typeof stats>, getResidency: () => Promise<AssetResidencyStatus>) =>
            createRisuNestStorageDashboard({
                getStats,
                listSnapshots: vi.fn().mockResolvedValue([]),
                listConflictBackups: vi.fn().mockResolvedValue([]),
                getTemp: vi.fn().mockResolvedValue(cacheUsage(0)),
                getResidency,
                cleanupTemp: vi.fn(),
                previewGc: vi.fn(),
                executeGc: vi.fn(),
                deleteSnapshot: vi.fn(),
                deleteConflictBackup: vi.fn(),
                createSnapshot: vi.fn(),
            })

        it('reads where they are after the totals load, without holding the load', async () => {
            let resolveResidency: ((value: AssetResidencyStatus) => void) | undefined
            const getResidency = vi.fn(() => new Promise<AssetResidencyStatus>((resolve) => { resolveResidency = resolve }))
            const dashboard = create(vi.fn().mockResolvedValue(offloaded), getResidency)

            await dashboard.load()
            expect(dashboard.snapshot()).toMatchObject({ loading: false, stats: offloaded, residency: null, residencyLoading: true })
            expect(getResidency).toHaveBeenCalledOnce()

            resolveResidency?.(residency)
            await vi.waitFor(() => expect(dashboard.snapshot()).toMatchObject({ residency, residencyLoading: false }))
        })

        it('does not ask when every catalogued file is on this device', async () => {
            const getResidency = vi.fn()
            const dashboard = create(vi.fn().mockResolvedValue(stats), getResidency)

            await dashboard.load()
            expect(getResidency).not.toHaveBeenCalled()
            expect(dashboard.snapshot()).toMatchObject({ residency: null, residencyLoading: false })
        })

        it('shares one pending request between reloads and keeps the newest answer', async () => {
            let resolveResidency: ((value: AssetResidencyStatus) => void) | undefined
            const getResidency = vi.fn(() => new Promise<AssetResidencyStatus>((resolve) => { resolveResidency = resolve }))
            const dashboard = create(vi.fn().mockResolvedValue(offloaded), getResidency)

            await dashboard.load()
            await dashboard.load()
            expect(getResidency).toHaveBeenCalledOnce()
            resolveResidency?.(residency)
            await vi.waitFor(() => expect(dashboard.snapshot().residency).toBe(residency))

            const next = { ...residency, serverBytes: 1024 }
            getResidency.mockImplementationOnce(() => Promise.resolve(next))
            await dashboard.load()
            expect(getResidency).toHaveBeenCalledTimes(2)
            await vi.waitFor(() => expect(dashboard.snapshot().residency).toBe(next))
        })

        it('clears the answer once a reload finds every file on this device', async () => {
            const getStats = vi.fn().mockResolvedValueOnce(offloaded).mockResolvedValue(stats)
            const dashboard = create(getStats, vi.fn().mockResolvedValue(residency))

            await dashboard.load()
            await vi.waitFor(() => expect(dashboard.snapshot().residency).toBe(residency))
            await dashboard.load()
            expect(dashboard.snapshot()).toMatchObject({ residency: null, residencyLoading: false })
        })

        it('applies the answer after a pending reload was set aside', async () => {
            let resolveResidency: ((value: AssetResidencyStatus) => void) | undefined
            const getResidency = vi.fn(() => new Promise<AssetResidencyStatus>((resolve) => { resolveResidency = resolve }))
            const getStats = vi.fn()
                .mockResolvedValueOnce(offloaded)
                .mockImplementationOnce(() => new Promise<typeof stats>(() => {}))
                .mockResolvedValue(offloaded)
            const dashboard = createRisuNestStorageDashboard({
                getStats,
                listSnapshots: vi.fn().mockResolvedValue([]),
                listConflictBackups: vi.fn().mockResolvedValue([conflictBackups[0]]),
                getTemp: vi.fn().mockResolvedValue(cacheUsage(0)),
                getResidency,
                cleanupTemp: vi.fn(),
                previewGc: vi.fn(),
                executeGc: vi.fn(),
                deleteSnapshot: vi.fn(),
                deleteConflictBackup: vi.fn().mockResolvedValue(undefined),
                createSnapshot: vi.fn(),
            })
            await dashboard.load()
            void dashboard.load()
            await vi.waitFor(() => expect(dashboard.snapshot().loading).toBe(true))
            await dashboard.deleteConflictBackup('conflict')

            resolveResidency?.(residency)
            await vi.waitFor(() => expect(dashboard.snapshot()).toMatchObject({ residency, residencyLoading: false }))
        })

        it('leaves the totals intact when the residency status fails', async () => {
            const dashboard = create(vi.fn().mockResolvedValue(offloaded), vi.fn().mockRejectedValue(new Error('not connected')))

            await dashboard.load()
            await vi.waitFor(() => expect(dashboard.snapshot().residencyLoading).toBe(false))
            expect(dashboard.snapshot()).toMatchObject({ residency: null, loadFailed: false, stats: offloaded })
        })
    })
})
