import { describe, expect, it } from 'vitest'

import source from './UserSettings.svelte?raw'

describe('UserSettings local backup route', () => {
    it('routes the regular save and restore buttons through the production lossless caller', () => {
        expect(source).toContain('exportLocalBackupFromSystemPicker')
        expect(source).toContain('restoreLocalBackupFromSystemPicker')
        expect(source).toContain('await runLocalBackupOperation(\'export\')')
        expect(source).toContain('await runLocalBackupOperation(\'import\')')
    })

    it('keeps partial and PocketRisu backup compatibility on their legacy callers', () => {
        expect(source).toContain('SavePartialLocalBackup()')
        expect(source).toContain('LoadLocalBackup()')
    })
})
