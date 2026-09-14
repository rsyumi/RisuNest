import { describe, expect, test, vi } from 'vitest'
import {
    configureActiveBlobStore,
    createBackedBlobStore,
    createBrowserBlobBackend,
    createGatedBlobStore,
    createOpfsBlobBackend,
    createStorageBlobStore,
    createTauriBlobBackend,
    createTauriCasObjectUrl,
    createTauriBlobStore,
    createTauriNativeMediaUrl,
    createNativeMediaEndpointProvider,
    getBlobStore,
    physicalBlobKeys,
    readBlobForFacade,
} from './platformBlobStore'
import { SeekMode } from '@tauri-apps/plugin-fs'
import {
    createInRealmStorageLockManager,
    createStorageMutationGate,
    type StorageMutationGate,
} from './storageMutationGate'

function memoryBackend() {
    const values = new Map<string, Uint8Array>()
    return {
        values,
        backend: {
            write: async (key: string, value: Uint8Array) =>
                void values.set(key, value.slice()),
            read: async (key: string) => values.get(key)?.slice() ?? null,
            keys: async () => [...values.keys()],
            remove: async (key: string) => void values.delete(key),
        },
    }
}

function deferred() {
    let resolve!: () => void
    const promise = new Promise<void>((done) => {
        resolve = done
    })
    return { promise, resolve }
}

