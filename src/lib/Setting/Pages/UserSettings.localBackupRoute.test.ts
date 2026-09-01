import { describe, expect, it } from 'vitest'

import source from './UserSettings.svelte?raw'

describe('UserSettings local backup route', () => {
    it('routes the regular save and restore buttons through the production lossless caller', () => {
        expect(source).toContain('exportLocalBackupFromSystemPicker')
        expect(source).toContain('restoreLocalBackupFromSystemPicker')
        expect(source).toContain('await runLocalBackupOperation(\'export\')')
        expect(source).toContain('await runLocalBackupOperation(\'import\')')
    })

    it('keeps upstream local backup, account, and Drive controls on this page', () => {
        expect(source).toContain('SavePartialLocalBackup()')
        expect(source).toContain('loadRisuAccountData')
        expect(source).toContain('checkDriver')
    })

    it('moves RisuNest backup and sync controls to the dedicated page', async () => {
        const backupSource = await import('./RisuNestBackupRestore.svelte?raw')

        for (const control of [
            'runRisuSaveOperation',
            'LoadLocalBackup()',
            'restoreNativePersistentSnapshot',
            'openSyncConflictBackups()',
            'getNativeOfficialAccountFlow().publish',
            'getNativeOfficialAccountFlow().restore',
            'nativePublishController?.abort()',
        ]) {
            expect(backupSource.default).toContain(control)
            expect(source).not.toContain(control)
        }
    })
})
