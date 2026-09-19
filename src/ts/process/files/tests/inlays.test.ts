import { setRuntimePerformanceProfile } from 'src/ts/runtimePerformanceProfile'
vi.mock('src/ts/alert', () => ({ alertToast: vi.fn() }))
import fc from 'fast-check'
import localforage from 'localforage'
import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { InlayAsset } from '../inlays'
import {
    getInlayAsset,
    getInlayAssetBlob,
    getInlayEncodeOptions,
    getInlayAssetRenderUrl,
    listInlayAssets,
    listInlayAssetMetadata,
    postInlayAsset,
    reencodeImage,
    removeInlayAsset,
    saveInlayedSignature,
    setInlayAsset,
    writeInlayImage,
    InlayInputTooLargeError,
    maxNewInlayInputBytes,
} from '../inlays'
import {
    configureBlobStoreStorageProvider,
    createBackedBlobStore,
} from 'src/ts/storage/platformBlobStore'
import { getDatabase } from 'src/ts/storage/database.svelte'

//#region module mocks

// happy-dom canvas getContext returns null
const fakeCtx = {
    drawImage: vi.fn(),
}
let canvasOutputMime = 'image/webp'
let canvasEncodeArgs: [string, number | undefined] | undefined
let loadedImageWidth = 100
let loadedImageHeight = 100
const origCreateElement = document.createElement.bind(document)
vi.spyOn(document, 'createElement').mockImplementation((tag: string, options?: any) => {
    const el = origCreateElement(tag, options)
    if (tag === 'canvas') {
        ;(el as HTMLCanvasElement).getContext = (() => fakeCtx) as any
        ;(el as HTMLCanvasElement).toBlob = ((cb: BlobCallback, mime?: string, quality?: number) => {
            canvasEncodeArgs = [mime ?? '', quality]
            cb(new Blob(['fake-image'], { type: canvasOutputMime }))
        }) as any
    }
    return el
})

const store = new Map<string, unknown>()
let corruptReadKey: string | undefined
let throwReadKey: string | undefined
let payloadReads = 0
let legacyReads = 0
let legacyKeyLists = 0

vi.mock('localforage', () => ({
    default: {
        createInstance: (options?: { name?: string }) => ({
            getItem: vi.fn(async (key: string) => {
                if (options?.name === 'inlay') legacyReads += 1
                if (key.startsWith('blobstore/inlays/')) payloadReads += 1
                if (key === throwReadKey) {
                    throwReadKey = undefined
                    throw new Error('verification read failed')
                }
                if (key === corruptReadKey) {
                    corruptReadKey = undefined
                    return new Uint8Array([9])
                }
                return store.get(key) ?? null
            }),
            setItem: vi.fn(async (key: string, value: unknown) => {
                store.set(key, value)
            }),
            removeItem: vi.fn(async (key: string) => {
                store.delete(key)
            }),
            keys: vi.fn(async () => {
                if (options?.name === 'inlay') legacyKeyLists += 1
                return [...store.keys()]
            }),
            iterate: vi.fn(async (cb: (value: unknown, key: string) => void) => {
                for (const [key, value] of store) {
                    cb(value, key)
                }
            }),
        }),
    },
}))

vi.mock('uuid', () => ({
    v4: vi.fn(() => 'test-uuid-1234'),
}))

vi.mock(import('src/ts/media'), () => ({
    getImageType: vi.fn(),
}))

vi.mock(import('src/ts/model/modellist'), () => ({
    getModelInfo: vi.fn(),
}))

vi.mock(import('src/ts/storage/database.svelte'), () => ({
    getDatabase: vi.fn(),
}))

vi.mock(
    import('src/ts/util'),
    () =>
        ({
            asBuffer: (arr: Uint8Array) => arr,
        }) as typeof import('src/ts/util'),
)

//#endregion

const supportedAudioExts = ['wav', 'mp3', 'ogg', 'flac'] as const
const supportedVideoExts = ['webm', 'mp4', 'mkv'] as const
const supportedImageExts = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'avif'] as const
const allSupportedExts = [...supportedAudioExts, ...supportedVideoExts, ...supportedImageExts]

function apngBytes(): Uint8Array {
    return Uint8Array.from([
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
        0, 0, 0, 8, 0x61, 0x63, 0x54, 0x4c,
        0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0,
    ])
}

