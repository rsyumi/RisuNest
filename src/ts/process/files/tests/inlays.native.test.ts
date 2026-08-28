import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { BlobMetadata, BlobStore, InlayBlobMetadata } from 'src/ts/storage/blobStore'

const legacy = vi.hoisted(() => ({
    reads: 0,
    lists: 0,
    removes: 0,
    values: new Map<string, unknown>(),
}))

const native = vi.hoisted(() => ({
    metadata: new Map<string, BlobMetadata>(),
    payloads: new Map<string, Uint8Array>(),
    optimizedWrites: [] as Array<{ key: string, data: Uint8Array, name: string }>,
    opaqueWrites: [] as string[],
    removes: [] as string[],
}))

const nativeStore = {
    async put(key, data, input) {
        native.opaqueWrites.push(key)
        const metadata = { ...input, key, size: data.byteLength } as BlobMetadata
        native.metadata.set(key, metadata)
        native.payloads.set(key, data)
        return metadata
    },
    async putNewInlayImage(key: string, data: Uint8Array, input: { name: string }) {
        const png = data.length >= 8 && data[0] === 0x89 && data[1] === 0x50 && data[2] === 0x4e && data[3] === 0x47
        const jpeg = data.length >= 3 && data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff
        const webp = data.length >= 12 && new TextDecoder().decode(data.subarray(0, 4)) === 'RIFF'
            && new TextDecoder().decode(data.subarray(8, 12)) === 'WEBP'
        if (!png && !jpeg && !webp) throw new Error('unsupported new Inlay image format: Bmp')
        native.optimizedWrites.push({ key, data: data.slice(), name: input.name })
        const metadata: InlayBlobMetadata = {
            key, kind: 'inlay', size: 3, mime: 'image/webp', name: input.name,
            ext: 'webp', inlayType: 'image', width: 6, height: 4,
        }
        native.metadata.set(key, metadata)
        native.payloads.set(key, Uint8Array.of(8, 5, 0))
        return metadata
    },
    async read(key) {
        return native.payloads.get(key) ?? null
    },
    async stat(key) {
        return native.metadata.get(key) ?? null
    },
    async list(query) {
        return [...native.metadata.values()].filter((item) => !query?.kind || item.kind === query.kind)
    },
    async remove(key) {
        native.removes.push(key)
        native.metadata.delete(key)
        native.payloads.delete(key)
    },
    async resolveUrl(key) {
        return native.metadata.has(key) ? `http://asset.local/${key}` : null
    },
} satisfies BlobStore & {
    putNewInlayImage(key: string, data: Uint8Array, input: { name: string }): Promise<InlayBlobMetadata>
}

vi.mock('src/ts/platform', () => ({ isTauri: true }))
vi.mock('src/ts/storage/platformBlobStore', () => ({ resolveBlobStore: vi.fn(async () => nativeStore) }))
vi.mock('localforage', () => ({
    default: {
        createInstance: () => ({
            getItem: vi.fn(async (key: string) => {
                legacy.reads += 1
                return legacy.values.get(key) ?? null
            }),
            setItem: vi.fn(async (key: string, value: unknown) => legacy.values.set(key, value)),
            removeItem: vi.fn(async (key: string) => {
                legacy.removes += 1
                legacy.values.delete(key)
            }),
            keys: vi.fn(async () => {
                legacy.lists += 1
                return [...legacy.values.keys()]
            }),
        }),
    },
}))
vi.mock('uuid', () => ({ v4: vi.fn(() => 'native-test-id') }))
vi.mock('src/ts/media', () => ({ getImageType: vi.fn() }))
vi.mock('src/ts/model/modellist', () => ({ getModelInfo: vi.fn() }))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: vi.fn() }))
vi.mock('src/ts/util', () => ({ asBuffer: (value: Uint8Array) => value }))

