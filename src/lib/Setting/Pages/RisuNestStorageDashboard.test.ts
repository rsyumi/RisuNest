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
    restartNativeApp: vi.fn(async () => {}),
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
const residency = vi.hoisted(() => ({ getAssetResidencyStatus: vi.fn() }))
vi.mock('src/ts/storage/sync/serverAssetResidency', () => residency)
const backups = vi.hoisted(() => ({ list: vi.fn(), remove: vi.fn() }))
const alerts = vi.hoisted(() => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
}))

vi.mock('src/ts/storage/nativePersistentMaintenance', () => maintenance)
const runtime = vi.hoisted(() => ({ flushPendingData: vi.fn(async (_reason: string) => {}) }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => {
    const { createAssetAliasCleanupTarget } = await import('src/ts/storage/deletedOwnerAssetCleanup.testSupport')
    return { ...runtime, assetAliasCleanupTarget: () => createAssetAliasCleanupTarget(runtime.flushPendingData) }
})
const core = vi.hoisted(() => ({ invoke: vi.fn(async (_command: string, _args?: unknown): Promise<unknown> => ({ revision: 2 })) }))
vi.mock('@tauri-apps/api/core', async (importOriginal) => ({ ...await importOriginal<typeof import('@tauri-apps/api/core')>(), invoke: core.invoke }))
vi.mock('src/ts/storage/sync/syncConflictBackup', () => ({ getSyncConflictBackupStore: () => backups }))
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))

