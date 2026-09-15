import fc from 'fast-check'
import { describe, expect, test } from 'vitest'
import { summarizeInlayAssets } from '../inlayInventory'
import type { InlayBlobMetadata, InlayBlobType } from 'src/ts/storage/blobStore'

let nextKey = 0

function asset(ext: string, size: number, inlayType: InlayBlobType = 'image'): InlayBlobMetadata {
    nextKey += 1
    return {
        key: `inlay-${nextKey}`,
        kind: 'inlay',
        size,
        mime: 'application/octet-stream',
        name: `inlay-${nextKey}.${ext}`,
        ext,
        inlayType,
    }
}

describe('summarizeInlayAssets', () => {
    test('returns empty totals for an empty store', () => {
        const inventory = summarizeInlayAssets([])

        expect(inventory.total).toEqual({ count: 0, bytes: 0 })
        expect(inventory.images).toEqual([])
        expect(inventory.others).toEqual([])
        expect(inventory.byType).toEqual({
            image: { count: 0, bytes: 0 },
            video: { count: 0, bytes: 0 },
            audio: { count: 0, bytes: 0 },
            signature: { count: 0, bytes: 0 },
        })
    })

    test('orders image extensions by count, breaking ties by extension', () => {
        const inventory = summarizeInlayAssets([
            asset('png', 10), asset('webp', 1), asset('webp', 2), asset('webp', 3), asset('gif', 20),
        ])

        expect(inventory.images).toEqual([
            { ext: 'webp', count: 3, bytes: 6 },
            { ext: 'gif', count: 1, bytes: 20 },
            { ext: 'png', count: 1, bytes: 10 },
        ])
        expect(inventory.total).toEqual({ count: 5, bytes: 36 })
    })

    test('keeps jpg and jpeg apart so the stored state stays visible', () => {
        const inventory = summarizeInlayAssets([asset('jpg', 5), asset('jpeg', 7)])

        expect(inventory.images.map((entry) => entry.ext).sort()).toEqual(['jpeg', 'jpg'])
    })

    test('normalizes case and leading dots', () => {
        const inventory = summarizeInlayAssets([asset('PNG', 1), asset('.png', 2), asset('png', 4)])

        expect(inventory.images).toEqual([{ ext: 'png', count: 3, bytes: 7 }])
    })

    test('counts assets without an extension under an empty key', () => {
        const inventory = summarizeInlayAssets([asset('', 3), asset('', 4), asset('webp', 1)])

        expect(inventory.images).toEqual([
            { ext: '', count: 2, bytes: 7 },
            { ext: 'webp', count: 1, bytes: 1 },
        ])
    })

    test('separates images from audio, video, and signature rows', () => {
        const inventory = summarizeInlayAssets([
            asset('webp', 1), asset('mp3', 2, 'audio'), asset('mp4', 4, 'video'), asset('json', 8, 'signature'),
        ])

        expect(inventory.images).toEqual([{ ext: 'webp', count: 1, bytes: 1 }])
        expect(inventory.others).toEqual([
            { ext: 'json', count: 1, bytes: 8 },
            { ext: 'mp3', count: 1, bytes: 2 },
            { ext: 'mp4', count: 1, bytes: 4 },
        ])
        expect(inventory.byType.audio).toEqual({ count: 1, bytes: 2 })
        expect(inventory.byType.signature).toEqual({ count: 1, bytes: 8 })
    })

    test('preserves counts and bytes across every grouping', () => {
        const types: InlayBlobType[] = ['image', 'video', 'audio', 'signature']
        fc.assert(fc.property(
            fc.array(fc.record({
                ext: fc.constantFrom('png', 'PNG', '.webp', 'gif', 'mp3', 'json', ''),
                size: fc.nat({ max: 1_000_000 }),
                inlayType: fc.constantFrom(...types),
            })),
            (inputs) => {
                const inventory = summarizeInlayAssets(
                    inputs.map((input) => asset(input.ext, input.size, input.inlayType)),
                )
                const rows = [...inventory.images, ...inventory.others]
                const sum = (values: { count: number, bytes: number }[]) => ({
                    count: values.reduce((carry, value) => carry + value.count, 0),
                    bytes: values.reduce((carry, value) => carry + value.bytes, 0),
                })

                expect(inventory.total.count).toBe(inputs.length)
                expect(sum(rows)).toEqual(inventory.total)
                expect(sum(Object.values(inventory.byType))).toEqual(inventory.total)
                for (const list of [inventory.images, inventory.others]) {
                    expect(new Set(list.map((row) => row.ext)).size).toBe(list.length)
                }
            },
        ))
    })
})
