// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const maintenance = vi.hoisted(() => ({
    getNativePersistentStorageStats: vi.fn(), listNativePersistentSnapshots: vi.fn(), getPeerTempUsage: vi.fn(), cleanupPeerTemp: vi.fn(), previewNativePersistentAssetGc: vi.fn(), executeNativePersistentAssetGc: vi.fn(),
    deleteNativePersistentSnapshot: vi.fn(), removePeerBackup: vi.fn(), listPeerBackups: vi.fn(), createNativePersistentSnapshot: vi.fn(),
    isNativePeerBackupDeleteError: (error: unknown) => {
        if (error && typeof error === 'object' && (error as { code?: unknown }).code === 'peer-backup-in-use') return { code: 'peer-backup-in-use' as const }
        if (error && typeof error === 'object' && (error as { code?: unknown }).code === 'peer-backup-delete-failed') return { code: 'peer-backup-delete-failed' as const }
        return null
    },
}))
const backups = vi.hoisted(() => ({ list: vi.fn(), remove: vi.fn() }))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(), alertError: vi.fn() }))

vi.mock('src/ts/storage/nativePersistentMaintenance', () => maintenance)
vi.mock('src/ts/storage/sync/syncConflictBackup', () => ({ getSyncConflictBackupStore: () => backups }))
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/platform', () => ({ isTauri: true }))
vi.mock('src/lang', async () => ({ language: (await import('src/lang/en')).languageEnglish }))

import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'

const stats = {
    databaseBytes: 1024 * 1024,
    assetObjects: { count: 2, bytes: 2 * 1024 * 1024 }, assetAliases: [{ kind: 'inlay', inlayType: null, count: 1, bytes: 1024 * 1024 }], coldAliases: { count: 0, bytes: 0 }, pluginStorage: { count: 1, bytes: 1024 },
    characters: { active: { count: 2, bytes: 0 }, trashedCount: 1 }, conversations: { count: 3, messageCount: 4 }, assetObjectDeletions: [],
}

describe('RisuNestStorageDashboard', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    function setup(): HTMLElement {
        maintenance.getNativePersistentStorageStats.mockResolvedValue(stats)
        maintenance.listNativePersistentSnapshots.mockResolvedValue([{ path: 'snapshot.db', bytes: 1024, modifiedAt: 1 }])
        maintenance.listPeerBackups.mockResolvedValue([{ path: 'peer.risudat', bytes: 2048, modifiedAt: 2 }])
        backups.list.mockResolvedValue([{ id: 'conflict', createdAt: 3, side: 'local', characterCount: 2, byteLength: 4096, scope: 'database-only' }])
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestStorageDashboard, { target })
        return target
    }

    it('renders six cards, count summary, and backup rows without calculating temp usage', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))

        expect(target.querySelectorAll('.grid.grid-cols-2.sm\\:grid-cols-3 > *')).toHaveLength(6)
        expect(target.textContent).toContain('2 characters · 3 chats · 4 messages')
        expect(target.textContent).toContain('(1 in trash)')
        expect(target.textContent).toContain(new Date(1).toLocaleString())
        expect(target.textContent).toContain(new Date(2).toLocaleString())
        expect(target.textContent).not.toContain('snapshot.db')
        expect(target.textContent).not.toContain('peer.risudat')
        expect(target.querySelector('[role="status"][aria-live="polite"]')).not.toBeNull()
        expect(maintenance.getPeerTempUsage).not.toHaveBeenCalled()
    })

    it('calculates and cleans temporary data, then previews, confirms, and runs GC', async () => {
        const target = setup()
        maintenance.getPeerTempUsage.mockResolvedValue({ count: 2, bytes: 1024 })
        maintenance.cleanupPeerTemp.mockResolvedValue({ count: 0, bytes: 0 })
        maintenance.previewNativePersistentAssetGc.mockResolvedValue({ candidateCount: 2, candidateBytes: 2048, deletedCount: 0, deletedBytes: 0, blockers: [] })
        maintenance.executeNativePersistentAssetGc.mockResolvedValue({ candidateCount: 2, candidateBytes: 2048, deletedCount: 2, deletedBytes: 2048, blockers: [] })
        alerts.alertConfirm.mockResolvedValue(true)
        await vi.waitFor(() => expect(target.textContent).toContain('Calculate size'))
        const button = (text: string) => [...target.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === text)
        expect(button('Clean up sync temp files')).toBeUndefined()

        button('Calculate size')?.click()
        await vi.waitFor(() => expect(target.textContent).toContain('1.0 KiB'))
        await vi.waitFor(() => expect(button('Clean up sync temp files')?.disabled).toBe(false))
        button('Clean up sync temp files')?.click()
        await vi.waitFor(() => expect(maintenance.cleanupPeerTemp).toHaveBeenCalledOnce())
        button('Clean up unused images')?.click()
        await vi.waitFor(() => expect(target.textContent).toContain('Removable: 2 items (2.0 KiB)'))
        button('Clean up unused images')?.click()
        await vi.waitFor(() => expect(maintenance.executeNativePersistentAssetGc).toHaveBeenCalledOnce())
        expect(alerts.alertConfirm).toHaveBeenCalledWith('This will delete 2 unused images (2.0 KiB). Continue?')
    })

    it('uses localized safe errors for a backup that is in use', async () => {
        const target = setup()
        maintenance.removePeerBackup.mockRejectedValue({ code: 'peer-backup-in-use' })
        alerts.alertConfirm.mockResolvedValue(true)
        await vi.waitFor(() => expect(target.textContent).toContain(new Date(2).toLocaleString()))
        const peerDelete = [...target.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.dataset.path === 'peer.risudat')
        peerDelete?.click()
        await vi.waitFor(() => expect(alerts.alertError).toHaveBeenCalledWith("This backup is used by a sync in progress and can't be deleted."))
    })
})
