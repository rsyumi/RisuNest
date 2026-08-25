import { describe, expect, test, vi } from 'vitest'
import {
    createBackedBlobStore,
    createBrowserBlobBackend,
    createGatedBlobStore,
    createOpfsBlobBackend,
    createStorageBlobStore,
    createTauriBlobBackend,
    createTauriBlobStore,
    createTauriNativeMediaUrl,
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

describe('platform BlobStore', () => {

    test('encodes physical keys into the risuasset protocol without exposing AppData paths', () => {
        const convert = vi.fn((path: string, protocol?: string) => `${protocol}://${path}`)
        expect(createTauriNativeMediaUrl('assets/folder/photo.jpg', convert)).toBe(
            'risuasset://6173736574732f666f6c6465722f70686f746f2e6a7067',
        )
        expect(convert).toHaveBeenCalledWith('6173736574732f666f6c6465722f70686f746f2e6a7067', 'risuasset')
    })

    test('Tauri BlobStore invokes thumbnail cleanup after successful puts and removals', async () => {
        const events: string[] = []
        const backend = {
            write: async (key: string) => { events.push(`write:${key}`) },
            read: async () => null,
            keys: async () => [],
            remove: async (key: string) => { events.push(`remove:${key}`) },
        }
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${args?.physicalKey}`)
        })
        const store = createTauriBlobStore(backend, invoke)

        await store.put('assets/a.png', new Uint8Array([1]), {
            kind: 'asset', mime: 'image/png', name: 'a', ext: 'png',
        })
        expect(events).toEqual([
            'write:assets/a.png',
            'write:blobstore/metadata/6173736574732f612e706e67.json',
            'invoke:native_media_remove_thumbnails:assets/a.png',
        ])
        expect(invoke).toHaveBeenLastCalledWith('native_media_remove_thumbnails', {
            physicalKey: 'assets/a.png',
        })

        events.length = 0
        await store.remove('assets/a.png')
        expect(events).toEqual([
            'remove:assets/a.png',
            'remove:blobstore/metadata/6173736574732f612e706e67.json',
            'invoke:native_media_remove_thumbnails:assets/a.png',
        ])
    })

    test('Tauri BlobStore keeps successful mutations when thumbnail cleanup fails', async () => {
        const backend = {
            write: vi.fn(async () => undefined),
            read: async () => null,
            keys: async () => [],
            remove: vi.fn(async () => undefined),
        }
        const cleanupError = new Error('cleanup unavailable')
        const invoke = vi.fn(async () => { throw cleanupError })
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        const store = createTauriBlobStore(backend, invoke)

        try {
            await expect(store.put('assets/a.png', new Uint8Array([1]), {
                kind: 'asset', mime: 'image/png', name: 'a', ext: 'png',
            })).resolves.toMatchObject({ key: 'assets/a.png' })
            await expect(store.remove('assets/a.png')).resolves.toBeUndefined()

            expect(invoke).toHaveBeenCalledTimes(2)
            expect(warn).toHaveBeenCalledTimes(2)
            expect(warn).toHaveBeenCalledWith('Failed to clean native media thumbnails', cleanupError)
        } finally {
            warn.mockRestore()
        }
    })

    test('gates writes and removals while leaving reads ungated', async () => {
        const { backend } = memoryBackend()
        const events: string[] = []
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) {
                events.push('lock')
                return operation()
            },
        }
        const store = createGatedBlobStore(createBackedBlobStore(backend), gate)
        const metadata = { kind: 'asset' as const, mime: 'application/octet-stream', name: 'a', ext: '' }

        await store.put('assets/a', new Uint8Array([1]), metadata)
        expect(events).toEqual(['lock'])
        expect(await store.read('assets/a')).toEqual(new Uint8Array([1]))

        events.length = 0
        await store.read('assets/a')
        await store.stat('assets/a')
        await store.list()
        await store.resolveUrl('assets/a')
        expect(events).toEqual([])

        await store.remove('assets/a')
        expect(events).toEqual(['lock'])
        expect(await store.read('assets/a')).toBeNull()
    })

    test('owns blob bytes before a deferred write gate proceeds', async () => {
        const { backend } = memoryBackend()
        let release!: () => void
        const blocked = new Promise<void>((resolve) => { release = resolve })
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) { await blocked; return operation() },
        }
        const backing = createBackedBlobStore(backend)
        const store = createGatedBlobStore(backing, gate)
        const source = new Uint8Array([1])
        const pending = store.put('assets/a', source, {
            kind: 'asset', mime: 'application/octet-stream', name: 'a', ext: '',
        })
        source[0] = 9
        release()

        await pending
        expect(await backing.read('assets/a')).toEqual(new Uint8Array([1]))
    })

    test('preserves legacy paths and encodes raw inlay ids', () => {
        expect(physicalBlobKeys('assets/photo.jpg')).toEqual({
            payload: 'assets/photo.jpg',
            metadata: 'blobstore/metadata/6173736574732f70686f746f2e6a7067.json',
        })
        expect(physicalBlobKeys('../raw')).toEqual({
            payload: 'blobstore/inlays/2e2e2f726177.bin',
            metadata: 'blobstore/metadata/2e2e2f726177.json',
        })
    })





    test('refuses AccountStorage before invoking it', () => {
        const storage = {
            setItem: async () => { throw new Error('must not run') },
            getItem: async () => { throw new Error('must not run') },
            keys: async () => { throw new Error('must not run') },
            removeItem: async () => { throw new Error('must not run') },
        }
        expect(() => createStorageBlobStore({ storage, isAccount: true })).toThrow(TypeError)
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

    test('Tauri keys include legacy cold payload files', async () => {
        const listed: string[] = []
        const backend = createTauriBlobBackend({
            exists: async () => false,
            mkdir: async () => {},
            write: async () => {},
            read: async () => new Uint8Array(),
            list: async (path) => {
                listed.push(path)
                return path === 'coldstorage' ? ['coldstorage/one.json'] : []
            },
            remove: async () => {},
            size: async () => 0,
            open: async () => { throw new Error('not used') },
            resolveUrl: async () => '',
        })

        expect(await backend.keys()).toEqual(['coldstorage/one.json'])
        expect(listed).toEqual(['assets', 'blobstore', 'coldstorage'])
    })

    test('does not resolve a URL for a missing logical key', async () => {
        const backend = createTauriBlobBackend({
            exists: async () => false, mkdir: async () => {}, write: async () => {}, read: async () => new Uint8Array(),
            list: async () => [], remove: async () => {}, size: async () => 0,
            open: async () => { throw new Error('not used') }, resolveUrl: async (key) => `asset://${key}`,
        })
        const store = createBackedBlobStore(backend)
        expect(await store.resolveUrl('assets/missing')).toBeNull()
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

    test('OPFS keys skip foreign non-hex names in the shared root', async () => {
        const entries = [
            { kind: 'file', name: Buffer.from('assets/a.png', 'utf-8').toString('hex') },
            { kind: 'file', name: 'coldstorage_3f6b.json' },
            { kind: 'file', name: 'ABCDEF' },
            { kind: 'file', name: 'abc' },
            { kind: 'directory', name: '6162' },
        ]
        const directory = {
            values: async function* () { yield* entries },
        } as unknown as FileSystemDirectoryHandle
        const backend = createOpfsBlobBackend(directory)

        expect(await backend.keys()).toEqual(['assets/a.png'])
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
        const store = createBackedBlobStore(memoryBackend().backend)

        await expect(readBlobForFacade(store, 'assets/missing', true)).rejects.toThrow('Missing asset')
    })

    test('facade preserves nullable browser reads', async () => {
        const store = createBackedBlobStore(memoryBackend().backend)

        await expect(readBlobForFacade(store, 'assets/missing', false)).resolves.toBeNull()
    })
})
