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
    restoreNativePersistentSnapshot: vi.fn(),
    restartNativeApp: vi.fn(),
}))
const server = vi.hoisted(() => ({
    getServerSyncBackupInventory: vi.fn(),
    getServerSyncCacheUsage: vi.fn(),
    cleanupServerSyncCache: vi.fn(),
    deleteServerSyncBackup: vi.fn(),
    exportServerSyncBackup: vi.fn(),
    restoreServerSyncBackup: vi.fn(),
}))
vi.mock('src/ts/storage/sync/serverSyncProduction', () => server)
const backups = vi.hoisted(() => ({ list: vi.fn(), remove: vi.fn() }))
const alerts = vi.hoisted(() => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
}))

vi.mock('src/ts/storage/nativePersistentMaintenance', () => maintenance)
vi.mock('src/ts/storage/sync/syncConflictBackup', () => ({ getSyncConflictBackupStore: () => backups }))
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/platform', () => ({ isTauri: true }))
vi.mock('src/lang', async () => ({ language: (await import('src/lang/en')).languageEnglish }))

import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'
import type { ManagedServerSyncBackup } from 'src/ts/storage/sync/serverSyncProduction'

const syncText = languageEnglish.risuNest.serverSync
const serverBackup: ManagedServerSyncBackup = {
    id: 'synthetic-id',
    createdAt: 5,
    localRevision: 2,
    head: {
        libraryId: 'library',
        epoch: 'epoch',
        seq: '1',
        headId: 'head',
        minRetainedSeq: '0',
        sections: {
            hypa: { stateId: 'hypa-state', changedSeq: '0', gcFloor: '0' },
            library: { stateId: 'library-state', changedSeq: '0', gcFloor: '0' },
            'local-plugins': { stateId: 'plugins-state', changedSeq: '0', gcFloor: '0' },
        },
    },
    local: {
        localRequiredBytes: 1024,
        remoteDependentBytes: 0,
        availability: 'local-complete',
    },
    remote: {
        localRequiredBytes: 256,
        remoteDependentBytes: 2048,
        availability: 'connection-required',
    },
    preservationScope: 'library',
    diskBytes: 3200,
    deletable: true,
    blockedReason: null,
}

