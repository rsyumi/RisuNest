// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const maintenance = vi.hoisted(() => ({
    getNativePersistentStorageStats: vi.fn(),
    listNativePersistentSnapshots: vi.fn(),
    previewNativePersistentAssetGc: vi.fn(),
    executeNativePersistentAssetGc: vi.fn(),
    deleteNativePersistentSnapshot: vi.fn(),
    createNativePersistentSnapshot: vi.fn(),
}))
const server = vi.hoisted(() => ({
    getServerSyncBackupInventory: vi.fn(),
    getServerSyncCacheUsage: vi.fn(),
    cleanupServerSyncCache: vi.fn(),
    deleteServerSyncBackup: vi.fn(),
    restoreServerSyncBackup: vi.fn(),
}))
vi.mock('src/ts/storage/sync/serverSyncProduction', () => server)
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

    function setup(
        statsPromise: Promise<typeof stats> = Promise.resolve(stats),
    ): HTMLElement {
        maintenance.getNativePersistentStorageStats.mockImplementation(
            () => statsPromise,
        )
        maintenance.listNativePersistentSnapshots.mockResolvedValue([
            {
                id: 'snapshot.db',
                reason: 'manual',
                reclaimableBytes: 0,
                bytes: 1024,
                modifiedAt: 1,
            },
        ])
        server.getServerSyncBackupInventory.mockResolvedValue({
            items: [],
            next: null,
            completeCount: 105,
            completeBytes: 2048,
            incompleteCount: 1,
            incompleteBytes: 1024,
            diskBytes: 4096,
        })
        server.getServerSyncCacheUsage.mockResolvedValue({
            totalBytes: 1024,
            protectedBytes: 512,
            reclaimableBytes: 512,
            blockedReason: null,
        })
        backups.list.mockResolvedValue([
            {
                id: 'conflict',
                createdAt: 3,
                side: 'local',
                characterCount: 2,
                byteLength: 4096,
                scope: 'database-only',
            },
        ])
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestStorageDashboard, { target })
        return target
    }

    it('renders the total with its six-part breakdown, count summary, and backup rows with complete server storage totals', async () => {
        const target = setup()
        await vi.waitFor(() =>
            expect(target.textContent).toContain('Total data'),
        )

        expect(
            target.querySelectorAll('[data-storage-legend] > li'),
        ).toHaveLength(6)
        expect(target.textContent).toContain('5.0 MiB')
        expect(target.textContent).toContain('Database')
        expect(target.textContent).toContain(
            'Chat attachments 1.0 MiB (included in images & media) · Plugin data 1.0 KiB',
        )
        expect(target.textContent).toContain(
            '2 characters · 3 chats · 4 messages',
        )
        expect(target.textContent).toContain('(1 in trash)')
        expect(target.textContent).toContain(new Date(1).toLocaleString())
        expect(target.textContent).not.toContain('snapshot.db')
        expect(
            target.querySelector('[role="status"][aria-live="polite"]'),
        ).not.toBeNull()
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

    it('summarizes each backup list with its count and size and keeps delete beside the row text', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Snapshots'))

        const summaries = [...target.querySelectorAll<HTMLElement>('[data-storage-backup-list] > summary')]
        expect(summaries.map((summary) => summary.textContent?.replace(/\s+/g, ' ').trim())).toEqual([
            'Snapshots 1 items · 2.0 MiB',
            'Conflict backups 1 items · 4.0 KiB',
        ])
        const row = target.querySelector<HTMLElement>('[data-storage-backup-list] [data-storage-backup-row]')
        expect(row?.className).not.toContain('justify-between')
        expect(row?.textContent).toContain('1.0 KiB')
        expect(target.textContent).toContain('The total counts shared storage once.')
        expect(target.textContent).toContain('2 characters · 3 chats · 4 messages')
    })

    it('previews and confirms unused-image GC independently of server cache', async () => {
        const target = setup()
        maintenance.previewNativePersistentAssetGc.mockResolvedValue({
            candidateCount: 2,
            candidateBytes: 2048,
            deletedCount: 0,
            deletedBytes: 0,
            blockers: [],
        })
        maintenance.executeNativePersistentAssetGc.mockResolvedValue({
            candidateCount: 2,
            candidateBytes: 2048,
            deletedCount: 2,
            deletedBytes: 2048,
            blockers: [],
        })
        alerts.alertConfirm.mockResolvedValue(true)
        const button = (text: string) =>
            [...target.querySelectorAll<HTMLButtonElement>('button')].find(
                (candidate) => candidate.textContent?.trim() === text,
            )
        await vi.waitFor(() => expect(button('Find')).toBeDefined())
        button('Find')!.click()
        await vi.waitFor(() =>
            expect(target.textContent).toContain(
                'Removable: 2 items (2.0 KiB)',
            ),
        )
        button('Delete now')!.click()
        await vi.waitFor(() =>
            expect(
                maintenance.executeNativePersistentAssetGc,
            ).toHaveBeenCalledOnce(),
        )
        expect(alerts.alertConfirm).toHaveBeenCalledWith(
            'This will delete 2 unused images (2.0 KiB). Continue?',
        )
        expect(server.cleanupServerSyncCache).not.toHaveBeenCalled()
    })
    it('formats large counts with locale separators', async () => {
        const target = setup(Promise.resolve({ ...stats, conversations: { count: 1200, messageCount: 15231 } }))
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))

        expect(target.textContent).toContain(`${(1200).toLocaleString()} chats · ${(15231).toLocaleString()} messages`)
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

    it('orders all backup lists before the storage action row', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Create now'))

        const actionRow = target.querySelector<HTMLElement>('[data-storage-action-row]')
        const lists = [...target.querySelectorAll<HTMLElement>('[data-storage-backup-list]')]
        expect(actionRow).not.toBeNull()
        expect(lists).toHaveLength(2)
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