function ftypBytes(majorBrand: string, compatibleBrands: string[] = []): Uint8Array {
    const bytes = new Uint8Array(16 + compatibleBrands.length * 4)
    new DataView(bytes.buffer).setUint32(0, bytes.byteLength)
    bytes.set(new TextEncoder().encode('ftyp'), 4)
    bytes.set(new TextEncoder().encode(majorBrand), 8)
    compatibleBrands.forEach((brand, index) => {
        bytes.set(new TextEncoder().encode(brand), 16 + index * 4)
    })
    return bytes
}

function makeImage(w: number, h: number): HTMLImageElement {
    const img = new Image()
    Object.defineProperty(img, 'width', { get: () => w })
    Object.defineProperty(img, 'height', { get: () => h })
    Object.defineProperty(img, 'naturalWidth', { get: () => w })
    Object.defineProperty(img, 'naturalHeight', { get: () => h })
    Object.defineProperty(img, 'currentSrc', {
        configurable: true,
        get: () => 'data:image/png;base64,iVBORw0KGgo=',
    })
    Object.defineProperty(img, 'onload', {
        set(fn: () => void) {
            fn?.()
        },
        get() {
            return null
        },
    })
    return img
}

beforeEach(() => {
    vi.clearAllMocks()
    store.clear()
    corruptReadKey = undefined
    throwReadKey = undefined
    payloadReads = 0
    legacyReads = 0
    legacyKeyLists = 0
    canvasOutputMime = 'image/webp'
    canvasEncodeArgs = undefined
    loadedImageWidth = 100
    loadedImageHeight = 100
    configureBlobStoreStorageProvider(async () => localforage.createInstance({ name: 'risunest' }))
    vi.stubGlobal('fetch', vi.fn(async () => new Response(
        Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
        { headers: { 'Content-Type': 'image/png' } },
    )))
    vi.stubGlobal('Image', class {
        naturalWidth = loadedImageWidth
        naturalHeight = loadedImageHeight
        width = loadedImageWidth
        height = loadedImageHeight
        onload: (() => void) | null = null
        onerror: (() => void) | null = null
        set src(_value: string) { queueMicrotask(() => this.onload?.()) }
    })
    vi.mocked(getDatabase).mockReturnValue({} as any)
})