const stats = {
    snapshotBytes: 2 * 1024 * 1024,
    databaseBytes: 1024 * 1024,
    assetObjects: { count: 2, bytes: 2 * 1024 * 1024 }, assetAliases: [{ kind: 'inlay', inlayType: null, count: 1, bytes: 1024 * 1024 }], pluginStorage: { count: 1, bytes: 1024 },
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

    const button = (target: HTMLElement, text: string) =>
        [...target.querySelectorAll<HTMLButtonElement>('button')].find(
            (candidate) => candidate.textContent?.trim().startsWith(text),
        )
    const exact = (target: HTMLElement, text: string) =>
        [...target.querySelectorAll<HTMLButtonElement>('button')].find(
            (candidate) => candidate.textContent?.trim() === text,
        )

    function setup(
        statsPromise: Promise<typeof stats> = Promise.resolve(stats),
        serverBackups: ManagedServerSyncBackup[] = [],
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
            items: serverBackups,
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
            'Sync backups 106 items · 4.0 KiB',
            'Temporary files 1.0 KiB',
        ])
        const row = target.querySelector<HTMLElement>('[data-storage-backup-list] [data-storage-backup-row]')
        expect(row?.className).not.toContain('justify-between')
        expect(row?.textContent).toContain('1.0 KiB')
        expect(row?.textContent).toContain('Created manually')
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
    it('lists what the cleanup found and why each file stayed', async () => {
        const target = setup()
        maintenance.previewNativePersistentAssetGc.mockResolvedValue({
            candidateCount: 1,
            candidateBytes: 1024,
            deletedCount: 0,
            deletedBytes: 0,
            blockers: [],
            candidates: [
                { objectHash: 'a'.repeat(64), bytes: 1024, createdAtMs: 0, state: 'deletable', holders: [] },
                { objectHash: 'b'.repeat(64), bytes: 2048, createdAtMs: 0, state: 'held', holders: ['repair'] },
                { objectHash: 'c'.repeat(64), bytes: 4096, createdAtMs: 0, state: 'held', holders: [] },
                { objectHash: 'd'.repeat(64), bytes: 512, createdAtMs: 0, state: 'recent', holders: [] },
            ],
            omitted: 7,
        })
        const find = () =>
            [...target.querySelectorAll<HTMLButtonElement>('button')].find(
                (candidate) => candidate.textContent?.trim() === 'Find',
            )
        await vi.waitFor(() => expect(find()).toBeDefined())
        find()!.click()
        await vi.waitFor(() =>
            expect(target.querySelectorAll('[data-storage-gc-row]')).toHaveLength(4),
        )
        const rows = [...target.querySelectorAll('[data-storage-gc-row]')].map(
            (row) => row.textContent ?? '',
        )
        expect(rows[0]).toContain('Can be deleted')
        expect(rows[1]).toContain('kept so a fix can be undone')
        expect(rows[2]).toContain('in use')
        expect(rows[3]).toContain('Too new to delete yet')
        expect(target.querySelector('[data-storage-gc-list]')?.textContent).toContain(
            'and 7 more',
        )
    })

    it('says that clearing a link does not delete the file', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Unused images'))
        expect(target.textContent).toContain(
            'This is the only place a file is actually deleted.',
        )
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
        expect(lists).toHaveLength(4)
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

    it('restores a snapshot from its row after confirmation and restarts through the guarded flow', async () => {
        const target = setup()
        alerts.alertConfirm.mockResolvedValue(true)
        maintenance.restoreNativePersistentSnapshot.mockImplementation(async (actions) => {
            const chosen = await actions.choose([{ id: 'snapshot.db', reason: 'manual', reclaimableBytes: 0, bytes: 1024, modifiedAt: 1 }])
            if (chosen === null || !(await actions.confirm())) return false
            await actions.restart()
            return true
        })
        await vi.waitFor(() => expect(exact(target, 'Restore')).toBeDefined())

        exact(target, 'Restore')!.click()

        await vi.waitFor(() => expect(maintenance.restartNativeApp).toHaveBeenCalledOnce())
        expect(maintenance.restoreNativePersistentSnapshot).toHaveBeenCalledOnce()
        expect(alerts.alertConfirm).toHaveBeenCalledWith(languageEnglish.restoreLocalSnapshotConfirm)
        expect(languageKorean.restoreLocalSnapshotConfirm).toBe('현재 데이터를 이 로컬 스냅샷으로 교체하고 앱을 다시 시작할까요?')
    })

    it('lists sync backups with restore and delete, and clears temporary files, through the dashboard actions', async () => {
        const target = setup(Promise.resolve(stats), [serverBackup])
        alerts.alertConfirm.mockResolvedValue(true)
        server.restoreServerSyncBackup.mockResolvedValue(undefined)
        server.deleteServerSyncBackup.mockResolvedValue({ localDeleted: true, cleanup: 'complete' })
        server.cleanupServerSyncCache.mockResolvedValue({ totalBytes: 512, protectedBytes: 512, reclaimableBytes: 0, blockedReason: null })
        await vi.waitFor(() => expect(button(target, syncText.restoreRemoteBackup)).toBeDefined())

        button(target, syncText.restoreRemoteBackup)!.click()
        await vi.waitFor(() => expect(server.restoreServerSyncBackup).toHaveBeenCalledExactlyOnceWith('synthetic-id', 'remote'))
        expect(alerts.alertConfirm).toHaveBeenCalledWith(syncText.management.restoreConfirm)

        button(target, syncText.exportLocalBackup)!.click()
        await vi.waitFor(() => expect(server.exportServerSyncBackup).toHaveBeenCalledExactlyOnceWith('synthetic-id', 'local'))

        const syncList = target.querySelector<HTMLElement>('[data-storage-backup-list="sync-backups"]')!
        expect(syncList.textContent).toContain(new Date(5).toLocaleString())
        const remove = [...syncList.querySelectorAll<HTMLButtonElement>('button')].find((candidate) => candidate.textContent?.trim() === 'Remove')!
        await vi.waitFor(() => expect(remove.disabled).toBe(false))
        remove.click()
        await vi.waitFor(() => expect(server.deleteServerSyncBackup).toHaveBeenCalledExactlyOnceWith('synthetic-id'))
        expect(alerts.alertConfirm).toHaveBeenCalledWith(syncText.management.deleteConfirm)

        const clean = button(target, syncText.management.clean)!
        await vi.waitFor(() => expect(clean.disabled).toBe(false))
        clean.click()
        await vi.waitFor(() => expect(server.cleanupServerSyncCache).toHaveBeenCalledOnce())
        expect(alerts.alertConfirm).toHaveBeenCalledWith(syncText.management.cleanConfirm)
        expect(target.querySelector('[data-storage-backup-list="temp-files"]')?.textContent).toContain(syncText.management.reclaimable)
    })

    it('marks a running action busy on its button instead of swapping the label', async () => {
        const target = setup()
        let finish: ((value: { id: string; revision: number; bytes: number; durationMs: number }) => void) | undefined
        maintenance.createNativePersistentSnapshot.mockImplementation(() => new Promise((resolve) => { finish = resolve }))
        await vi.waitFor(() => expect(button(target, 'Create now')).toBeDefined())
        const create = button(target, 'Create now')!

        create.click()

        await vi.waitFor(() => expect(create.getAttribute('aria-busy')).toBe('true'))
        expect(create.disabled).toBe(true)
        expect(create.textContent).toContain('Create now')
        expect(create.textContent).not.toContain('Loading...')
        finish?.({ id: 'new.db', revision: 1, bytes: 1, durationMs: 1 })
        await vi.waitFor(() => expect(create.getAttribute('aria-busy')).toBeNull())
    })
})
