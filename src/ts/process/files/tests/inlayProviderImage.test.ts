// @vitest-environment happy-dom

import { beforeEach, describe, expect, test, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    getDatabase: vi.fn(() => ({ risunestInlayAnimationStillFrame: true, risunestInlayWebpQuality: 85 })),
}))
vi.mock('src/ts/storage/database.svelte', () => mocks)
vi.mock('src/ts/util', () => ({ asBuffer: (bytes: Uint8Array) => bytes }))

import { forgetInlayProviderImages, inlayImageForProvider } from '../inlayProviderImage'
import * as performanceProfile from '../../../runtimePerformanceProfile'

const drawImage = vi.fn()
let createBitmap = vi.fn()
let encodedBytes = Uint8Array.of(9, 9, 9)

// happy-dom has no canvas encoder, so the canvas is faked once for the whole file.
const originalCreateElement = document.createElement.bind(document)
vi.spyOn(document, 'createElement').mockImplementation((tag: string, options?: any) => {
    const element = originalCreateElement(tag, options)
    if (tag === 'canvas') {
        ;(element as HTMLCanvasElement).getContext = (() => ({ drawImage })) as any
        ;(element as HTMLCanvasElement).toBlob = ((callback: BlobCallback) => {
            callback(new Blob([encodedBytes.slice().buffer as ArrayBuffer], { type: 'image/webp' }))
        }) as any
    }
    return element
})

function dataUri(mime: string, bytes: Uint8Array): string {
    let binary = ''
    for (const byte of bytes) binary += String.fromCharCode(byte)
    return `data:${mime};base64,${btoa(binary)}`
}

const gifBytes = Uint8Array.from([0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 1, 2, 3])
const pngBytes = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 7])
const avifBytes = (() => {
    const bytes = new Uint8Array(16)
    new DataView(bytes.buffer).setUint32(0, bytes.byteLength)
    bytes.set(new TextEncoder().encode('ftyp'), 4)
    bytes.set(new TextEncoder().encode('avif'), 8)
    return bytes
})()

describe('inlayImageForProvider', () => {
    beforeEach(() => {
        forgetInlayProviderImages()
        vi.clearAllMocks()
        mocks.getDatabase.mockReturnValue({ risunestInlayAnimationStillFrame: true, risunestInlayWebpQuality: 85 })
        encodedBytes = Uint8Array.of(9, 9, 9)
        createBitmap = vi.fn(async () => ({ width: 40, height: 20, close: vi.fn() }))
        vi.stubGlobal('createImageBitmap', createBitmap)
    })

    test('evicts by bytes in access order and returns oversized images without retaining them', async () => {
        const budget = vi.spyOn(performanceProfile, 'getRuntimePerformanceBudgets').mockReturnValue({
            ...performanceProfile.getRuntimePerformanceBudgets(), providerImageCacheBytes: 180,
        })
        try {
            forgetInlayProviderImages()
            const asset = { data: dataUri('image/gif', gifBytes) }
            await inlayImageForProvider('a', asset)
            await inlayImageForProvider('b', asset)
            await inlayImageForProvider('a', asset)
            await inlayImageForProvider('c', asset)
            expect(createBitmap).toHaveBeenCalledTimes(3)
            await inlayImageForProvider('b', asset)
            expect(createBitmap).toHaveBeenCalledTimes(4)
            encodedBytes = new Uint8Array(100)
            const first = await inlayImageForProvider('large', asset)
            const second = await inlayImageForProvider('large', asset)
            expect(first.data).toBe(dataUri('image/webp', encodedBytes))
            expect(second).toEqual(first)
            expect(createBitmap).toHaveBeenCalledTimes(6)
        } finally {
            budget.mockRestore()
            forgetInlayProviderImages()
        }
    })

    test('an in-flight conversion cannot repopulate an invalidated cache', async () => {
        let resume!: (value: { width: number, height: number, close: () => void }) => void
        createBitmap.mockImplementationOnce(() => new Promise((resolve) => { resume = resolve }))
        const asset = { data: dataUri('image/gif', gifBytes) }
        const pending = inlayImageForProvider('a', asset)
        await vi.waitFor(() => expect(createBitmap).toHaveBeenCalledTimes(1))
        forgetInlayProviderImages()
        resume({ width: 40, height: 20, close: vi.fn() })
        await pending
        await inlayImageForProvider('a', asset)
        expect(createBitmap).toHaveBeenCalledTimes(2)
    })

    test('sends an image every provider reads without touching it', async () => {
        const asset = { data: dataUri('image/png', pngBytes), width: 4, height: 2 }

        await expect(inlayImageForProvider('png-inlay', asset)).resolves.toBe(asset)

        expect(createBitmap).not.toHaveBeenCalled()
    })

    test('sends the first scene of an animation and reuses it next time', async () => {
        const asset = { data: dataUri('image/gif', gifBytes) }

        const first = await inlayImageForProvider('gif-inlay', asset)
        const second = await inlayImageForProvider('gif-inlay', asset)

        expect(first.data).toBe(dataUri('image/webp', encodedBytes))
        expect(first).toMatchObject({ width: 40, height: 20 })
        expect(second).toEqual(first)
        expect(createBitmap).toHaveBeenCalledOnce()
        expect(drawImage).toHaveBeenCalledWith(expect.anything(), 0, 0, 40, 20)
    })

    test('converts a still format no provider lists', async () => {
        const converted = await inlayImageForProvider('avif-inlay', { data: dataUri('image/avif', avifBytes) })

        expect(converted.data).toBe(dataUri('image/webp', encodedBytes))
        expect(createBitmap).toHaveBeenCalledOnce()
    })

    test('sends the stored file itself when the setting is off', async () => {
        mocks.getDatabase.mockReturnValue({ risunestInlayAnimationStillFrame: false, risunestInlayWebpQuality: 85 })
        const asset = { data: dataUri('image/gif', gifBytes) }

        await expect(inlayImageForProvider('gif-inlay', asset)).resolves.toBe(asset)

        expect(createBitmap).not.toHaveBeenCalled()
    })

    test('falls back to the stored file when it cannot be decoded', async () => {
        createBitmap.mockRejectedValue(new Error('cannot decode'))
        const asset = { data: dataUri('image/gif', gifBytes) }

        await expect(inlayImageForProvider('gif-inlay', asset)).resolves.toBe(asset)
    })
})