describe('setInlayAsset', () => {
    test('normalizes absent and malformed configured options at the encoding boundary', () => {
        expect(getInlayEncodeOptions()).toEqual({
            format: 'webp', quality: 85, maxDimension: 0, skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0,
        })
        vi.mocked(getDatabase).mockReturnValue({
            risunestInlayFormat: 'invalid', risunestInlayWebpQuality: 140.6,
            risunestInlayMaxDimension: -4.4, risunestInlaySkipReencode: 'yes',
        } as any)
        expect(getInlayEncodeOptions()).toEqual({
            format: 'webp', quality: 100, maxDimension: 0, skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0,
        })
    })

    test.each([
        [Number.NaN, 0],
        [-1, 0],
        [12.6, 13],
        [4_294_967_296, 4_294_967_295],
        [Number.MAX_SAFE_INTEGER, 4_294_967_295],
    ])('normalizes maximum dimension %s before encoding', (input, expected) => {
        vi.mocked(getDatabase).mockReturnValue({ risunestInlayMaxDimension: input } as any)

        expect(getInlayEncodeOptions().maxDimension).toBe(expected)
    })

    test('encodes configured browser PNG with PNG metadata', async () => {
        canvasOutputMime = 'image/png'
        vi.mocked(getDatabase).mockReturnValue({
            risunestInlayFormat: 'png', risunestInlayWebpQuality: 17,
            risunestInlayMaxDimension: 0, risunestInlaySkipReencode: false,
        } as any)
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:configured-png')

        await setInlayAsset('configured-png', {
            data: new Blob([Uint8Array.of(1)], { type: 'image/png' }),
            ext: 'png', name: 'source.png', type: 'image',
        })

        const stored = await getInlayAssetBlob('configured-png')
        expect(stored).toMatchObject({ ext: 'png', height: 100, width: 100 })
        expect(stored!.data.type).toBe('image/png')
        expect(canvasEncodeArgs).toEqual(['image/png', 0.17])
    })

    test.each([
        ['png', Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a), 'image/png', 'png'],
        ['jpeg', Uint8Array.of(0xff, 0xd8, 0xff, 0xe0), 'image/jpeg', 'jpg'],
        ['webp', new TextEncoder().encode('RIFF\x04\0\0\0WEBPVP8 '), 'image/webp', 'webp'],
    ] as const)('preserves configured browser original %s bytes and canonical metadata', async (label, source, mime, ext) => {
        vi.mocked(getDatabase).mockReturnValue({ risunestInlayFormat: 'original' } as any)
        vi.stubGlobal('fetch', vi.fn(async () => new Response(source, { headers: { 'Content-Type': mime } })))
        vi.spyOn(URL, 'createObjectURL').mockReturnValue(`blob:original-${label}`)

        await setInlayAsset(`original-${label}`, {
            data: new Blob([source], { type: mime }), ext, name: `source.${ext}`, type: 'image',
        })

        const stored = await getInlayAssetBlob(`original-${label}`)
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(source)
        expect(stored).toMatchObject({ ext, height: 100, width: 100 })
        expect(stored!.data.type).toBe(mime)
        expect(canvasEncodeArgs).toBeUndefined()
    })

    test('forwards configured WebP quality to canvas encoding', async () => {
        vi.mocked(getDatabase).mockReturnValue({
            risunestInlayFormat: 'webp', risunestInlayWebpQuality: 42,
            risunestInlayMaxDimension: 0, risunestInlaySkipReencode: false,
        } as any)
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:quality-webp')

        await setInlayAsset('quality-webp', {
            data: new Blob([Uint8Array.of(1)], { type: 'image/png' }),
            ext: 'png', name: 'source.png', type: 'image',
        })

        expect(canvasEncodeArgs).toEqual(['image/webp', 0.42])
    })

    test('preserves static WebP bytes when skip re-encode is enabled without a resize', async () => {
        const source = new TextEncoder().encode('RIFF\x04\0\0\0WEBPVP8 ')
        vi.mocked(getDatabase).mockReturnValue({
            risunestInlayFormat: 'webp', risunestInlayWebpQuality: 85,
            risunestInlayMaxDimension: 0, risunestInlaySkipReencode: true,
        } as any)
        vi.stubGlobal('fetch', vi.fn(async () => new Response(source, {
            headers: { 'Content-Type': 'image/webp' },
        })))
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:source-webp')

        await setInlayAsset('source-webp', {
            data: new Blob([source], { type: 'image/webp' }),
            ext: 'webp', name: 'source.webp', type: 'image',
        })

        const stored = await getInlayAssetBlob('source-webp')
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(source)
        expect(stored).toMatchObject({ ext: 'webp', height: 100, width: 100 })
        expect(stored!.data.type).toBe('image/webp')
    })

    test('re-encodes skipped WebP when the configured long side requires resizing', async () => {
        const source = new TextEncoder().encode('RIFF\x04\0\0\0WEBPVP8 ')
        vi.mocked(getDatabase).mockReturnValue({
            risunestInlayFormat: 'webp', risunestInlayWebpQuality: 42,
            risunestInlayMaxDimension: 50, risunestInlaySkipReencode: true,
        } as any)
        vi.stubGlobal('fetch', vi.fn(async () => new Response(source, {
            headers: { 'Content-Type': 'image/webp' },
        })))
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:resized-webp')

        await setInlayAsset('resized-webp', {
            data: new Blob([source], { type: 'image/webp' }),
            ext: 'webp', name: 'source.webp', type: 'image',
        })

        const stored = await getInlayAssetBlob('resized-webp')
        expect(new Uint8Array(await stored!.data.arrayBuffer())).not.toEqual(source)
        expect(stored).toMatchObject({ ext: 'webp', height: 50, width: 50 })
        expect(fakeCtx.drawImage).toHaveBeenCalledWith(expect.anything(), 0, 0, 50, 50)
    })

    test('optimizes direct browser image writes at decoded natural dimensions', async () => {
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:direct-image')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        loadedImageWidth = 300
        loadedImageHeight = 150

        await setInlayAsset('direct-image', {
            data: new Blob([Uint8Array.of(1, 2, 3)], { type: 'image/png' }),
            ext: 'png', height: 1, width: 1, name: 'direct.png', type: 'image',
        })

        const stored = await getInlayAssetBlob('direct-image')
        expect(stored).toMatchObject({ ext: 'webp', height: 150, width: 300 })
        expect(stored!.data.type).toBe('image/webp')
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:direct-image')
    })

    test.each([
        ['GIF', Uint8Array.from([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]), 'gif', 'image/gif'],
        ['AVIF', ftypBytes('avif'), 'avif', 'image/avif'],
        ['animated WebP', new TextEncoder().encode('RIFF\x0c\0\0\0WEBPANIM\0\0\0\0'), 'webp', 'image/webp'],
        ['APNG', apngBytes(), 'png', 'image/png'],
    ])('stores new %s input as it arrived instead of flattening it', async (_label, bytes, ext, mime) => {
        await setInlayAsset('unsupported-image', {
            data: new Blob([bytes.slice().buffer as ArrayBuffer], { type: mime }),
            ext, name: `unsupported.${ext}`, type: 'image',
        })

        const stored = await getInlayAssetBlob('unsupported-image')
        expect(stored).toMatchObject({ ext, type: 'image' })
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(bytes)
        expect(fakeCtx.drawImage).not.toHaveBeenCalled()
    })

    test('refuses only an attachment past the input size limit', async () => {
        const oversized = new Uint8Array(maxNewInlayInputBytes + 1)

        await expect(setInlayAsset('too-large', {
            data: new Blob([oversized.buffer as ArrayBuffer], { type: 'image/png' }),
            ext: 'png', name: 'huge.png', type: 'image',
        })).rejects.toBeInstanceOf(InlayInputTooLargeError)

        expect(await getInlayAssetBlob('too-large')).toBeNull()
    })

    test('stores an asset in the storage', async () => {
        const asset: InlayAsset = {
            data: new Blob(['hello'], { type: 'text/plain' }),
            ext: 'png',
            height: 100,
            width: 100,
            name: 'test.png',
            type: 'image',
        }

        await setInlayAsset('asset-1', asset)

        expect(await getInlayAssetBlob('asset-1')).toMatchObject({
            ext: 'webp', height: 100, name: 'test.png', type: 'image', width: 100,
        })
    })

    test('overwrites an existing asset with the same id', async () => {
        const first: InlayAsset = {
            data: new Blob(['a']),
            ext: 'png',
            height: 10,
            name: 'first.png',
            type: 'image',
            width: 10,
        }
        const second: InlayAsset = {
            data: new Blob(['b']),
            ext: 'png',
            height: 20,
            name: 'second.png',
            type: 'image',
            width: 20,
        }

        loadedImageWidth = 10
        loadedImageHeight = 10
        await setInlayAsset('id-1', first)
        loadedImageWidth = 20
        loadedImageHeight = 20
        await setInlayAsset('id-1', second)

        expect(await getInlayAssetBlob('id-1')).toMatchObject({
            height: 20,
            name: 'second.png',
            type: 'image',
            width: 20,
        })
    })
})

