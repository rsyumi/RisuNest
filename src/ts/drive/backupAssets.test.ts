import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { BlobStore } from '../storage/blobStore'
import { configureOfficialAccountAssetReader } from '../storage/accountAssetAccess'
import {
    collectBackupAssetKeys,
    collectPinnedBackupAssetReferences,
    collectReferencedBackupInlays,
    decodeBackupInlayEntry,
    encodeBackupInlayEntry,
    getBackupInlayName,
    isLegacyBackupAssetKey,
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
    })
})

describe('backup inlay entries', () => {
    const metadata = {
        key: 'inlay-1',
        kind: 'inlay',
        size: 3,
        mime: 'image/png',
        name: 'shot.png',
        ext: 'png',
        inlayType: 'image',
        width: 4,
        height: 5,
    } as const

    test('round trips payload and metadata through a single entry', () => {
        const data = new Uint8Array([1, 2, 3])
        const name = getBackupInlayName(metadata.key)

        expect(name).toBe('inlay_696e6c61792d31.risuinlay')
        expect(decodeBackupInlayEntry(name, encodeBackupInlayEntry(metadata, data))).toEqual({
            key: 'inlay-1',
            data,
            metadata: {
                kind: 'inlay',
                mime: 'image/png',
                name: 'shot.png',
                ext: 'png',
                inlayType: 'image',
                width: 4,
                height: 5,
            },
        })
    })

    test('round trips a zero byte payload', () => {
        const name = getBackupInlayName('empty')
        const decoded = decodeBackupInlayEntry(name, encodeBackupInlayEntry(
            { ...metadata, key: 'empty', size: 0 },
            new Uint8Array(),
        ))

        expect(decoded?.data).toEqual(new Uint8Array())
    })

    test('leaves entries it does not own to the asset and cold branches', () => {
        const entry = encodeBackupInlayEntry(metadata, new Uint8Array([1]))

        expect(decodeBackupInlayEntry('profile.png', entry)).toBeNull()
        expect(decodeBackupInlayEntry('coldstorage_a.json', entry)).toBeNull()
        expect(decodeBackupInlayEntry('inlay_zz.risuinlay', entry)).toBeNull()
    })

    test('rejects damaged, mistyped, and asset shadowing entries', () => {
        const name = getBackupInlayName(metadata.key)
        const entry = encodeBackupInlayEntry(metadata, new Uint8Array([1]))

        expect(decodeBackupInlayEntry(name, entry.subarray(0, 3))).toBeNull()
        expect(decodeBackupInlayEntry(name, entry.subarray(0, 6))).toBeNull()
        expect(decodeBackupInlayEntry(name, new Uint8Array([9, 0, 0, 0, 1, 2]))).toBeNull()
        expect(decodeBackupInlayEntry(
            name,
            encodeBackupInlayEntry({ ...metadata, kind: 'asset' } as never, new Uint8Array([1])),
        )).toBeNull()
        expect(decodeBackupInlayEntry(
            name,
            encodeBackupInlayEntry({ ...metadata, inlayType: 'model' } as never, new Uint8Array([1])),
        )).toBeNull()
        expect(decodeBackupInlayEntry(
            name,
            encodeBackupInlayEntry({ ...metadata, key: 'assets/shadow.png' }, new Uint8Array([1])),
        )).toBeNull()
        for (const header of ['null', '[]', '"text"', '{"key":"a","kind":"inlay","inlayType":"image"}']) {
            const payload = new TextEncoder().encode(header)
            const forged = new Uint8Array(4 + payload.byteLength)
            new DataView(forged.buffer).setUint32(0, payload.byteLength, true)
            forged.set(payload, 4)
            expect(decodeBackupInlayEntry(name, forged)).toBeNull()
        }
    })

    test('keeps only inlays referenced by the pinned logical snapshot', async () => {
        const referenced = { chats: [{ data: '{{inlayed::pinned-id}}' }] }
        const list = vi.fn(async () => [
            { ...metadata, key: 'pinned-id' },
            { ...metadata, key: 'created-after-pin' },
        ])
        const store = { list } as unknown as BlobStore

        await expect(collectReferencedBackupInlays(store, referenced)).resolves.toEqual([
            { ...metadata, key: 'pinned-id' },
        ])
    })
})

describe('account backup asset I/O', () => {
    test('derives asset references from one database and cold-payload snapshot', () => {
        const database = {
            customBackground: 'assets/root.png',
            characters: [{
                chaId: 'cold-char', type: 'character', image: 'assets/stub.png', chats: [],
            }],
        }
        const coldPayloads = [{
            character: {
                chaId: 'cold-char', type: 'character', image: 'assets/cold.png', chats: [],
                additionalAssets: [['prop', 'assets/cold-prop.png', 'png']],
            },
        }]

        expect(collectPinnedBackupAssetReferences(
            database as never,
            coldPayloads,
        )).toEqual([
            'assets/cold-prop.png',
            'assets/cold.png',
            'assets/root.png',
        ])
    })

    test('does not add unreferenced live BlobStore payloads to a pinned backup', async () => {
        const list = vi.fn(async () => [
            { key: 'assets/local.png', kind: 'asset' },
            { key: 'sync-conflict-backups/backup-id.risudat', kind: 'asset' },
        ])
        const store = { list } as unknown as BlobStore

        await expect(collectBackupAssetKeys(store, [])).resolves.toEqual([])
        expect(list).not.toHaveBeenCalled()
    })

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

    test('keeps pinned references stable when a live orphan appears after pinning', async () => {
        const list = vi.fn(async () => [
            { key: 'assets/pinned.png', kind: 'asset' },
            { key: 'assets/created-after-pin.png', kind: 'asset' },
        ])
        const store = { list } as unknown as BlobStore

        const keys = await collectBackupAssetKeys(store, [
            'assets/pinned.png',
            'assets/remote-only.png',
        ])

        expect(keys).toEqual(['assets/pinned.png', 'assets/remote-only.png'])
        expect(list).not.toHaveBeenCalled()
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
