import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    backupStore: {
        list: vi.fn(),
        read: vi.fn(),
        save: vi.fn(),
    },
    alertSelect: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
    decodeRisuSave: vi.fn(),
    installLocalBackup: vi.fn(),
    replacePersistentDatabase: vi.fn(),
    publishCurrentOfficialRevision: vi.fn(),
    withFlushedRisuSaveExport: vi.fn(),
    runtime: { store: {}, revision: 7, flushPendingData: vi.fn() },
}))

vi.mock('src/lang', () => ({
    language: {
        syncBackupSideLocal: 'local',
        syncBackupSideRemote: 'remote',
        syncBackupEntry: '{date} {side} {count}',
        syncBackupDatabaseOnly: 'database only, no assets/cold/inlays',
        syncConflictBackups: 'backups',
        syncConflictNoBackups: 'no backups',
        syncConflictRestoreConfirm: 'restore database only?',
    },
}))
vi.mock('../../alert', () => ({
    alertSelect: mocks.alertSelect,
    alertConfirm: mocks.alertConfirm,
    alertError: mocks.alertError,
    alertNormal: mocks.alertNormal,
}))
vi.mock('../database.svelte', () => ({
    getDatabase: () => ({
        characters: [{ type: 'character', chaId: 'catalog-only', chats: [] }],
    }),
}))
vi.mock('../databaseRestore', () => ({ installLocalBackup: mocks.installLocalBackup }))
vi.mock('../persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => mocks.runtime,
    replacePersistentDatabase: mocks.replacePersistentDatabase,
    publishCurrentOfficialRevision: mocks.publishCurrentOfficialRevision,
}))
vi.mock('../risuSave', () => ({
    decodeRisuSave: mocks.decodeRisuSave,
    encodeRisuSaveLegacy: vi.fn(() => new Uint8Array([99])),
}))
vi.mock('../risuSaveStoreAdapter', () => ({
    withFlushedRisuSaveExport: mocks.withFlushedRisuSaveExport,
}))
vi.mock('./syncConflictBackup', () => ({
    getSyncConflictBackupStore: () => mocks.backupStore,
}))

describe('openSyncConflictBackups', () => {
    beforeEach(() => {
        vi.resetModules()
        const entry = {
            id: 'backup',
            createdAt: 1,
            side: 'remote',
            characterCount: 1,
            byteLength: 3,
            scope: 'database-only',
        }
        mocks.backupStore.list.mockReset().mockResolvedValue([entry])
        mocks.backupStore.read.mockReset().mockResolvedValue(new Uint8Array([1, 2, 3]))
        mocks.backupStore.save.mockReset().mockResolvedValue(undefined)
        mocks.alertSelect.mockReset().mockResolvedValue('0')
        mocks.alertConfirm.mockReset().mockResolvedValue(true)
        mocks.alertError.mockReset()
        mocks.alertNormal.mockReset()
        mocks.decodeRisuSave.mockReset().mockResolvedValue({
            characters: [{ type: 'character', chaId: 'remote', chats: [] }],
        })
        mocks.installLocalBackup.mockReset().mockImplementation(async (database, dependencies) => {
            await dependencies.replaceDatabase(database, 'local-backup')
        })
        mocks.withFlushedRisuSaveExport.mockReset().mockImplementation(
            async (_runtime, _reason, callback) => callback({
                revision: 7,
                mutationGeneration: 9,
                collectBytes: vi.fn(async () => new Uint8Array([7, 8, 9])),
                countCharacters: vi.fn(async () => 2),
            }),
        )
    })

    it('pins the authoritative local revision before restoring the selected backup', async () => {
        const { openSyncConflictBackups } = await import('./syncConflictRestore')

        await openSyncConflictBackups()

        expect(mocks.withFlushedRisuSaveExport).toHaveBeenCalledWith(
            mocks.runtime,
            'sync-conflict-restore-safety-backup',
            expect.any(Function),
        )
        expect(mocks.backupStore.save).toHaveBeenCalledWith({
            side: 'local',
            bytes: new Uint8Array([7, 8, 9]),
            characterCount: 2,
        })
        expect(mocks.alertSelect).toHaveBeenCalledWith(
            [expect.stringContaining('database only, no assets/cold/inlays')],
            'backups',
        )
        expect(mocks.alertConfirm).toHaveBeenCalledWith('restore database only?')
        expect(mocks.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ characters: expect.any(Array) }),
            'local-backup',
            {
                authoritative: true,
                expectedRevision: 7,
                expectedMutationGeneration: 9,
            },
        )
        expect(mocks.installLocalBackup).toHaveBeenCalledOnce()
    })
})