describe('getInlayAsset', () => {
    test('returns null for a non-existent id', async () => {
        const result = await getInlayAsset('does-not-exist')
        expect(result).toBeNull()
    })

    test('returns asset with base64 data URI', async () => {
        loadedImageWidth = 50
        loadedImageHeight = 50
        await setInlayAsset('blob-id', {
            data: new Blob([new Uint8Array([1, 2])], { type: 'image/png' }),
            ext: 'png',
            height: 50,
            width: 50,
            name: 'blob-asset.png',
            type: 'image',
        })

        const result = await getInlayAsset('blob-id')

        expect(result!.data).toMatch(/^data:/)
        expect(result!.name).toBe('blob-asset.png')
    })
})

describe('getInlayAssetBlob', () => {
    test('returns null for a non-existent id', async () => {
        const result = await getInlayAssetBlob('does-not-exist')
        expect(result).toBeNull()
    })

    test('returns Blob data', async () => {
        loadedImageWidth = 64
        loadedImageHeight = 64
        await setInlayAsset('blob-id', {
            data: new Blob([new Uint8Array([1, 2])], { type: 'image/png' }),
            ext: 'png',
            height: 64,
            width: 64,
            name: 'blob.png',
            type: 'image',
        })

        const result = await getInlayAssetBlob('blob-id')
        expect(result!.data).toBeInstanceOf(Blob)
    })
})

describe('listInlayAssets', () => {
    test('returns empty array when no assets exist', async () => {
        const result = await listInlayAssets()
        expect(result).toEqual([])
    })

    test('returns all stored assets as [id, asset] tuples', async () => {
        const asset1: InlayAsset = {
            data: new Blob(['a']),
            ext: 'png',
            height: 10,
            width: 10,
            name: 'a.png',
            type: 'image',
        }
        const asset2: InlayAsset = {
            data: new Blob(['b']),
            ext: 'mp3',
            height: 0,
            width: 0,
            name: 'b.mp3',
            type: 'audio',
        }
        loadedImageWidth = 10
        loadedImageHeight = 10
        await setInlayAsset('id-a', asset1)
        await setInlayAsset('id-b', asset2)

        const result = await listInlayAssets()
        expect(result).toMatchObject([
            ['id-a', { name: 'a.png' }],
            ['id-b', { name: 'b.mp3' }],
        ])
    })
})

