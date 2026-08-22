import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { BlobStore } from '../storage/blobStore'
import { configureOfficialAccountAssetReader } from '../storage/accountAssetAccess'
import {
    collectBackupAssetKeys,
    isLegacyBackupAssetKey,
    legacyBackupIncludesInlays,
    readBackupAsset,
    selectLegacyBackupAssetKeys,
    writeBackupAsset,
} from './backupAssets'

beforeEach(() => configureOfficialAccountAssetReader(null))

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

describe('account backup asset I/O', () => {
    test('combines active-root assets with remote-only database references', async () => {
        const localBytes = new Uint8Array([1])
        const read = vi.fn(async (key: string) => key === 'assets/local.png' ? localBytes : null)
        const list = vi.fn(async () => [{ key: 'assets/local.png', kind: 'asset' }])
        const store = { read, list } as unknown as BlobStore
        const readRemote = vi.fn(async () => new Uint8Array([9]))
        configureOfficialAccountAssetReader(readRemote)

        const keys = await collectBackupAssetKeys(store, [
            'assets/local.png',
            'assets/remote.png',
            'https://example.invalid/not-an-asset.png',
        ])

        expect(keys).toEqual(['assets/local.png', 'assets/remote.png'])
        await expect(readBackupAsset(store, 'assets/local.png', true)).resolves.toEqual(localBytes)
        await expect(readBackupAsset(store, 'assets/remote.png', true)).resolves.toEqual(
            new Uint8Array([9]),
        )
        expect(readRemote).toHaveBeenCalledOnce()
        expect(readRemote).toHaveBeenCalledWith('assets/remote.png')
    })

    test('restores through BlobStore metadata on a Tauri-shaped backend', async () => {
        const put = vi.fn(async () => undefined)
        const store = { put } as unknown as BlobStore
        const bytes = new Uint8Array([4, 5])

        await writeBackupAsset(store, 'assets/restored.webp', bytes)

        expect(put).toHaveBeenCalledWith('assets/restored.webp', bytes, {
            kind: 'asset',
            mime: '',
            name: 'restored.webp',
            ext: 'webp',
        })
    })
})