import RisuNestStorageDashboard from './RisuNestStorageDashboard.svelte'
import { assetResidencyStatus } from 'src/ts/storage/sync/serverAssetResidency.testSupport'
import { languageEnglish } from 'src/lang/en'
import { getDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'

const syncText = languageEnglish.risuNest.serverSync
const stats = {
    snapshotBytes: 2 * 1024 * 1024,
    databaseBytes: 1024 * 1024,
    databaseReclaimableBytes: 0,
    assetObjects: { count: 2, bytes: 2 * 1024 * 1024 }, assetBodies: { count: 2, bytes: 2 * 1024 * 1024 },
    missingAssetBodies: { count: 0, bytes: 0 }, inlayBodies: { count: 1, bytes: 1024 * 1024 }, assetAliases: [{ kind: 'inlay', inlayType: null, count: 1, bytes: 1024 * 1024 }], pluginStorage: { count: 1, bytes: 1024 },
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

    it.each(['temp'] as const)('keeps snapshot restore usable when the first %s inventory fails', async source => {
        server.getServerSyncCacheUsage.mockRejectedValueOnce(new Error('cache'))
        const target = setup()
        await vi.waitFor(() => expect(target.querySelector('[data-storage-backup-list="snapshots"]')).not.toBeNull())
        expect(target.querySelector('[data-storage-summary]')).toBeNull()
        expect(exact(target, languageEnglish.risuNest.storage.restoreSnapshot)?.disabled).toBe(false)
        expect(target.textContent).toContain(languageEnglish.risuNest.storage.loadFailed)
    })

    it('localizes snapshot reasons without exposing unknown tokens', async () => {
        maintenance.listNativePersistentSnapshots.mockResolvedValueOnce(['manual', 'periodic', 'data-health-repair', 'synthetic-unknown'].map((reason, index) => ({ id: `snapshot-${index}`, reason, bytes: 1, modifiedAt: 1 })))
        const target = setup()
        await vi.waitFor(() => expect(target.querySelectorAll('[data-storage-backup-list="snapshots"] [data-storage-backup-row]')).toHaveLength(4))
        expect(target.textContent).toContain(languageEnglish.risuNest.storage.snapshotReasons.dataHealthRepair)
        expect(target.textContent).not.toContain('synthetic-unknown')
        expect(target.textContent).not.toContain('data-health-repair')
    })

    it('stores the automatic snapshot switch and a keep count from one to eight', async () => {
        const strings = languageEnglish.risuNest.storage
        const target = setup()
        const toggle = target.querySelector<HTMLInputElement>('[data-storage-setting="automatic-snapshots"] input[type="checkbox"]')!
        const count = target.querySelector<HTMLInputElement>('[data-storage-setting="snapshot-keep-count"] input')!
        expect(target.textContent).toContain(strings.snapshotKeepCountHelp.replace('{0}', '8'))
        try {
            expect(toggle.checked).toBe(true)
            toggle.click()
            await tick()
            expect(getDeviceSettings().automaticSnapshotsEnabled).toBe(false)
            for (const [typed, stored] of [['12', 8], ['0', 1], ['3', 3]] as const) {
                count.value = typed
                count.dispatchEvent(new Event('input', { bubbles: true }))
                count.dispatchEvent(new Event('change', { bubbles: true }))
                await tick()
                expect(getDeviceSettings().snapshotKeepCount).toBe(stored)
                expect(count.value).toBe(String(stored))
            }
        } finally {
            updateDeviceSettings({ automaticSnapshotsEnabled: true, snapshotKeepCount: 8 })
        }
    })

    it('describes a blocked temporary file cleanup without exposing the reason token', async () => {
        server.getServerSyncCacheUsage.mockResolvedValueOnce({ totalBytes: 1024, cacheBytes: 1024, protectedBytes: 0, reclaimableBytes: 1024, ledgerBytes: 0, databaseBytes: 0, blockedReason: 'server-sync-busy' })
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain(syncText.management.blockedReasons['server-sync-busy']))
        expect(target.textContent).not.toContain('server-sync-busy')
        expect(exact(target, syncText.management.clean)?.disabled).toBe(true)
    })

    function setup(
        statsPromise: Promise<typeof stats> = Promise.resolve(stats),
        snapshotsPromise: Promise<unknown[]> = Promise.resolve([
            {
                id: 'snapshot.db',
                reason: 'manual',
                bytes: 1024,
                modifiedAt: 1,
            },
        ]),
    ): HTMLElement {
        maintenance.getNativePersistentStorageStats.mockImplementation(
            () => statsPromise,
        )
        maintenance.listNativePersistentSnapshots.mockImplementation(
            () => snapshotsPromise,
        )
        server.getServerSyncCacheUsage.mockResolvedValue({
            totalBytes: 1024 + 2048,
            cacheBytes: 1024,
            protectedBytes: 512,
            reclaimableBytes: 512,
            ledgerBytes: 2048,
            databaseBytes: 0,
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

    it('renders the total with its seven-part breakdown, count summary, and backup rows with complete server storage totals', async () => {
        const target = setup()
        await vi.waitFor(() =>
            expect(target.textContent).toContain('Total data'),
        )

        const legend = [...target.querySelectorAll<HTMLElement>('[data-storage-legend] > li')]
        expect(legend).toHaveLength(6)
        const text = (label: string) =>
            legend.find((item) => item.textContent?.includes(label))?.textContent?.replace(/\s+/g, ' ').trim()
        expect(text(syncText.management.cache)).toBe('Temporary files 1.0 KiB')
        expect(text(syncText.management.ledger)).toBe('Asset storage records 2.0 KiB')
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

    describe('images and media held elsewhere', () => {
        const offloaded = {
            ...stats,
            assetObjects: { count: 4, bytes: 1024 * 1024 * 1024 + 1024 * 1024 },
            assetBodies: { count: 1, bytes: 1024 * 1024 },
            missingAssetBodies: { count: 3, bytes: 1024 * 1024 * 1024 },
        }
        const status = assetResidencyStatus({
            localBytes: 1024 * 1024, remoteBytes: 1024 * 1024 * 1024, remoteObjects: 3, serverBytes: 1024 * 1024 * 1024, serverObjects: 3,
        })
        const mediaItem = (target: HTMLElement) =>
            [...target.querySelectorAll<HTMLElement>('[data-storage-legend] > li')]
                .find((item) => item.textContent?.includes(languageEnglish.risuNest.storage.media))!
        const flat = (element: Element | null | undefined) => element?.textContent?.replace(/\s+/g, ' ').trim()

        it('counts the files on this device and notes the size held only on the server', async () => {
            residency.getAssetResidencyStatus.mockResolvedValue(status)
            const target = setup(Promise.resolve(offloaded))
            await vi.waitFor(() => expect(target.querySelector('[data-storage-off-device]')).not.toBeNull())

            const item = mediaItem(target)
            expect(flat(item.firstElementChild)).toBe('Images & media 1.0 MiB')
            expect(flat(item.querySelector('[data-storage-off-device]'))).toBe('Files only on the server 1.0 GiB')
            // Total: database 1 MiB + media 1 MiB + snapshots 2 MiB + cache and ledger 3 KiB + conflict backup 4 KiB.
            const total = flat(target.querySelector('[data-storage-summary] .font-bold'))
            expect(total).toBe('4.0 MiB')
            expect(residency.getAssetResidencyStatus).toHaveBeenCalledOnce()
        })

        it('holds the note line while the residency status loads, without delaying the totals', async () => {
            let resolveStatus: ((value: typeof status) => void) | undefined
            residency.getAssetResidencyStatus.mockImplementation(() => new Promise((resolve) => { resolveStatus = resolve }))
            const target = setup(Promise.resolve(offloaded))
            await vi.waitFor(() => expect(target.querySelector('[data-storage-off-device-placeholder]')).not.toBeNull())

            const item = mediaItem(target)
            expect(item.querySelector('[data-storage-off-device-placeholder]')?.className).toContain('h-4')
            expect(item.className).toContain('col-span-full')
            expect(target.textContent).toContain('Total data')
            resolveStatus?.(status)
            await vi.waitFor(() => expect(item.querySelector('[data-storage-off-device]')?.className).toContain('text-xs'))
            expect(target.querySelector('[data-storage-off-device-placeholder]')).toBeNull()
            expect(item.className).toContain('col-span-full')
        })

        it('names external storage and files not found apart from the server', async () => {
            residency.getAssetResidencyStatus.mockResolvedValue({
                ...status, remoteBytes: 1000, remoteObjects: 4, serverBytes: 900, serverObjects: 3, unavailableObjects: 2,
                externalObjects: [{ connectionId: 'synthetic', objects: 1 }],
            })
            const target = setup(Promise.resolve(offloaded))
            await vi.waitFor(() => expect(target.querySelector('[data-storage-off-device]')).not.toBeNull())

            expect(flat(target.querySelector('[data-storage-off-device]'))).toBe(
                'Files only on the server 900 B · Files only in external storage 100 B · Files not found 2',
            )
        })

        it('adds no note when every file is on this device', async () => {
            const target = setup()
            await vi.waitFor(() => expect(target.textContent).toContain('Total data'))

            expect(target.querySelector('[data-storage-off-device-placeholder]')).toBeNull()
            expect(target.querySelector('[data-storage-off-device]')).toBeNull()
            expect(mediaItem(target).className).not.toContain('col-span-full')
            expect(residency.getAssetResidencyStatus).not.toHaveBeenCalled()
        })
    })

    it('places the loading placeholder where the total, bar and legend appear', async () => {
        let resolveStats: ((value: typeof stats) => void) | undefined
        const target = setup(new Promise((resolve) => { resolveStats = resolve }))

        await vi.waitFor(() => expect(target.querySelector('[data-storage-summary-placeholder]')).not.toBeNull())
        const placeholder = target.querySelector<HTMLElement>('[data-storage-summary-placeholder]')!
        const blocks = [...placeholder.querySelectorAll<HTMLElement>('.animate-pulse')]
        expect(blocks.length).toBeGreaterThan(0)
        expect(blocks.every((block) => block.classList.contains('bg-darkbutton'))).toBe(true)
        expect(placeholder.innerHTML).not.toContain('sm:')

        resolveStats?.(stats)
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))
        expect(target.querySelector('[data-storage-summary-placeholder]')).toBeNull()
    })

    it('shows the totals while the snapshot list is still loading', async () => {
        let resolveSnapshots: ((value: unknown[]) => void) | undefined
        const target = setup(Promise.resolve(stats), new Promise((resolve) => { resolveSnapshots = resolve }))

        await vi.waitFor(() => expect(target.querySelector('[data-storage-summary]')).not.toBeNull())
        expect(target.querySelector('[data-storage-backup-list="snapshots"]')).toBeNull()

        resolveSnapshots?.([{ id: 'snapshot.db', reason: 'manual', bytes: 1024, modifiedAt: 1 }])
        await vi.waitFor(() => expect(target.querySelectorAll('[data-storage-backup-list="snapshots"] [data-storage-backup-row]')).toHaveLength(1))
    })

    it('summarizes each backup list with its count and size and keeps delete beside the row text', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.querySelectorAll('[data-storage-backup-list] > summary')).toHaveLength(3))

        const summaries = [...target.querySelectorAll<HTMLElement>('[data-storage-backup-list] > summary')]
        expect(summaries.map((summary) => summary.textContent?.replace(/\s+/g, ' ').trim())).toEqual([
            'Snapshots 1 items · 2.0 MiB',
            'Conflict backups 1 items · 4.0 KiB',
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
        // Pending edits are committed before each scan, so a file they use is not offered.
        const [beforePreview, beforeExecute] = runtime.flushPendingData.mock.invocationCallOrder
        expect(beforePreview).toBeLessThan(maintenance.previewNativePersistentAssetGc.mock.invocationCallOrder[0])
        expect(beforeExecute).toBeGreaterThan(maintenance.previewNativePersistentAssetGc.mock.invocationCallOrder[0])
        expect(beforeExecute).toBeLessThan(maintenance.executeNativePersistentAssetGc.mock.invocationCallOrder[0])
        // Unused aliases are deleted through the save queue before the files they held are swept.
        expect(core.invoke).toHaveBeenCalledExactlyOnceWith('pds_asset_delete_orphaned_aliases', { expectedRevision: 1, owner: null })
        expect(beforeExecute).toBeLessThan(core.invoke.mock.invocationCallOrder[0])
        expect(core.invoke.mock.invocationCallOrder[0]).toBeLessThan(maintenance.executeNativePersistentAssetGc.mock.invocationCallOrder[0])
        expect(alerts.alertConfirm).toHaveBeenCalledWith(
            'This will delete 2 unused files (2.0 KiB). Continue?',
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
        // The files to delete are listed first; the kept ones sit behind their own fold.
        const deletable = [...target.querySelectorAll('[data-storage-gc-list] [data-storage-gc-row]')].map(
            (row) => row.textContent ?? '',
        )
        expect(deletable).toHaveLength(1)
        expect(deletable[0]).toContain('Can be deleted')
        expect(target.querySelector('[data-storage-gc-list]')?.textContent).toContain(
            'and 7 more',
        )
        const kept = target.querySelector('[data-storage-gc-kept]')
        expect(kept?.textContent).toContain('3 files kept')
        expect(kept?.hasAttribute('open')).toBe(false)
        const keptRows = [...kept!.querySelectorAll('[data-storage-gc-row]')].map(
            (row) => row.textContent ?? '',
        )
        expect(keptRows[0]).toContain('kept so a fix can be undone')
        expect(keptRows[1]).toContain('in use')
        expect(keptRows[2]).toContain('Too new to delete yet')
    })

    it('shows the cleanup blocker instead of offering deletion', async () => {
        const target = setup()
        maintenance.previewNativePersistentAssetGc.mockResolvedValue({ candidateCount: 0, candidateBytes: 0, deletedCount: 0, deletedBytes: 0, blockers: ['plugin-storage-opaque'] })
        await vi.waitFor(() => expect([...target.querySelectorAll('button')].some(button => button.textContent?.trim() === 'Find')).toBe(true))
        ;[...target.querySelectorAll('button')].find(button => button.textContent?.trim() === 'Find')!.click()
        await vi.waitFor(() => expect(target.textContent).toContain('Plugin data may reference these files.'))
        expect([...target.querySelectorAll('button')].some(button => button.textContent?.trim() === 'Delete now')).toBe(false)
        expect(maintenance.executeNativePersistentAssetGc).not.toHaveBeenCalled()
    })

    it('shows the search as a progress panel while the cleanup preview runs', async () => {
        const target = setup()
        let release: (value: unknown) => void = () => {}
        maintenance.previewNativePersistentAssetGc.mockImplementation(
            () => new Promise((resolve) => { release = resolve }),
        )
        const find = () =>
            [...target.querySelectorAll<HTMLButtonElement>('button')].find(
                (candidate) => candidate.textContent?.trim() === 'Find',
            )
        await vi.waitFor(() => expect(find()).toBeDefined())
        find()!.click()
        await vi.waitFor(() =>
            expect(target.querySelector('[data-storage-gc-progress]')?.textContent).toContain(
                'Looking for unused files',
            ),
        )
        release({ candidateCount: 0, candidateBytes: 0, deletedCount: 0, deletedBytes: 0, blockers: [], candidates: [] })
        await vi.waitFor(() =>
            expect(target.querySelector('[data-storage-gc-progress]')).toBeNull(),
        )
    })

    it('describes the unused files row with its own action only', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.querySelector('[data-storage-action="gc"]')).not.toBeNull())

        const row = target.querySelector<HTMLElement>('[data-storage-action="gc"]')!
        expect(row.querySelector('p')?.textContent).toBe('Finds and deletes files that nothing refers to.')
    })

    it('offers database compaction only when the files can shrink, and restarts after recording it', async () => {
        const strings = languageEnglish.risuNest.storage
        const reclaimable = 3 * 1024 * 1024
        const order: string[] = []
        runtime.flushPendingData.mockImplementationOnce(async () => { order.push('flush') })
        core.invoke.mockImplementation(async (command) => {
            order.push(command)
            return command === 'pds_store_compaction_request' ? { kind: 'requested' } : { revision: 2 }
        })
        maintenance.restartNativeApp.mockImplementationOnce(async () => { order.push('restart') })
        alerts.alertConfirm.mockResolvedValue(true)
        const target = setup(Promise.resolve({ ...stats, databaseReclaimableBytes: reclaimable }))
        await vi.waitFor(() => expect(target.querySelector('[data-storage-action="compact"]')).not.toBeNull())

        const row = target.querySelector<HTMLElement>('[data-storage-action="compact"]')!
        expect(row.textContent).toContain(strings.compactReclaimable.replace('{0}', '3.0 MiB'))
        exact(row, strings.compact)!.click()
        await vi.waitFor(() => expect(order).toContain('restart'))
        expect(alerts.alertConfirm).toHaveBeenCalledWith(strings.compactConfirm)
        expect(order).toEqual(['flush', 'pds_store_compaction_request', 'restart'])
    })

    it('disables database compaction when the files have no free space', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.querySelector('[data-storage-action="compact"]')).not.toBeNull())
        expect(exact(target, languageEnglish.risuNest.storage.compact)?.disabled).toBe(true)
    })

    it('formats large counts with locale separators', async () => {
        const target = setup(Promise.resolve({ ...stats, conversations: { count: 1200, messageCount: 15231 } }))
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))

        expect(target.textContent).toContain(`${(1200).toLocaleString()} chats · ${(15231).toLocaleString()} messages`)
    })


    it('uses a dedicated localized confirmation before deleting a conflict backup', async () => {
        const target = setup()
        alerts.alertConfirm.mockResolvedValue(false)
        await vi.waitFor(() => expect(target.textContent).toContain(languageEnglish.risuNest.storage.conflictBackups))

        const conflictRow = target.querySelectorAll<HTMLElement>('[data-storage-backup-list]')[1]
        conflictRow?.querySelector<HTMLButtonElement>('button')?.click()

        await vi.waitFor(() => expect(alerts.alertConfirm).toHaveBeenCalledWith(
            languageEnglish.risuNest.storage.deleteConflictBackupConfirm,
        ))
        expect(backups.remove).not.toHaveBeenCalled()
    })

    it('orders all backup lists before the storage action rows', async () => {
        const target = setup()
        await vi.waitFor(() => expect(target.textContent).toContain('Create now'))

        const actionRow = target.querySelector<HTMLElement>('[data-storage-action="snapshot"]')
        const lists = [...target.querySelectorAll<HTMLElement>('[data-storage-backup-list]')]
        expect(actionRow).not.toBeNull()
        expect(lists).toHaveLength(3)
        expect(lists.every((list) => Boolean(list.compareDocumentPosition(actionRow!) & Node.DOCUMENT_POSITION_FOLLOWING))).toBe(true)
    })

    it('keeps stale totals visible and leaves one reload control after a post-snapshot reload fails', async () => {
        const target = setup()
        maintenance.createNativePersistentSnapshot.mockResolvedValue({ path: 'new.db', bytes: 1, modifiedAt: 4 })
        await vi.waitFor(() => expect(target.textContent).toContain('Total data'))
        maintenance.getNativePersistentStorageStats.mockRejectedValueOnce(new Error('reload failed'))

        ;[...target.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent?.trim() === 'Create now')?.click()

        await vi.waitFor(() => expect(target.textContent).toContain('Storage totals may be out of date.'))
        expect(target.textContent).toContain('Total data')
        // The header refresh is the only control that reloads the page.
        expect(target.textContent).not.toContain('Retry')
        expect([...target.querySelectorAll<HTMLButtonElement>('button')]
            .filter((button) => button.textContent?.trim() === 'Refresh')).toHaveLength(1)
    })

    it.each([true, false])('delegates the selected snapshot row to the live guarded restore (accepted=%s)', async accepted => {
        const target = setup()
        const selected: string[] = []
        maintenance.restoreNativePersistentSnapshot.mockImplementation(async (actions) => {
            const chosen = await actions.choose([{ id: 'snapshot.db', reason: 'manual', bytes: 1024, modifiedAt: 1 }])
            expect(chosen).toBe('snapshot.db')
            expect(actions).not.toHaveProperty('confirm')
            expect(actions).not.toHaveProperty('restart')
            selected.push(chosen)
            return accepted
        })
        await vi.waitFor(() => expect(exact(target, 'Restore')).toBeDefined())

        exact(target, 'Restore')!.click()

        await vi.waitFor(() => expect(selected).toEqual(['snapshot.db']))
        await vi.waitFor(() => expect(exact(target, 'Restore')?.disabled).toBe(false))
        expect(maintenance.restoreNativePersistentSnapshot).toHaveBeenCalledOnce()
        expect(alerts.alertConfirm).not.toHaveBeenCalled()
        expect(alerts.alertError).not.toHaveBeenCalled()
    })

    it('asks to connect sync when a snapshot restore cannot reach the bound sync target', async () => {
        const target = setup()
        maintenance.restoreNativePersistentSnapshot.mockRejectedValue(
            Object.assign(new Error('Sync is unavailable for library replacement'), { code: 'sync-unavailable' }),
        )
        await vi.waitFor(() => expect(exact(target, 'Restore')).toBeDefined())

        exact(target, 'Restore')!.click()

        await vi.waitFor(() => expect(alerts.alertError).toHaveBeenCalledOnce())
        expect(alerts.alertError).toHaveBeenCalledWith(languageEnglish.risuNest.backup.syncUnavailable)
        expect(alerts.alertError).not.toHaveBeenCalledWith(languageEnglish.risuNest.storage.actionFailed)
    })

    const retryLater = languageEnglish.risuNest.importDialog.reasonBusy
    const offerDataCheck = `${languageEnglish.risuNest.storage.actionFailed} ${languageEnglish.risuNest.dataHealth.openResult}`
    it.each([
        ['library-operation-busy', [], [retryLater]],
        ['commit-busy', [], [retryLater]],
        ['validation', [offerDataCheck], []],
    ])('offers the data check only when the unused file search fails for a reason other than a busy library (%s)', async (code, confirms, errors) => {
        const target = setup()
        maintenance.previewNativePersistentAssetGc.mockRejectedValue({ code })
        alerts.alertConfirm.mockResolvedValue(false)
        await vi.waitFor(() => expect(exact(target, 'Find')).toBeDefined())

        exact(target, 'Find')!.click()

        await vi.waitFor(() => expect(alerts.alertConfirm.mock.calls.length + alerts.alertError.mock.calls.length).toBe(1))
        expect(alerts.alertConfirm.mock.calls.map(([message]) => message)).toEqual(confirms)
        expect(alerts.alertError.mock.calls.map(([message]) => message)).toEqual(errors)
    })

    it('asks for a retry when another operation holds the library as the unused files are deleted', async () => {
        const target = setup()
        maintenance.previewNativePersistentAssetGc.mockResolvedValue({ candidateCount: 1, candidateBytes: 1024, deletedCount: 0, deletedBytes: 0, blockers: [] })
        core.invoke.mockRejectedValueOnce({ code: 'library-operation-busy' })
        alerts.alertConfirm.mockResolvedValue(true)
        await vi.waitFor(() => expect(exact(target, 'Find')).toBeDefined())
        exact(target, 'Find')!.click()
        await vi.waitFor(() => expect(exact(target, 'Delete now')).toBeDefined())

        exact(target, 'Delete now')!.click()

        await vi.waitFor(() => expect(alerts.alertError).toHaveBeenCalledExactlyOnceWith(retryLater))
        expect(core.invoke).toHaveBeenCalledExactlyOnceWith('pds_asset_delete_orphaned_aliases', { expectedRevision: 1, owner: null })
        expect(maintenance.executeNativePersistentAssetGc).not.toHaveBeenCalled()
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