describe('native inlay rendering', () => {
    test('lists native metadata without reading or enumerating the legacy store', async () => {
        store.set('legacy-id', {
            data: new Blob(['legacy'], { type: 'image/webp' }),
            ext: 'webp',
            name: 'legacy.webp',
            type: 'image',
        } satisfies InlayAsset)
        store.set('blobstore/inlays/6e65772d6964.bin', new Uint8Array([1]))
        store.set('blobstore/metadata/6e65772d6964.json', new TextEncoder().encode(JSON.stringify({
            key: 'new-id', kind: 'inlay', size: 1, mime: 'image/png', name: 'new.png', ext: 'png',
            inlayType: 'image',
        })))

        await expect(listInlayAssetMetadata()).resolves.toMatchObject([{ key: 'new-id' }])
        expect(legacyReads).toBe(0)
        expect(legacyKeyLists).toBe(0)
        expect(payloadReads).toBe(0)
    })


    test('lists metadata without reading payload bytes', async () => {
        store.set('blobstore/inlays/69642d61.bin', new Uint8Array([1, 2, 3]))
        store.set('blobstore/metadata/69642d61.json', new TextEncoder().encode(JSON.stringify({
            key: 'id-a', kind: 'inlay', size: 3, mime: 'image/png', name: 'a.png', ext: 'png',
            inlayType: 'image', width: 2, height: 1,
        })))
        const result = await listInlayAssetMetadata()
        expect(result).toEqual([{
            key: 'id-a', kind: 'inlay', size: 3, mime: 'image/png', name: 'a.png', ext: 'png',
            inlayType: 'image', width: 2, height: 1,
        }])
        expect(payloadReads).toBe(0)
    })

    test('returns a render URL without reading the payload', async () => {
        store.set('blobstore/inlays/69642d61.bin', new Uint8Array([1, 2, 3]))
        store.set('blobstore/metadata/69642d61.json', new TextEncoder().encode(JSON.stringify({
            key: 'id-a', kind: 'inlay', size: 3, mime: 'image/png', name: 'a.png', ext: 'png', inlayType: 'image',
        })))
        const blobStore = createBackedBlobStore({
            write: async () => {},
            read: async (key) => {
                if (key.startsWith('blobstore/inlays/')) payloadReads += 1
                return store.get(key) as Uint8Array | null ?? null
            },
            keys: async () => [...store.keys()],
            remove: async () => {},
            resolveUrl: async (key) => `asset://${key}`,
        })
        await expect(getInlayAssetRenderUrl('id-a', blobStore)).resolves.toBe(
            'asset://blobstore/inlays/69642d61.bin',
        )
        expect(payloadReads).toBe(0)
    })
})

describe('removeInlayAsset', () => {
    test('does not throw when removing a non-existent id', async () => {
        await expect(removeInlayAsset('nope')).resolves.not.toThrow()
    })
})

