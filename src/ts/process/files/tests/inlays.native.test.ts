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
    removes: [] as string[],
}))

const nativeStore: BlobStore = {
    async put(key, data, input) {
        const metadata = { ...input, key, size: data.byteLength } as BlobMetadata
        native.metadata.set(key, metadata)
        native.payloads.set(key, data)
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
    removeInlayAsset,
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
})