describe('platform BlobStore', () => {
    const endpoint = 'http://127.0.0.1:12345/0123456789abcdef0123456789abcdef/'
    test('encodes physical keys into a scoped loopback endpoint without AppData paths', () => {
        expect(
            createTauriNativeMediaUrl('assets/folder/photo.jpg', endpoint),
        ).toBe(`${endpoint}6173736574732f666f6c6465722f70686f746f2e6a7067`)
        expect(() =>
            createTauriNativeMediaUrl('assets/a', 'https://outside.invalid/'),
        ).toThrow('endpoint')
    })

    test('coalesces endpoint requests and retries failed native initialization', async () => {
        const invoke = vi
            .fn()
            .mockRejectedValueOnce(new Error('unavailable'))
            .mockResolvedValue(endpoint)
        const get = createNativeMediaEndpointProvider(invoke)
        await expect(get()).rejects.toThrow('unavailable')
        expect(await Promise.all([get(), get(), get()])).toEqual([
            endpoint,
            endpoint,
            endpoint,
        ])
        expect(invoke).toHaveBeenCalledTimes(2)
        expect(invoke).toHaveBeenLastCalledWith('native_media_base_url')
    })

    test('builds an exact native CAS URL from the alias hash, MIME, and size', () => {
        const hash = 'ab'.repeat(32)

        expect(
            createTauriCasObjectUrl(
                {
                    contentHash: hash,
                    mime: 'image/custom; profile=exact',
                    size: 42,
                },
                endpoint,
            ),
        ).toBe(
            `${endpoint}${Buffer.from(`assets-v2/objects/ab/${hash.slice(2)}`).toString('hex')}` +
                '?mime=image%2Fcustom%3B+profile%3Dexact&size=42',
        )
        expect(() =>
            createTauriCasObjectUrl(
                {
                    contentHash: hash,
                    mime: 'image/png\0text/html',
                    size: 42,
                },
                endpoint,
            ),
        ).toThrow('MIME')
    })

    test('Tauri BlobStore mutates original payloads without native cleanup commands', async () => {
        const events: string[] = []
        const backend = {
            write: async (key: string) => { events.push(`write:${key}`) },
            read: async () => null,
            keys: async () => [],
            remove: async (key: string) => { events.push(`remove:${key}`) },
        }
        const store = createTauriBlobStore(backend)

        await store.put('assets/a.png', new Uint8Array([1]), {
            kind: 'asset', mime: 'image/png', name: 'a', ext: 'png',
        })
        expect(events).toEqual([
            'write:assets/a.png',
            'write:blobstore/metadata/6173736574732f612e706e67.json',
        ])

        events.length = 0
        await store.remove('assets/a.png')
        expect(events).toEqual([
            'remove:assets/a.png',
            'remove:blobstore/metadata/6173736574732f612e706e67.json',
        ])
    })

    test('Tauri new Inlay image writes invoke the native encoder with owned bytes and options', async () => {
        const { backend } = memoryBackend()
        const metadata = {
            key: 'image-id', kind: 'inlay' as const, size: 7, mime: 'image/webp',
            name: 'source.png', ext: 'webp', inlayType: 'image' as const, width: 5, height: 3,
        }
        const invoke = vi.fn(async () => metadata)
        const store = createTauriBlobStore(backend, invoke)
        const source = Uint8Array.of(1, 2, 3)

        const pending = store.putNewInlayImage!('image-id', source, {
            name: 'source.png', options: { format: 'original', quality: 85, maxDimension: 0, skipReencode: false, animationMaxFps: 0 },
        })
        source[0] = 9

        await expect(pending).resolves.toEqual(metadata)
        expect(invoke).toHaveBeenCalledWith('native_media_write_inlay_image', {
            id: 'image-id', data: [1, 2, 3], name: 'source.png',
            options: { format: 'original', quality: 85, maxDimension: 0, skipReencode: false, animationMaxFps: 0 },
        })
    })

    test('Tauri new Inlay image writes stream input larger than one IPC chunk', async () => {
        const { backend } = memoryBackend()
        const metadata = {
            key: 'large-image', kind: 'inlay' as const, size: 64 * 1024 + 1,
            mime: 'image/png', name: 'large.png', ext: 'png', inlayType: 'image' as const,
            width: 1, height: 1,
        }
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_media_inlay_input_open') return { capacity: 64 * 1024 }
            if (command === 'native_media_inlay_input_chunk') {
                return Number(args?.offset) + (args?.data as number[]).length
            }
            if (command === 'native_media_write_inlay_finish') return metadata
            return undefined
        })
        const store = createTauriBlobStore(backend, invoke)

        await expect(store.putNewInlayImage!(
            'large-image',
            new Uint8Array(64 * 1024 + 1),
            { name: 'large.png' },
        )).resolves.toEqual(metadata)

        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'native_media_inlay_input_open',
            'native_media_inlay_input_chunk',
            'native_media_inlay_input_chunk',
            'native_media_write_inlay_finish',
            'native_media_inlay_input_cancel',
        ])
        expect(invoke).not.toHaveBeenCalledWith(
            'native_media_write_inlay_image',
            expect.anything(),
        )
    })

    test('normalizes maximum dimension before native write invocation', async () => {
        const { backend } = memoryBackend()
        const metadata = {
            key: 'image-id', kind: 'inlay' as const, size: 3, mime: 'image/png',
            name: 'source.png', ext: 'png', inlayType: 'image' as const, width: 1, height: 1,
        }
        const invoke = vi.fn(async () => metadata)
        const store = createTauriBlobStore(backend, invoke)

        await store.putNewInlayImage!('image-id', Uint8Array.of(1, 2, 3), {
            name: 'source.png',
            options: {
                format: 'original', quality: 85,
                maxDimension: Number.MAX_SAFE_INTEGER, skipReencode: false, animationMaxFps: 0,
            },
        })

        expect(invoke).toHaveBeenCalledWith('native_media_write_inlay_image', {
            id: 'image-id', data: [1, 2, 3], name: 'source.png',
            options: { format: 'original', quality: 85, maxDimension: 4_294_967_295, skipReencode: false, animationMaxFps: 0 },
        })
    })

    test('gates writes and removals while leaving reads ungated', async () => {
        const { backend } = memoryBackend()
        const events: string[] = []
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) {
                events.push('lock')
                return operation()
            },
            async runKeyedWrite<T>(_key: string, operation: () => Promise<T>) {
                events.push('lock')
                return operation()
            },
            async runTransition<T>(operation: () => Promise<T>) { return operation() },
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

    test('gates a supplied authoritative store by default', async () => {
        const runKeyedWrite = vi.fn(async (
            _key: string,
            operation: () => Promise<unknown>,
        ) => operation())
        const gate: StorageMutationGate = {
            runWrite: (operation) => operation(),
            runKeyedWrite<T>(key: string, operation: () => Promise<T>) {
                return runKeyedWrite(key, operation) as Promise<T>
            },
            runTransition: (operation) => operation(),
        }
        const authoritative = createBackedBlobStore(memoryBackend().backend)
        configureActiveBlobStore(gate, authoritative)

        await getBlobStore().put('assets/a', new Uint8Array([1]), {
            kind: 'asset', mime: 'application/octet-stream', name: 'a', ext: '',
        })

        expect(runKeyedWrite).toHaveBeenCalledTimes(1)
    })

    test('does not gate an already-guarded authoritative store a second time', async () => {
        const runKeyedWrite = vi.fn(async (
            _key: string,
            operation: () => Promise<unknown>,
        ) => operation())
        const gate: StorageMutationGate = {
            runWrite: (operation) => operation(),
            runKeyedWrite<T>(key: string, operation: () => Promise<T>) {
                return runKeyedWrite(key, operation) as Promise<T>
            },
            runTransition: (operation) => operation(),
        }
        const authoritative = createBackedBlobStore(memoryBackend().backend)
        configureActiveBlobStore(gate, authoritative, { alreadyGuarded: true })

        await getBlobStore().put('assets/a', new Uint8Array([1]), {
            kind: 'asset', mime: 'application/octet-stream', name: 'a', ext: '',
        })

        expect(runKeyedWrite).not.toHaveBeenCalled()
    })

    test('owns blob bytes before a deferred write gate proceeds', async () => {
        const { backend } = memoryBackend()
        let release!: () => void
        const blocked = new Promise<void>((resolve) => { release = resolve })
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) { await blocked; return operation() },
            async runKeyedWrite<T>(_key: string, operation: () => Promise<T>) { await blocked; return operation() },
            async runTransition<T>(operation: () => Promise<T>) { return operation() },
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

    test('gates native Inlay image writes and owns their inputs', async () => {
        let release!: () => void
        const blocked = new Promise<void>((resolve) => { release = resolve })
        const optimized = vi.fn(async (key: string, data: Uint8Array, input: { name: string }) => ({
            key, kind: 'inlay' as const, size: data.byteLength, mime: 'image/webp',
            name: input.name, ext: 'webp', inlayType: 'image' as const, width: 1, height: 1,
        }))
        const store = {
            ...createBackedBlobStore(memoryBackend().backend),
            putNewInlayImage: optimized,
        }
        const gated = createGatedBlobStore(store, {
            async runWrite<T>(operation: () => Promise<T>) { await blocked; return operation() },
            async runKeyedWrite<T>(_key: string, operation: () => Promise<T>) { await blocked; return operation() },
            async runTransition<T>(operation: () => Promise<T>) { return operation() },
        })
        const source = Uint8Array.of(4, 5)
        const input = { name: 'original.png' }

        const pending = gated.putNewInlayImage!('owned', source, input)
        source[0] = 8
        input.name = 'mutated.png'
        release()
        await pending

        expect(optimized).toHaveBeenCalledWith('owned', Uint8Array.of(4, 5), { name: 'original.png' })
    })

    test('prevents native optimization from interleaving with opaque restore for the same Inlay', async () => {
        const opaqueRelease = deferred()
        const events: string[] = []
        const store = {
            ...createBackedBlobStore(memoryBackend().backend),
            async put(key: string, data: Uint8Array, metadata: any) {
                events.push('opaque-start')
                await opaqueRelease.promise
                events.push('opaque-end')
                return { ...metadata, key, size: data.byteLength }
            },
            async putNewInlayImage(key: string, data: Uint8Array, input: { name: string }) {
                events.push('optimized')
                return {
                    key, kind: 'inlay' as const, size: data.byteLength, mime: 'image/webp',
                    name: input.name, ext: 'webp', inlayType: 'image' as const, width: 1, height: 1,
                }
            },
        }
        const gated = createGatedBlobStore(
            store,
            createStorageMutationGate({ locks: createInRealmStorageLockManager() }),
        )

        const opaque = gated.put('same-id', Uint8Array.of(1), {
            kind: 'inlay', mime: 'image/png', name: 'restore.png', ext: 'png', inlayType: 'image',
        })
        const optimized = gated.putNewInlayImage!('same-id', Uint8Array.of(2), { name: 'new.png' })

        await Promise.resolve()
        await Promise.resolve()
        expect(events).toEqual(['opaque-start'])
        opaqueRelease.resolve()
        await Promise.all([opaque, optimized])
        expect(events).toEqual(['opaque-start', 'opaque-end', 'optimized'])
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

    test('OPFS write abort preserves the write failure and does not close an errored stream', async () => {
        const writeError = new DOMException('quota exhausted', 'QuotaExceededError')
        const abort = vi.fn(async () => undefined)
        const close = vi.fn(async () => undefined)
        const directory = {
            getFileHandle: async () => ({
                createWritable: async () => ({
                    write: async () => { throw writeError },
                    abort,
                    close,
                }),
            }),
        } as unknown as FileSystemDirectoryHandle
        const backend = createOpfsBlobBackend(directory)

        await expect(backend.write('assets/a', Uint8Array.of(1))).rejects.toBe(writeError)
        expect(abort).toHaveBeenCalledWith(writeError)
        expect(close).not.toHaveBeenCalled()
    })

    test('OPFS write reports an abort failure without replacing the write failure', async () => {
        const writeError = new Error('write failed')
        const abortError = new Error('abort failed')
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const directory = {
            getFileHandle: async () => ({
                createWritable: async () => ({
                    write: async () => { throw writeError },
                    abort: async () => { throw abortError },
                    close: vi.fn(),
                }),
            }),
        } as unknown as FileSystemDirectoryHandle
        const backend = createOpfsBlobBackend(directory)

        await expect(backend.write('assets/a', Uint8Array.of(1))).rejects.toBe(writeError)
        expect(consoleError).toHaveBeenCalledWith('OPFS blob write abort failed', abortError)
        consoleError.mockRestore()
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

    test('rejects an unconfigured browser storage provider', async () => {
        await expect(createBrowserBlobBackend(null)).rejects.toThrow(
            'Blob storage provider is not configured',
        )
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