describe('postInlayAsset', () => {
    test('keeps the original bytes and the URL contract when the source setter throws', async () => {
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:source-throw')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        vi.stubGlobal('Image', class {
            onload: (() => void) | null = null
            onerror: (() => void) | null = null
            set src(_value: string) { throw new Error('source assignment failed') }
        })

        await postInlayAsset({ name: 'broken.png', data: new Uint8Array([1]) })

        // The bytes survive a decoder that never got started.
        const stored = await getInlayAssetBlob('test-uuid-1234')
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(new Uint8Array([1]))
        expect(stored).toMatchObject({ ext: 'png' })
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:source-throw')
    })

    test('keeps the original bytes and the URL contract when image loading fails', async () => {
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:load-error')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        vi.stubGlobal('Image', class {
            onload: (() => void) | null = null
            onerror: (() => void) | null = null
            set src(_value: string) { queueMicrotask(() => this.onerror?.()) }
        })

        await postInlayAsset({ name: 'broken.png', data: new Uint8Array([1]) })

        const stored = await getInlayAssetBlob('test-uuid-1234')
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(new Uint8Array([1]))
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:load-error')
    })

    test('stores audio asset and returns id', async () => {
        const data = new Uint8Array([0xff, 0xfb, 0x90, 0x00])
        const result = await postInlayAsset({
            name: 'clip.mp3',
            data,
        })
        expect(result).toBe('test-uuid-1234')

        const stored = await getInlayAssetBlob('test-uuid-1234')
        expect(stored).toMatchObject({
            data: expect.any(Blob),
            ext: 'mp3',
            name: 'clip.mp3',
            type: 'audio',
        })
    })

    test('stores video asset and returns id', async () => {
        const data = new Uint8Array([0x1a, 0x45, 0xdf, 0xa3])
        const result = await postInlayAsset({
            name: 'video.webm',
            data,
        })
        expect(result).toBe('test-uuid-1234')

        const stored = await getInlayAssetBlob('test-uuid-1234')
        expect(stored).toMatchObject({
            data: expect.any(Blob),
            ext: 'webm',
            name: 'video.webm',
            type: 'video',
        })
    })

    test('returns null for any unsupported extension', async () => {
        await fc.assert(
            fc.asyncProperty(
                fc.string({ minLength: 1, maxLength: 10 }).filter((ext) => !allSupportedExts.includes(ext as any)),
                async (ext) => {
                    store.clear()
                    const result = await postInlayAsset({
                        name: `file.${ext}`,
                        data: new Uint8Array([0x00]),
                    })
                    expect(result).toBeNull()
                },
            ),
        )
    })

    test('routes audio extensions to audio type', async () => {
        await fc.assert(
            fc.asyncProperty(fc.constantFrom(...supportedAudioExts), async (ext) => {
                store.clear()
                const result = await postInlayAsset({
                    name: `sound.${ext}`,
                    data: new Uint8Array([0x00]),
                })
                expect(result).not.toBeNull()
                const stored = await getInlayAssetBlob(result!)
                expect(stored!.type).toBe('audio')
                expect(stored!.ext).toBe(ext)
            }),
        )
    })

    test('routes video extensions to video type', async () => {
        await fc.assert(
            fc.asyncProperty(fc.constantFrom(...supportedVideoExts), async (ext) => {
                store.clear()
                const result = await postInlayAsset({
                    name: `clip.${ext}`,
                    data: new Uint8Array([0x00]),
                })
                expect(result).not.toBeNull()
                const stored = await getInlayAssetBlob(result!)
                expect(stored!.type).toBe('video')
                expect(stored!.ext).toBe(ext)
            }),
        )
    })
})

describe('reencodeImage temporary object URLs', () => {
    test('revokes the temporary URL after a successful conversion', async () => {
        const createObjectURL = vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:success')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        vi.spyOn(HTMLCanvasElement.prototype, 'toDataURL').mockReturnValue('data:image/png;base64,AA==')
        vi.stubGlobal('Image', class {
            width = 1
            height = 1
            src = ''
            async decode() {}
        })

        await reencodeImage(new Uint8Array([1]))

        expect(createObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:success')
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
    })

    test('revokes the temporary URL when conversion fails', async () => {
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:failure')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        vi.stubGlobal('Image', class {
            src = ''
            async decode() { throw new Error('decode failed') }
        })

        await expect(reencodeImage(new Uint8Array([1]))).rejects.toThrow('decode failed')

        expect(revokeObjectURL).toHaveBeenCalledWith('blob:failure')
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
    })
})

