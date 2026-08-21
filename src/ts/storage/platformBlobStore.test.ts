import { describe, expect, test, vi } from 'vitest'
import {
    createBrowserBlobBackend,
    createOpfsBlobBackend,
    createStorageRootedBlobStoreFactory,
    createTauriBlobBackend,
    createKeyValueRootedBlobStoreFactory,
    createResolvingBlobStore,
    physicalBlobKeys,
    readBlobForFacade,
} from './platformBlobStore'
import { SeekMode } from '@tauri-apps/plugin-fs'

function memoryBackend() {
    const values = new Map<string, Uint8Array>()
    return {
        values,
        backend: {
            write: async (key: string, value: Uint8Array) => void values.set(key, value.slice()),
            read: async (key: string) => values.get(key)?.slice() ?? null,
            keys: async () => [...values.keys()],
            remove: async (key: string) => void values.delete(key),
        },
    }
}

describe('rooted BlobStore mapping', () => {
    test('preserves legacy paths and encodes raw inlay ids', () => {
        expect(physicalBlobKeys({ kind: 'legacy' }, 'assets/photo.jpg')).toEqual({
            payload: 'assets/photo.jpg',
            metadata: 'blobstore/metadata/6173736574732f70686f746f2e6a7067.json',
        })
        expect(physicalBlobKeys({ kind: 'legacy' }, '../raw')).toEqual({
            payload: 'blobstore/inlays/2e2e2f726177.bin',
            metadata: 'blobstore/metadata/2e2e2f726177.json',
        })
    })

    test('isolates equal logical keys across generated roots', async () => {
        const { backend } = memoryBackend()
        const factory = createKeyValueRootedBlobStoreFactory(backend)
        const first = factory.open({ kind: 'generation', id: 'first_1' })
        const second = factory.open({ kind: 'generation', id: 'second-2' })
        const metadata = { kind: 'asset' as const, mime: 'image/png', name: 'same.png', ext: 'png' }
        await first.put('assets/same.png', new Uint8Array([1]), metadata)
        await second.put('assets/same.png', new Uint8Array([2]), metadata)
        expect(await first.read('assets/same.png')).toEqual(new Uint8Array([1]))
        expect(await second.read('assets/same.png')).toEqual(new Uint8Array([2]))
        expect(await first.list()).toHaveLength(1)
        await first.remove('assets/same.png')
        expect(await second.read('assets/same.png')).toEqual(new Uint8Array([2]))
    })

    test('rejects unsafe generation identifiers', () => {
        const { backend } = memoryBackend()
        const factory = createKeyValueRootedBlobStoreFactory(backend)
        expect(() => factory.open({ kind: 'generation', id: '../escape' })).toThrow(TypeError)
    })

    test('resolves one active root for a complete public operation', async () => {
        const { backend } = memoryBackend()
        const factory = createKeyValueRootedBlobStoreFactory(backend)
        let calls = 0
        const store = createResolvingBlobStore(factory, {
            async getActiveRoot() {
                calls += 1
                return calls === 1 ? { kind: 'generation', id: 'one' } : { kind: 'generation', id: 'two' }
            },
        })
        await store.put('assets/a', new Uint8Array([1]), {
            kind: 'asset', mime: 'application/octet-stream', name: 'a', ext: '',
        })
        expect(calls).toBe(1)
        expect(await factory.open({ kind: 'generation', id: 'one' }).read('assets/a')).toEqual(new Uint8Array([1]))
    })

    test('refuses AccountStorage before invoking it', () => {
        const storage = {
            setItem: async () => { throw new Error('must not run') },
            getItem: async () => { throw new Error('must not run') },
            keys: async () => { throw new Error('must not run') },
            removeItem: async () => { throw new Error('must not run') },
        }
        expect(() => createStorageRootedBlobStoreFactory({ storage, isAccount: true })).toThrow(TypeError)
    })

    test('Tauri bounded reads clamp, seek once, fill, and close', async () => {
        const source = new Uint8Array([0, 1, 2, 3, 4])
        let cursor = 0
        const seeks: number[] = []
        const readSizes: number[] = []
        let closes = 0
        const backend = createTauriBlobBackend({
            exists: async () => true,
            mkdir: async () => {}, write: async () => {}, read: async () => source,
            list: async () => [], remove: async () => {}, size: async () => source.byteLength,
            open: async () => ({
                async seek(offset, mode) { expect(mode).toBe(SeekMode.Start); seeks.push(offset); cursor = offset; return cursor },
                async read(buffer) {
                    readSizes.push(buffer.byteLength)
                    const count = Math.min(1, buffer.byteLength, source.byteLength - cursor)
                    if (count <= 0) return null
                    buffer[0] = source[cursor++]
                    return count
                },
                async close() { closes += 1 },
            }),
            resolveUrl: async (key) => `asset://${key}`,
        })
        expect(await backend.readRange!('assets/a', { start: 2, endExclusive: 99 })).toEqual(new Uint8Array([2, 3, 4]))
        expect(seeks).toEqual([2])
        expect(Math.max(...readSizes)).toBeLessThanOrEqual(3)
        expect(closes).toBe(1)
        expect(await backend.resolveUrl!('assets/a')).toBe('asset://assets/a')
    })

    test('does not resolve a URL for a missing logical key', async () => {
        const backend = createTauriBlobBackend({
            exists: async () => false, mkdir: async () => {}, write: async () => {}, read: async () => new Uint8Array(),
            list: async () => [], remove: async () => {}, size: async () => 0,
            open: async () => { throw new Error('not used') }, resolveUrl: async (key) => `asset://${key}`,
        })
        const store = createKeyValueRootedBlobStoreFactory(backend).open({ kind: 'legacy' })
        expect(await store.resolveUrl('assets/missing')).toBeNull()
    })

    test('Tauri resolves generated-root URLs from the isolated physical path', async () => {
        const values = new Map<string, Uint8Array>()
        const backend = createTauriBlobBackend({
            exists: async (key) => values.has(key), mkdir: async () => {},
            write: async (key, value) => void values.set(key, value.slice()),
            read: async (key) => values.get(key)!,
            list: async (prefix) => [...values.keys()].filter((key) => key.startsWith(prefix)),
            remove: async (key) => void values.delete(key), size: async (key) => values.get(key)!.byteLength,
            open: async () => { throw new Error('not used') }, resolveUrl: async (key) => `asset://${key}`,
        })
        const store = createKeyValueRootedBlobStoreFactory(backend).open({ kind: 'generation', id: 'stage_1' })
        await store.put('assets/photo.jpg', new Uint8Array([1]), {
            kind: 'asset', mime: 'image/jpeg', name: 'photo.jpg', ext: 'jpg',
        })
        expect(await store.resolveUrl('assets/photo.jpg')).toBe(
            'asset://blobstore/generations/stage_1/assets/photo.jpg',
        )
    })

    test('Tauri bounded reads close after a read failure', async () => {
        let closes = 0
        const backend = createTauriBlobBackend({
            exists: async () => true, mkdir: async () => {}, write: async () => {}, read: async () => new Uint8Array(),
            list: async () => [], remove: async () => {}, size: async () => 2,
            open: async () => ({
                async seek() { return 0 },
                async read() { throw new Error('read failed') },
                async close() { closes += 1 },
            }),
            resolveUrl: async () => '',
        })
        await expect(backend.readRange!('assets/a', { start: 0, endExclusive: 2 })).rejects.toThrow('read failed')
        expect(closes).toBe(1)
    })

    test('OPFS bounded reads use File.slice', async () => {
        const sliceCalls: [number, number][] = []
        const file = new File([new Uint8Array([0, 1, 2, 3])], 'value')
        const originalSlice = file.slice.bind(file)
        file.slice = ((start?: number, end?: number) => {
            sliceCalls.push([start!, end!])
            return originalSlice(start, end)
        }) as typeof file.slice
        const directory = {
            getFileHandle: async () => ({ getFile: async () => file }),
        } as unknown as FileSystemDirectoryHandle
        const backend = createOpfsBlobBackend(directory)
        expect(await backend.readRange!('assets/a', { start: 1, endExclusive: 3 })).toEqual(new Uint8Array([1, 2]))
        expect(sliceCalls).toEqual([[1, 3]])
    })

    test('production browser selection keeps OPFS reads bounded', async () => {
        const getItem = vi.fn(async () => new Uint8Array([0, 1, 2, 3]))
        const selected = {
            blobStorageKind: 'opfs' as const,
            setItem: vi.fn(async () => undefined),
            getItem,
            keys: vi.fn(async () => []),
            removeItem: vi.fn(async () => undefined),
        }
        const file = new File([new Uint8Array([0, 1, 2, 3])], 'value')
        const directory = {
            getFileHandle: async () => ({ getFile: async () => file }),
        } as unknown as FileSystemDirectoryHandle

        const backend = await createBrowserBlobBackend(selected, async () => directory)

        expect(await backend.readRange!('assets/a', { start: 1, endExclusive: 3 })).toEqual(new Uint8Array([1, 2]))
        expect(getItem).not.toHaveBeenCalled()
    })

    test('facade rejects missing native blobs', async () => {
        const store = createKeyValueRootedBlobStoreFactory(memoryBackend().backend).open({ kind: 'legacy' })

        await expect(readBlobForFacade(store, 'assets/missing', true)).rejects.toThrow('Missing asset')
    })

    test('facade preserves nullable browser reads', async () => {
        const store = createKeyValueRootedBlobStoreFactory(memoryBackend().backend).open({ kind: 'legacy' })

        await expect(readBlobForFacade(store, 'assets/missing', false)).resolves.toBeNull()
    })
})
