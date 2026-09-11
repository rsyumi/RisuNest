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
import { languageKorean } from 'src/lang/ko'

const stats = {
    snapshotBytes: 2 * 1024 * 1024,
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

    function setup(statsPromise: Promise<typeof stats> = Promise.resolve(stats)): HTMLElement {
        maintenance.getNativePersistentStorageStats.mockImplementation(() => statsPromise)
        maintenance.listNativePersistentSnapshots.mockResolvedValue([{ id: 'snapshot.db', reason: 'manual', reclaimableBytes: 0, bytes: 1024, modifiedAt: 1 }])
        maintenance.listPeerBackups.mockResolvedValue([{ path: 'peer.risudat', bytes: 2048, modifiedAt: 2 }])
        backups.list.mockResolvedValue([{ id: 'conflict', createdAt: 3, side: 'local', characterCount: 2, byteLength: 4096, scope: 'database-only' }])
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestStorageDashboard, { target })
        return target
    }

    it('renders the total with its five-part breakdown, count summary, and backup rows without calculating temp usage', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))

        expect(target.querySelectorAll('[data-storage-legend] > li')).toHaveLength(5)
        expect(target.textContent).toContain('5.0 MiB')
        expect(target.textContent).toContain('Database')
        expect(target.textContent).toContain('Chat attachments 1.0 MiB (included in images & media) · Plugin data 1.0 KiB')
        expect(target.textContent).toContain('2 characters · 3 chats · 4 messages')
        expect(target.textContent).toContain('(1 in trash)')
        expect(target.textContent).toContain(new Date(1).toLocaleString())
        expect(target.textContent).toContain(new Date(2).toLocaleString())
        expect(target.textContent).not.toContain('snapshot.db')
        expect(target.textContent).not.toContain('peer.risudat')
        expect(target.querySelector('[role="status"][aria-live="polite"]')).not.toBeNull()
        expect(maintenance.getPeerTempUsage).not.toHaveBeenCalled()
    })

    it('renders six gray card-position placeholders during the initial load', async () => {
        let resolveStats: ((value: typeof stats) => void) | undefined
        const target = setup(new Promise((resolve) => { resolveStats = resolve }))

        await vi.waitFor(() => expect(target.querySelectorAll('[data-storage-card-placeholder]')).toHaveLength(6))
        const placeholders = [...target.querySelectorAll<HTMLElement>('[data-storage-card-placeholder]')]
        expect(placeholders.every((placeholder) => placeholder.classList.contains('bg-darkbutton'))).toBe(true)

        resolveStats?.(stats)
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))
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
        expect(button('Clean up')).toBeUndefined()

        button('Calculate size')?.click()
        await vi.waitFor(() => expect(target.textContent).toContain('1.0 KiB used'))
        await vi.waitFor(() => expect(button('Clean up')?.disabled).toBe(false))
        button('Clean up')?.click()
        await vi.waitFor(() => expect(maintenance.cleanupPeerTemp).toHaveBeenCalledOnce())
        expect(button('Calculate size')).toBeDefined()
        button('Find')?.click()
        await vi.waitFor(() => expect(target.textContent).toContain('Removable: 2 items (2.0 KiB)'))
        expect(button('Find')).toBeDefined()
        expect(languageKorean.risuNest.storage.gcRunConfirm).toBe('지금 삭제')
        button('Delete now')?.click()
        await vi.waitFor(() => expect(maintenance.executeNativePersistentAssetGc).toHaveBeenCalledOnce())
        expect(alerts.alertConfirm).toHaveBeenCalledWith('This will delete 2 unused images (2.0 KiB). Continue?')
        expect(target.textContent).toContain('Deleted: 2 items (2.0 KiB)')
        expect(button('Delete now')).toBeUndefined()
    })

    it('keeps each maintenance result next to the button that produced it', async () => {
        const target = setup()
        maintenance.getPeerTempUsage.mockResolvedValue({ count: 2, bytes: 1024 })
        maintenance.previewNativePersistentAssetGc.mockResolvedValue({ candidateCount: 2, candidateBytes: 2048, deletedCount: 0, deletedBytes: 0, blockers: [] })
        await vi.waitFor(() => expect(target.textContent).toContain('Calculate size'))
        const button = (text: string) => [...target.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === text)
        const rowOf = (element: Element | undefined) => element?.closest<HTMLElement>('[data-storage-action]') ?? null

        button('Calculate size')?.click()
        await vi.waitFor(() => expect(target.textContent).toContain('1.0 KiB used'))
        const tempRow = rowOf(button('Calculate size'))
        expect(tempRow?.textContent).toContain('1.0 KiB used')
        expect(tempRow?.contains(button('Clean up') ?? null)).toBe(true)
        expect(tempRow?.textContent).toContain('Removes temporary files left behind by device sync.')

        button('Find')?.click()
        await vi.waitFor(() => expect(target.textContent).toContain('Removable: 2 items (2.0 KiB)'))
        const gcRow = rowOf(button('Find'))
        expect(gcRow?.textContent).toContain('Removable: 2 items (2.0 KiB)')
        expect(gcRow?.contains(button('Delete now') ?? null)).toBe(true)
        expect(gcRow).not.toBe(tempRow)
    })

    it('summarizes each backup list with its count and size and keeps delete beside the row text', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Snapshots'))

        const summaries = [...target.querySelectorAll<HTMLElement>('[data-storage-backup-list] > summary')]
        expect(summaries.map((summary) => summary.textContent?.replace(/\s+/g, ' ').trim())).toEqual([
            'Snapshots 1 items · 2.0 MiB',
            'Conflict backups 1 items · 4.0 KiB',
            'Sync backups 1 items · 2.0 KiB',
        ])
        const row = target.querySelector<HTMLElement>('[data-storage-backup-list] [data-storage-backup-row]')
        expect(row?.className).not.toContain('justify-between')
        expect(row?.textContent).toContain('1.0 KiB')
        expect(target.textContent).toContain('The total counts shared storage once.')
        expect(target.textContent).toContain('2 characters · 3 chats · 4 messages')
    })

    it('formats large counts with locale separators', async () => {
        const target = setup(Promise.resolve({ ...stats, conversations: { count: 1200, messageCount: 15231 } }))
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))

        expect(target.textContent).toContain(`${(1200).toLocaleString()} chats · ${(15231).toLocaleString()} messages`)
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

    it('uses a dedicated localized confirmation before deleting a conflict backup', async () => {
        const target = setup()
        alerts.alertConfirm.mockResolvedValue(false)
        await vi.waitFor(() => expect(target.textContent).toContain('Conflict backups'))

        const conflictRow = target.querySelectorAll<HTMLElement>('[data-storage-backup-list]')[1]
        conflictRow?.querySelector<HTMLButtonElement>('button')?.click()

        await vi.waitFor(() => expect(alerts.alertConfirm).toHaveBeenCalledWith(
            'Delete this conflict backup? This conflict backup cannot be recovered after deletion.',
        ))
        expect(backups.remove).not.toHaveBeenCalled()
        expect(languageKorean.risuNest.storage.deleteConflictBackupConfirm)
            .toBe('이 충돌 백업을 삭제할까요? 삭제한 충돌 백업은 복구할 수 없습니다.')
    })

    it('surfaces a temporary-size failure with safe localized copy', async () => {
        const target = setup()
        maintenance.getPeerTempUsage.mockRejectedValue(new Error('private native detail'))
        await vi.waitFor(() => expect(target.textContent).toContain('Calculate size'))

        ;[...target.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent?.trim() === 'Calculate size')?.click()

        await vi.waitFor(() => expect(alerts.alertError).toHaveBeenCalledWith("Couldn't calculate temporary file size. Refresh the information and try again."))
        expect(alerts.alertError.mock.calls.at(-1)?.[0]).not.toContain("wasn't changed")
    })

    it('disables only the active action and prevents its duplicate submission', async () => {
        const target = setup()
        let resolveTemp: ((value: { count: number; bytes: number }) => void) | undefined
        maintenance.getPeerTempUsage.mockImplementation(() => new Promise((resolve) => { resolveTemp = resolve }))
        await vi.waitFor(() => expect(target.textContent).toContain('Calculate size'))
        const button = (text: string) => [...target.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === text)

        button('Calculate size')?.click()
        await tick()
        expect(button('Loading')?.disabled).toBe(true)
        expect(button('Create now')?.disabled).toBe(false)
        expect(button('Find')?.disabled).toBe(false)
        button('Loading')?.click()
        expect(maintenance.getPeerTempUsage).toHaveBeenCalledOnce()

        resolveTemp?.({ count: 1, bytes: 1024 })
        await vi.waitFor(() => expect(target.textContent).toContain('1.0 KiB used'))
    })

    it('orders all backup lists before the storage action row', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Create now'))

        const actionRow = target.querySelector<HTMLElement>('[data-storage-action-row]')
        const lists = [...target.querySelectorAll<HTMLElement>('[data-storage-backup-list]')]
        expect(actionRow).not.toBeNull()
        expect(lists).toHaveLength(3)
        expect(lists.every((list) => Boolean(list.compareDocumentPosition(actionRow!) & Node.DOCUMENT_POSITION_FOLLOWING))).toBe(true)
    })

    it('keeps stale totals visible and offers retry after a post-snapshot reload fails', async () => {
        const target = setup()
        maintenance.createNativePersistentSnapshot.mockResolvedValue({ path: 'new.db', bytes: 1, modifiedAt: 4 })
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))
        maintenance.getNativePersistentStorageStats.mockRejectedValueOnce(new Error('reload failed'))

        ;[...target.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent?.trim() === 'Create now')?.click()

        await vi.waitFor(() => expect(target.textContent).toContain('Storage totals may be out of date.'))
        expect(target.textContent).toContain('Total data')
        expect(target.textContent).toContain('Retry')
    })
})