describe('writeInlayImage', () => {
    test.each([
        ['GIF', Uint8Array.from([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]), 'image/gif'],
        ['AVIF', ftypBytes('avif'), 'image/avif'],
        ['animated WebP', new TextEncoder().encode('RIFF\x0c\0\0\0WEBPANIM\0\0\0\0'), 'image/webp'],
        ['APNG', apngBytes(), 'image/png'],
    ])('stores a direct %s source without drawing it to canvas', async (_label, bytes, mime) => {
        const source = `data:${mime};base64,fixture`
        vi.stubGlobal('fetch', vi.fn(async () => new Response(bytes.slice().buffer as ArrayBuffer, {
            headers: { 'Content-Type': mime },
        })))
        const image = makeImage(20, 10)
        Object.defineProperty(image, 'currentSrc', { get: () => source })

        await writeInlayImage(image, { id: 'direct-unsupported' })

        const stored = await getInlayAssetBlob('direct-unsupported')
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(bytes)
        expect(stored!.data.type).toBe(mime)
        expect(fakeCtx.drawImage).not.toHaveBeenCalled()
    })

    test.each([
        ['AVIS major brand', ftypBytes('avis')],
        ['AVIF compatible brand', ftypBytes('mif1', ['miaf', 'avif'])],
        ['AVIS compatible brand', ftypBytes('mif1', ['avis'])],
    ])('stores a direct %s source from its bytes, not its extension', async (_label, bytes) => {
        vi.stubGlobal('fetch', vi.fn(async () => new Response(bytes.slice().buffer as ArrayBuffer, {
            headers: { 'Content-Type': 'application/octet-stream' },
        })))

        await writeInlayImage(makeImage(20, 10), { id: 'direct-avif-brand' })

        const stored = await getInlayAssetBlob('direct-avif-brand')
        expect(stored).toMatchObject({ ext: 'avif' })
        expect(new Uint8Array(await stored!.data.arrayBuffer())).toEqual(bytes)
        expect(fakeCtx.drawImage).not.toHaveBeenCalled()
    })

    test('captures a production-style load event that fires during source validation', async () => {
        const image = {
            complete: false,
            currentSrc: 'data:image/png;base64,iVBORw0KGgo=',
            src: 'data:image/png;base64,iVBORw0KGgo=',
            naturalWidth: 64,
            naturalHeight: 32,
            width: 64,
            height: 32,
            onload: null as (() => void) | null,
            onerror: null as (() => void) | null,
        } as unknown as HTMLImageElement
        vi.stubGlobal('fetch', vi.fn(async () => {
            ;(image as unknown as { complete: boolean }).complete = true
            image.onload?.(new Event('load'))
            return new Response(
                Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).buffer,
                { headers: { 'Content-Type': 'image/png' } },
            )
        }))

        const result = await Promise.race([
            writeInlayImage(image, { id: 'fast-load' }),
            new Promise<string>((resolve) => setTimeout(() => resolve('timed-out'), 25)),
        ])

        expect(result).toBe('fast-load')
        expect(await getInlayAssetBlob('fast-load')).toMatchObject({ width: 64, height: 32 })
    })

    test('stores image asset with correct metadata and returns id', async () => {
        const imgObj = makeImage(200, 100)

        const result = await writeInlayImage(imgObj, {
            name: 'photo.jpg',
            ext: 'jpg',
            id: 'custom-id',
        })

        expect(result).toBe('custom-id')

        const stored = await getInlayAssetBlob('custom-id')
        expect(stored).toMatchObject({
            data: expect.any(Blob),
            ext: 'webp',
            height: 100,
            name: 'photo.jpg',
            type: 'image',
            width: 200,
        })
        expect(stored!.data.type).toBe('image/webp')
    })

    test('maps an asset namespace id to a stable Inlay id without overwriting the source asset', async () => {
        const sourceAssetKey = 'assets/character-image.png'
        store.set(sourceAssetKey, new Uint8Array([9, 8, 7]))

        const first = await writeInlayImage(makeImage(200, 100), {
            name: sourceAssetKey,
            ext: 'png',
            id: sourceAssetKey,
        })
        const second = await writeInlayImage(makeImage(200, 100), {
            name: sourceAssetKey,
            ext: 'png',
            id: sourceAssetKey,
        })

        expect(first).toBe('character-image.png')
        expect(second).toBe(first)
        expect(store.get(sourceAssetKey)).toEqual(new Uint8Array([9, 8, 7]))
        expect(await getInlayAssetBlob(first)).toMatchObject({
            name: sourceAssetKey,
            type: 'image',
        })
    })

    test('generates uuid when no id is provided', async () => {
        const imgObj = makeImage(50, 50)

        const result = await writeInlayImage(imgObj)
        expect(result).toBe('test-uuid-1234')

        const stored = await getInlayAssetBlob('test-uuid-1234')
        expect(stored!.name).toBe('test-uuid-1234')
    })

    test('preserves natural dimensions above the former pixel cap', async () => {
        await writeInlayImage(makeImage(4096, 2048), { id: 'full-size' })

        expect(await getInlayAssetBlob('full-size')).toMatchObject({
            height: 2048,
            width: 4096,
        })
    })

    test('uses the actual browser encoder MIME and extension when WebP is unavailable', async () => {
        canvasOutputMime = 'image/png'

        await writeInlayImage(makeImage(80, 40), { id: 'png-fallback' })

        const stored = await getInlayAssetBlob('png-fallback')
        expect(stored).toMatchObject({ ext: 'png', height: 40, width: 80 })
        expect(stored!.data.type).toBe('image/png')
    })
})

