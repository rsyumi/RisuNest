import { describe, expect, it, vi } from 'vitest'

import {
    createRisuNestStorageDashboard,
    formatRisuNestStorageBytes,
    storageDashboardRollup,
} from './risuNestStorageDashboard'

const stats = {
    databaseBytes: 2 * 1024 * 1024,
    assetObjects: { count: 4, bytes: 3 * 1024 * 1024 },
    assetAliases: [
        { kind: 'inlay', inlayType: null, count: 1, bytes: 1024 * 1024 },
        { kind: 'image', inlayType: 'image', count: 1, bytes: 2 * 1024 * 1024 },
    ],
    coldAliases: { count: 0, bytes: 0 },
    pluginStorage: { count: 2, bytes: 512 * 1024 },
    characters: { active: { count: 3, bytes: 0 }, trashedCount: 1 },
    conversations: { count: 4, messageCount: 5 },
    assetObjectDeletions: [],
}

const snapshots = [{ path: 'snapshot.db', bytes: 2 * 1024 * 1024, modifiedAt: 1 }]
const conflictBackups = [{ id: 'conflict', createdAt: 2, side: 'local' as const, characterCount: 1, byteLength: 1024 * 1024, scope: 'database-only' as const }]
const peerBackups = [{ path: 'peer.risudat', bytes: 3 * 1024 * 1024, modifiedAt: 3 }]

describe('RisuNest storage dashboard view model', () => {
    it('rolls up six overlapping logical cards and formats MiB and GiB', () => {
        const rollup = storageDashboardRollup(stats, snapshots, conflictBackups, peerBackups)

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

    it('loads rows, retries failures, and does not traverse temp storage until requested', async () => {
        const getStats = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(stats)
        const getTemp = vi.fn().mockResolvedValue({ count: 2, bytes: 1024 })
        const dashboard = createRisuNestStorageDashboard({
            getStats,
            listSnapshots: vi.fn().mockResolvedValue(snapshots),
            listConflictBackups: vi.fn().mockResolvedValue(conflictBackups),
            listPeerBackups: vi.fn().mockResolvedValue(peerBackups),
            getTemp,
            cleanupTemp: vi.fn(),
            previewGc: vi.fn(),
            executeGc: vi.fn(),
            deleteSnapshot: vi.fn(),
            deleteConflictBackup: vi.fn(),
            deletePeerBackup: vi.fn(),
            createSnapshot: vi.fn(),
        })

        await dashboard.load()
        expect(dashboard.snapshot()).toMatchObject({ loading: false, loadFailed: true })
        expect(getTemp).not.toHaveBeenCalled()

        await dashboard.load()
        expect(dashboard.snapshot()).toMatchObject({ loadFailed: false, snapshots, conflictBackups, peerBackups })

        await dashboard.calculateTempSize()
        expect(dashboard.snapshot().tempUsage).toEqual({ count: 2, bytes: 1024 })
    })

    it('cleans temp storage and previews then executes garbage collection one operation at a time', async () => {
        let resolveCleanup: (() => void) | undefined
        const cleanupTemp = vi.fn(() => new Promise<{ count: number; bytes: number }>((resolve) => { resolveCleanup = () => resolve({ count: 0, bytes: 0 }) }))
        const previewGc = vi.fn().mockResolvedValue({ candidateCount: 2, candidateBytes: 1024, deletedCount: 0, deletedBytes: 0, blockers: [] })
        const executeGc = vi.fn().mockResolvedValue({ candidateCount: 2, candidateBytes: 1024, deletedCount: 2, deletedBytes: 1024, blockers: [] })
        const dashboard = createRisuNestStorageDashboard({
            getStats: vi.fn().mockResolvedValue(stats), listSnapshots: vi.fn().mockResolvedValue([]), listConflictBackups: vi.fn().mockResolvedValue([]), listPeerBackups: vi.fn().mockResolvedValue([]),
            getTemp: vi.fn().mockResolvedValue({ count: 1, bytes: 1024 }), cleanupTemp, previewGc, executeGc,
            deleteSnapshot: vi.fn(), deleteConflictBackup: vi.fn(), deletePeerBackup: vi.fn(), createSnapshot: vi.fn(),
        })
        await dashboard.load()
        await dashboard.calculateTempSize()

        const cleanup = dashboard.cleanupTemp()
        expect(dashboard.snapshot().busy).toBe('cleanup-temp')
        await dashboard.previewGc()
        expect(previewGc).not.toHaveBeenCalled()
        resolveCleanup?.()
        await cleanup
        expect(dashboard.snapshot().tempUsage).toEqual({ count: 0, bytes: 0 })

        await dashboard.previewGc()
        expect(dashboard.snapshot().gcPreview).toMatchObject({ candidateCount: 2, candidateBytes: 1024 })
        await dashboard.executeGc()
        expect(executeGc).toHaveBeenCalledOnce()
    })
})