import {
    getInlayAsset,
    getInlayAssetBlob,
    getInlayAssetMetadata,
    listInlayAssets,
    listInlayAssetMetadata,
    migrateLegacyInlayAsset,
    postInlayAsset,
    removeInlayAsset,
    setInlayAsset,
    writeInlayImage,
} from '../inlays'

function seedNativeInlay(key: string, bytes: Uint8Array, input: Omit<InlayBlobMetadata, 'key' | 'size'>): void {
    native.metadata.set(key, { ...input, key, size: bytes.byteLength })
    native.payloads.set(key, bytes)
}

function seedLegacyInlay(key: string): void {
    legacy.values.set(key, {
        data: new Blob(['legacy'], { type: 'image/png' }),
        ext: 'png',
        name: 'legacy.png',
        type: 'image',
    })
}

function expectNoLegacyAccess(): void {
    expect(legacy.reads).toBe(0)
    expect(legacy.lists).toBe(0)
    expect(legacy.removes).toBe(0)
}

describe('native inlay fresh-install boundary', () => {
    beforeEach(() => {
        native.metadata.clear()
        native.payloads.clear()
        native.optimizedWrites = []
        native.opaqueWrites = []
        native.removes = []
        legacy.values.clear()
        legacy.reads = 0
        legacy.lists = 0
        legacy.removes = 0
    })

    test('migration and ID-scoped metadata lookup never inspect legacy storage', async () => {
        seedLegacyInlay('legacy-id')

        await expect(migrateLegacyInlayAsset('legacy-id')).resolves.toBeNull()
        await expect(getInlayAssetMetadata('legacy-id')).resolves.toBeNull()

        expectNoLegacyAccess()
    })

    test('plugin, model, and script readers use only native BlobStore data', async () => {
        seedLegacyInlay('native-audio')
        seedNativeInlay('native-audio', new Uint8Array([1, 2, 3]), {
            kind: 'inlay', mime: 'audio/ogg', name: 'voice.ogg', ext: 'ogg', inlayType: 'audio',
        })

        await expect(getInlayAsset('native-audio')).resolves.toMatchObject({
            data: 'data:audio/ogg;base64,AQID', type: 'audio',
        })
        await expect(getInlayAssetBlob('native-audio')).resolves.toMatchObject({ type: 'audio' })

        expectNoLegacyAccess()
    })

    test('default full and metadata listings omit legacy entries without inspecting them', async () => {
        seedLegacyInlay('legacy-id')
        seedNativeInlay('native-image', new Uint8Array([4, 5]), {
            kind: 'inlay', mime: 'image/png', name: 'native.png', ext: 'png', inlayType: 'image',
        })

        await expect(listInlayAssets()).resolves.toMatchObject([['native-image', { name: 'native.png' }]])
        await expect(listInlayAssetMetadata()).resolves.toMatchObject([{ key: 'native-image' }])

        expectNoLegacyAccess()
    })

    test('removal leaves legacy storage untouched', async () => {
        seedLegacyInlay('native-id')
        seedNativeInlay('native-id', new Uint8Array([1]), {
            kind: 'inlay', mime: 'image/png', name: 'native.png', ext: 'png', inlayType: 'image',
        })

        await removeInlayAsset('native-id')

        expect(native.removes).toEqual(['native-id'])
        expect(legacy.values.has('native-id')).toBe(true)
        expectNoLegacyAccess()
    })

    test.each([
        ['png', 'image/png'],
        ['jpg', 'image/jpeg'],
        ['gif', 'image/gif'],
        ['webp', 'image/webp'],
        ['avif', 'image/avif'],
    ])('reads existing %s image Inlays without conversion', async (ext, mime) => {
        const bytes = Uint8Array.of(1, 9, 3, 7)
        seedNativeInlay(`legacy-${ext}`, bytes, {
            kind: 'inlay', mime, name: `legacy.${ext}`, ext, inlayType: 'image', width: 2, height: 2,
        })

        const asset = await getInlayAssetBlob(`legacy-${ext}`)

        expect(new Uint8Array(await asset!.data.arrayBuffer())).toEqual(bytes)
        expect(asset).toMatchObject({ ext, name: `legacy.${ext}`, type: 'image' })
        expect(native.optimizedWrites).toEqual([])
        expect(native.opaqueWrites).toEqual([])
    })

    test('new image writes use the native optimizer without an opaque BlobStore put', async () => {
        const source = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)

        await setInlayAsset('new-image', {
            data: new Blob([source], { type: 'image/png' }),
            ext: 'png', name: 'source.png', type: 'image', width: 6, height: 4,
        })

        expect(native.optimizedWrites).toEqual([{
            key: 'new-image', data: source, name: 'source.png',
        }])
        expect(native.opaqueWrites).toEqual([])
        await expect(getInlayAssetMetadata('new-image')).resolves.toMatchObject({
            mime: 'image/webp', ext: 'webp', width: 6, height: 4,
        })
    })

    test('native file image posts send original source bytes directly to the optimizer', async () => {
        const source = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
        const createObjectURL = vi.spyOn(URL, 'createObjectURL')

        await expect(postInlayAsset({ name: 'upload.png', data: source })).resolves.toBe('native-test-id')

        expect(native.optimizedWrites).toEqual([{
            key: 'native-test-id', data: source, name: 'upload.png',
        }])
        expect(createObjectURL).not.toHaveBeenCalled()
    })

    test('native generated images fetch their source bytes without a canvas round trip', async () => {
        const source = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
        const fetchSource = vi.fn(async () => new Response(source, {
            headers: { 'Content-Type': 'image/png' },
        }))
        vi.stubGlobal('fetch', fetchSource)
        const image = {
            src: 'data:image/png;base64,BQQDAg==',
            currentSrc: '',
        } as HTMLImageElement

        await expect(writeInlayImage(image, { id: 'generated', name: 'generated.png' }))
            .resolves.toBe('generated')

        expect(fetchSource).toHaveBeenCalledWith(image.src)
        expect(native.optimizedWrites).toEqual([{
            key: 'generated', data: source, name: 'generated.png',
        }])
    })

    test('browser-decodable BMP images fall back to a WebP BlobStore write', async () => {
        const bmp = Uint8Array.of(0x42, 0x4d, 0, 0)
        const fetchSource = vi.fn(async () => new Response(bmp, {
            headers: { 'Content-Type': 'image/bmp' },
        }))
        const drawImage = vi.fn()
        const toBlob = vi.fn((callback: BlobCallback) => callback(new Blob([Uint8Array.of(8, 5, 0)], { type: 'image/webp' })))
        vi.stubGlobal('fetch', fetchSource)
        vi.stubGlobal('Image', class {
            complete = false
            naturalHeight = 4
            naturalWidth = 6
            height = 4
            width = 6
            onerror: (() => void) | null = null
            onload: (() => void) | null = null
            set src(_value: string) { this.onload?.() }
        })
        vi.stubGlobal('document', {
            createElement: vi.fn(() => ({
                getContext: vi.fn(() => ({ drawImage })),
                height: 0,
                toBlob,
                width: 0,
            })),
        })
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:native-bmp')
        vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => undefined)

        await setInlayAsset('bmp-image', {
            data: new Blob([bmp], { type: 'image/bmp' }),
            ext: 'bmp', name: 'source.bmp', type: 'image',
        })

        expect(native.optimizedWrites).toEqual([])
        expect(native.opaqueWrites).toEqual(['bmp-image'])
        expect(drawImage).toHaveBeenCalledWith(expect.anything(), 0, 0, 6, 4)
        expect(toBlob).toHaveBeenCalledWith(expect.any(Function), 'image/webp', 0.85)
        await expect(getInlayAssetMetadata('bmp-image')).resolves.toMatchObject({
            mime: 'image/webp', ext: 'webp', width: 6, height: 4,
        })
    })
})
