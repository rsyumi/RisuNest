import { describe, expect, it, vi } from 'vitest'

import {
    beginCasJob,
    createNativeImmutablePayloadCas,
    createNativeNewInlayImageEncoder,
    pinExistingCasObject,
    prepareCasObject,
    releaseCasJob,
    sealCasJob,
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

    it('exposes a native-only durable CAS pin session without catalog enumeration', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_job_begin') return 'session-1'
            if (command === 'asset_cas_job_prepare') return {
                contentHash: '44'.repeat(32),
                byteSize: 3,
                physicalKey: `assets-v2/objects/44/${'44'.repeat(32).slice(2)}`,
                deduplicated: false,
                directoryEntriesSynced: false,
            }
            return null
        })

        await expect(beginCasJob('lossless-import', invoke)).resolves.toBe('session-1')
        await expect(prepareCasObject(
            'session-1',
            Uint8Array.of(1, 2, 3),
            'direct-object',
            invoke,
        )).resolves.toMatchObject({ byteSize: 3 })
        await pinExistingCasObject('session-1', '55'.repeat(32), 7, 'owner-manifest', invoke)
        await sealCasJob('session-1', invoke)
        await releaseCasJob('session-1', 'committed', invoke)

        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'asset_cas_job_begin',
            'asset_cas_job_prepare',
            'asset_cas_job_pin_existing',
            'asset_cas_job_seal',
            'asset_cas_job_release',
        ])
        expect(invoke).not.toHaveBeenCalledWith(expect.stringContaining('catalog'), expect.anything())
    })
})
