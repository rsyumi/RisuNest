import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { BlobStore } from './blobStore'
import {
    configureOfficialAccountAssetReader,
    createStructuredAccountAssetReader,
    readActiveAsset,
    storeActiveAsset,
} from './accountAssetAccess'

function makeBlobStore(value: Uint8Array | null = new Uint8Array([1, 2, 3])) {
    const read = vi.fn(async () => value)
    const put = vi.fn(async () => undefined)
    return {
        read,
        put,
        store: { read, put } as unknown as BlobStore,
    }
}

describe('account asset access', () => {
    beforeEach(() => configureOfficialAccountAssetReader(null))

    it('uses active BlobStore bytes before the official account reader', async () => {
        const local = makeBlobStore()
        const readRemote = vi.fn(async () => new Uint8Array([9, 8, 7]))
        configureOfficialAccountAssetReader(readRemote)

        await expect(readActiveAsset(local.store, 'assets/local.png', {
            officialAccount: true,
            tauri: false,
        })).resolves.toEqual(new Uint8Array([1, 2, 3]))

        expect(local.read).toHaveBeenCalledWith('assets/local.png')
        expect(readRemote).not.toHaveBeenCalled()
    })

    it('reads remote-only bytes after official account bootstrap succeeds', async () => {
        const local = makeBlobStore(null)
        const readRemote = vi.fn(async () => new Uint8Array([9, 8, 7]))
        configureOfficialAccountAssetReader(readRemote)

        await expect(readActiveAsset(local.store, 'assets/remote.png', {
            officialAccount: true,
            tauri: false,
        })).resolves.toEqual(new Uint8Array([9, 8, 7]))

        expect(local.read).toHaveBeenCalledWith('assets/remote.png')
        expect(readRemote).toHaveBeenCalledWith('assets/remote.png')
    })

    it('reads remote-only bytes for Tauri byte consumers in account mode', async () => {
        const local = makeBlobStore(null)
        const readRemote = vi.fn(async () => new Uint8Array([6, 5, 4]))
        configureOfficialAccountAssetReader(readRemote)

        await expect(readActiveAsset(local.store, 'assets/remote.ogg', {
            officialAccount: true,
            tauri: true,
        })).resolves.toEqual(new Uint8Array([6, 5, 4]))

        expect(local.read).toHaveBeenCalledWith('assets/remote.ogg')
        expect(readRemote).toHaveBeenCalledWith('assets/remote.ogg')
    })

    it('uses local BlobStore bytes while official account access is disabled', async () => {
        const local = makeBlobStore()

        await expect(readActiveAsset(local.store, 'assets/local.png', {
            officialAccount: false,
            tauri: false,
        })).resolves.toEqual(new Uint8Array([1, 2, 3]))

        expect(local.read).toHaveBeenCalledWith('assets/local.png')
    })

    it('reads the active BlobStore root for local Tauri assets and rejects missing bytes', async () => {
        const local = makeBlobStore()

        await expect(readActiveAsset(local.store, 'assets/generated.png', {
            officialAccount: false,
            tauri: true,
        })).resolves.toEqual(new Uint8Array([1, 2, 3]))

        local.read.mockResolvedValueOnce(null)
        await expect(readActiveAsset(local.store, 'assets/missing.png', {
            officialAccount: false,
            tauri: true,
        })).rejects.toThrow('Missing asset: assets/missing.png')
    })

    it('stores account assets through BlobStore metadata and root tracking', async () => {
        const local = makeBlobStore()
        configureOfficialAccountAssetReader(vi.fn())
        const data = new Uint8Array([4, 5, 6])

        await storeActiveAsset(local.store, 'assets/new.png', data, {
            kind: 'asset',
            mime: '',
            name: 'new.png',
            ext: 'png',
        })

        expect(local.put).toHaveBeenCalledWith('assets/new.png', data, {
            kind: 'asset',
            mime: '',
            name: 'new.png',
            ext: 'png',
        })
    })

    it('returns cached bytes from an official not-modified response', async () => {
        const reader = createStructuredAccountAssetReader({
            readItem: vi.fn(async () => ({
                kind: 'not-modified' as const,
                bytes: new Uint8Array([3, 2, 1]),
            })),
        })

        await expect(reader('assets/cached.png')).resolves.toEqual(new Uint8Array([3, 2, 1]))
    })
})
