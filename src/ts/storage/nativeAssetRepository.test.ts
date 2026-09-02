import { describe, expect, it, vi } from 'vitest'

import {
    beginCasJob,
    createNativeDurableAssetWriteSessionFactory,
    createNativeDurableCasJobSessionFactory,
    createNativeImmutablePayloadCas,
    createNativeNewInlayImageEncoder,
    finalizeContentCasJob,
    pinExistingCasObject,
    prepareCasObject,
    releaseCasJob,
    sealCasJob,
} from './nativeAssetRepository'

describe('native asset repository adapters', () => {
    it('rejects bare native writes and uses native CAS commands for bounded reads', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_read_object') return [1, 2, 3]
            if (command === 'asset_cas_read_object_range') return [2]
            if (command === 'asset_cas_stat_object') return 3
            throw new Error(`Unexpected command ${command}`)
        })
        const cas = createNativeImmutablePayloadCas(invoke)

        await expect(cas.prepare(Uint8Array.of(1, 2, 3))).rejects.toThrow(
            'durable ownership session',
        )
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

    it('forwards configurable inlay options and accepts truthful PNG metadata', async () => {
        const invoke = vi.fn(async () => ({
            data: [4, 5, 6],
            metadata: {
                key: 'inlay-id',
                kind: 'inlay',
                size: 3,
                mime: 'image/png',
                name: 'Image',
                ext: 'png',
                inlayType: 'image',
                width: 13,
                height: 17,
            },
        }))
        const encoder = createNativeNewInlayImageEncoder(invoke)

        await expect(encoder.encodeNewInlayImage(
            'inlay-id',
            Uint8Array.of(1, 2),
            { name: 'Image', options: { format: 'png', quality: 12, maxDimension: 256, skipReencode: true } },
        )).resolves.toEqual({
            data: Uint8Array.of(4, 5, 6),
            metadata: {
                kind: 'inlay',
                mime: 'image/png',
                name: 'Image',
                ext: 'png',
                inlayType: 'image',
                width: 13,
                height: 17,
            },
        })
        expect(invoke).toHaveBeenCalledOnce()
        expect(invoke).toHaveBeenCalledWith('native_media_encode_inlay_image', {
            id: 'inlay-id', data: [1, 2], name: 'Image',
            options: { format: 'png', quality: 12, maxDimension: 256, skipReencode: true },
        })
    })

    it('normalizes ignored original conversion options before invoking native code', async () => {
        const source = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
        const invoke = vi.fn(async () => ({
            data: Array.from(source),
            metadata: {
                key: 'original-id', kind: 'inlay', size: source.byteLength,
                mime: 'image/png', name: 'original.png', ext: 'png', inlayType: 'image',
                width: 1, height: 1,
            },
        }))
        const encoder = createNativeNewInlayImageEncoder(invoke)

        await encoder.encodeNewInlayImage('original-id', source, {
            name: 'original.png',
            options: {
                format: 'original', quality: Number.NaN,
                maxDimension: Number.MAX_SAFE_INTEGER, skipReencode: true,
            },
        })

        expect(invoke).toHaveBeenCalledWith('native_media_encode_inlay_image', {
            id: 'original-id', data: Array.from(source), name: 'original.png',
            options: { format: 'original', quality: 85, maxDimension: 4_294_967_295, skipReencode: true },
        })
    })

    it.each([
        ['webp', 'image/png', 'png'],
        ['png', 'image/webp', 'webp'],
        ['original', 'image/webp', 'png'],
    ] as const)('rejects a native %s response with a mismatched MIME and extension pair', async (format, mime, ext) => {
        const encoder = createNativeNewInlayImageEncoder(async () => ({
            data: [4],
            metadata: {
                key: 'inlay-id', kind: 'inlay', size: 1, mime, name: 'Image', ext,
                inlayType: 'image', width: 1, height: 1,
            },
        }))

        await expect(encoder.encodeNewInlayImage('inlay-id', Uint8Array.of(1), {
            name: 'Image', options: { format, quality: 85, maxDimension: 0, skipReencode: false },
        })).rejects.toThrow('invalid metadata')
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

    it('binds normal asset publication to one direct-write native session', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_job_begin') return 'asset-write-1'
            if (command === 'asset_cas_job_prepare') return {
                contentHash: '66'.repeat(32),
                byteSize: 2,
                physicalKey: `assets-v2/objects/66/${'66'.repeat(32).slice(2)}`,
                deduplicated: true,
            }
            return null
        })
        const factory = createNativeDurableAssetWriteSessionFactory(invoke)

        const session = await factory.begin()
        await session.prepare(Uint8Array.of(9, 8))
        await session.seal()
        await session.release('committed')

        expect(invoke.mock.calls).toEqual([
            ['asset_cas_job_begin', { kind: 'direct-asset-or-inlay-write' }],
            ['asset_cas_job_prepare', {
                sessionId: 'asset-write-1',
                data: [9, 8],
                role: 'direct-object',
            }],
            ['asset_cas_job_seal', { sessionId: 'asset-write-1' }],
            ['asset_cas_job_release', {
                sessionId: 'asset-write-1',
                outcome: 'committed',
            }],
        ])
    })

    it('labels direct cold ownership with its exact durable job kind', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_job_begin') return 'cold-write-1'
            if (command === 'asset_cas_job_prepare') return {
                contentHash: '77'.repeat(32),
                byteSize: 1,
                physicalKey: `assets-v2/objects/77/${'77'.repeat(32).slice(2)}`,
                deduplicated: false,
            }
            return null
        })
        expect(invoke.mock.calls.map(([command]) => command)).not.toContain('asset_cas_prepare')
        const session = await createNativeDurableCasJobSessionFactory(
            'cold-direct-write',
            invoke,
        ).begin()

        await session.prepare(Uint8Array.of(7))
        await session.seal()
        await session.release('committed')

        expect(invoke.mock.calls[0]).toEqual([
            'asset_cas_job_begin',
            { kind: 'cold-direct-write' },
        ])
    })

    it('finalizes one content session without echoing asset descriptors through IPC', async () => {
        const manifestHash = '66'.repeat(32)
        const invoke = vi.fn(async () => ({
            contentHash: manifestHash,
            byteSize: 3,
            physicalKey: `assets-v2/objects/66/${manifestHash.slice(2)}`,
            deduplicated: false,
        }))

        await expect(finalizeContentCasJob(
            'content-1',
            Uint8Array.of(1, 2, 3),
            invoke,
        )).resolves.toMatchObject({ contentHash: manifestHash, byteSize: 3 })

        expect(invoke).toHaveBeenCalledOnce()
        expect(invoke).toHaveBeenCalledWith('asset_cas_job_finalize_content', {
            sessionId: 'content-1',
            ownerManifest: [1, 2, 3],
        })
        expect(JSON.stringify(invoke.mock.calls)).not.toContain('contentAssets')
    })
})
