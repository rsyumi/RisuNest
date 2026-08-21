import { describe, expect, test } from 'vitest'
import { isLegacyBackupAssetKey, legacyBackupIncludesInlays, selectLegacyBackupAssetKeys } from './backupAssets'

describe('legacy backup asset selection', () => {
    test('includes every nonempty assets descendant', () => {
        const keys = ['assets/a.png', 'assets/b.jpg', 'assets/c.mp3', 'assets/d.webm', 'assets/noext', 'assets\\windows.gif']
        expect(selectLegacyBackupAssetKeys(keys)).toEqual(keys)
    })

    test('excludes database, cold, BlobStore, and raw inlay keys', () => {
        for (const key of [
            'assets', 'database/database.bin', 'coldstorage/a', 'coldstorage_a',
            'blobstore/metadata/a.json', 'blobstore/inlays/a.bin', 'raw-inlay-id', 'backup/file',
        ]) expect(isLegacyBackupAssetKey(key)).toBe(false)
        expect(legacyBackupIncludesInlays).toBe(false)
    })
})