describe('set -> get round-trip', () => {
    test('returns the mapped identity when setInlayAsset receives an asset namespace id', async () => {
        const id = await setInlayAsset('assets/persona.png', {
            data: new Blob(['data'], { type: 'audio/mpeg' }),
            ext: 'mp3',
            name: 'persona.mp3',
            type: 'audio',
        })

        expect(id).toBe('persona.png')
        expect(await getInlayAssetBlob(id)).toMatchObject({ name: 'persona.mp3', type: 'audio' })
    })

    test('preserves metadata through setInlayAsset -> getInlayAsset', async () => {
        await fc.assert(
            fc.asyncProperty(
                fc.string({ minLength: 1, maxLength: 20 }),
                fc.string({ minLength: 1, maxLength: 30 }),
                fc.string({ minLength: 1, maxLength: 5 }),
                fc.nat({ max: 5000 }),
                fc.nat({ max: 5000 }),
                async (id, name, ext, width, height) => {
                    store.clear()
                    loadedImageWidth = width
                    loadedImageHeight = height
                    const blob = new Blob(['data'], { type: 'application/octet-stream' })
                    const asset: InlayAsset = {
                        data: blob,
                        ext,
                        height,
                        width,
                        name,
                        type: 'image',
                    }

                    await setInlayAsset(id, asset)

                    const result = await getInlayAsset(id)
                    expect(result).toMatchObject({
                        data: expect.any(String),
                        ext: 'webp',
                        height,
                        width,
                        name,
                        type: 'image',
                    })
                },
            ),
        )
    })
})

describe('set -> remove -> get', () => {
    test('asset is always null after removal', async () => {
        await fc.assert(
            fc.asyncProperty(fc.string({ minLength: 1, maxLength: 20 }), async (id) => {
                store.clear()
                loadedImageWidth = 1
                loadedImageHeight = 1
                const asset: InlayAsset = {
                    data: new Blob(['x']),
                    ext: 'png',
                    height: 1,
                    width: 1,
                    name: 'tmp.png',
                    type: 'image',
                }

                await setInlayAsset(id, asset)
                expect(await getInlayAsset(id)).not.toBeNull()

                await removeInlayAsset(id)
                expect(await getInlayAsset(id)).toBeNull()
            }),
        )
    })
})

describe('BlobStore inlay compatibility', () => {
    test('optimizes new images and round trips audio, video, and signature bytes', async () => {
        const fixtures: [string, InlayAsset, Uint8Array][] = [
            ['image', { data: new Blob([new Uint8Array([1, 2])], { type: 'image/png' }), ext: 'png', name: 'a.png', type: 'image', width: 2, height: 1 }, new Uint8Array([1, 2])],
            ['audio', { data: new Blob([new Uint8Array([3])], { type: 'audio/mpeg' }), ext: 'mp3', name: 'a.mp3', type: 'audio' }, new Uint8Array([3])],
            ['video', { data: new Blob([new Uint8Array([4, 5])], { type: 'video/webm' }), ext: 'webm', name: 'a.webm', type: 'video' }, new Uint8Array([4, 5])],
        ]
        for (const [id, asset, bytes] of fixtures) {
            if (asset.type === 'image') {
                loadedImageWidth = asset.width!
                loadedImageHeight = asset.height!
            }
            await setInlayAsset(id, asset)
            const loaded = await getInlayAssetBlob(id)
            if (asset.type === 'image') {
                expect(loaded).toMatchObject({ name: asset.name, ext: 'webp', type: 'image' })
            } else {
                expect(new Uint8Array(await loaded!.data.arrayBuffer())).toEqual(bytes)
                expect(loaded).toMatchObject({ name: asset.name, ext: asset.ext, type: asset.type })
            }
        }

        const signature = { signatures: [{ type: 'text' as const, content: 'synthetic' }], sourceFormat: 0 as any, source: 'local' }
        await saveInlayedSignature('signature', signature)
        expect((await getInlayAsset('signature'))?.data).toBe(JSON.stringify(signature))
    })
})

test('uses the currently selected performance profile for animation admission', () => {
    try {
        setRuntimePerformanceProfile('low-spec')
        expect(getInlayEncodeOptions().animationDecodeBytes).toBe(64 * 1024 * 1024)
        setRuntimePerformanceProfile('normal')
        expect(getInlayEncodeOptions().animationDecodeBytes).toBe(256 * 1024 * 1024)
    } finally {
        setRuntimePerformanceProfile('normal')
    }
})
