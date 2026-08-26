import { describe, expect, it, vi } from 'vitest'

import {
    createNativeImmutablePayloadCas,
    createNativeNewInlayImageEncoder,
} from './nativeAssetRepository'

describe('native asset repository adapters', () => {
    it('uses native CAS commands for exact writes and bounded reads', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_prepare') return {
                contentHash: '11'.repeat(32),
                byteSize: 3,
                physicalKey: `assets-v2/objects/${'11'.repeat(32).slice(0, 2)}/${'11'.repeat(32).slice(2)}`,
                deduplicated: false,
                directoryEntriesSynced: true,
            }
            if (command === 'asset_cas_read_object') return [1, 2, 3]
            if (command === 'asset_cas_read_object_range') return [2]
            if (command === 'asset_cas_stat_object') return 3
            throw new Error(`Unexpected command ${command}`)
        })
        const cas = createNativeImmutablePayloadCas(invoke)

        await expect(cas.prepare(Uint8Array.of(1, 2, 3))).resolves.toMatchObject({ byteSize: 3 })
        await expect(cas.readObject('11'.repeat(32))).resolves.toEqual(Uint8Array.of(1, 2, 3))
        await expect(cas.readObjectRange('11'.repeat(32), {
            start: 1,
            endExclusive: 2,
        })).resolves.toEqual(Uint8Array.of(2))
        await expect(cas.statObject('11'.repeat(32))).resolves.toBe(3)
        expect(invoke).toHaveBeenCalledWith('asset_cas_read_object_range', {
            contentHash: '11'.repeat(32),
            start: 1,
            endExclusive: 2,
        })
    })

    it('accepts only one unchanged-dimension WebP encoding result', async () => {
        const invoke = vi.fn(async () => ({
            data: [4, 5, 6],
            metadata: {
                key: 'inlay-id',
                kind: 'inlay',
                size: 3,
                mime: 'image/webp',
                name: 'Image',
                ext: 'webp',
                inlayType: 'image',
                width: 13,
                height: 17,
            },
        }))
        const encoder = createNativeNewInlayImageEncoder(invoke)

        await expect(encoder.encodeNewInlayImage(
            'inlay-id',
            Uint8Array.of(1, 2),
            { name: 'Image' },
        )).resolves.toEqual({
            data: Uint8Array.of(4, 5, 6),
            metadata: {
                kind: 'inlay',
                mime: 'image/webp',
                name: 'Image',
                ext: 'webp',
                inlayType: 'image',
                width: 13,
                height: 17,
            },
        })
        expect(invoke).toHaveBeenCalledOnce()
    })
})
