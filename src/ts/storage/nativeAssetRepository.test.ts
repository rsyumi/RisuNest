import { describe, expect, it, vi } from 'vitest'

import {
    beginCasJob,
    createNativeDurableAssetWriteSessionFactory,
    createNativeDurableCasJobSessionFactory,
    createNativeImmutablePayloadCas,
    createNativeNewInlayImageEncoder,
    finalizeContentCasJob,
    NATIVE_CAS_IPC_CHUNK_BYTES,
    pinExistingCasObject,
    prepareCasObject,
    releaseCasJob,
    sealCasJob,
} from './nativeAssetRepository'

describe('native asset repository adapters', () => {
    it('rejects bare native writes and uses native CAS commands for bounded reads', async () => {
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'asset_cas_read_object_range') {
                return args?.start === 0 ? [1, 2, 3] : [2]
            }
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

    it('streams large CAS writes through bounded chunks and always cancels the spool', async () => {
        const hash = '22'.repeat(32)
        const acknowledgements: number[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'asset_cas_job_upload_open') {
                return { capacity: 64 * 1024 }
            }
            if (command === 'asset_cas_job_upload_chunk') {
                const next = Number(args?.offset) + (args?.data as number[]).length
                acknowledgements.push(next)
                return next
            }
            if (command === 'asset_cas_job_upload_finish') {
                return {
                    contentHash: hash,
                    byteSize: 64 * 1024 + 3,
                    physicalKey: `assets-v2/objects/22/${hash.slice(2)}`,
                    deduplicated: false,
                }
            }
            return undefined
        })

        await expect(prepareCasObject(
            'session-large',
            new Uint8Array(64 * 1024 + 3),
            'direct-object',
            invoke,
        )).resolves.toMatchObject({ byteSize: 64 * 1024 + 3 })

        expect(acknowledgements).toEqual([64 * 1024, 64 * 1024 + 3])
        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'asset_cas_job_upload_open',
            'asset_cas_job_upload_chunk',
            'asset_cas_job_upload_chunk',
            'asset_cas_job_upload_finish',
            'asset_cas_job_upload_cancel',
        ])
        expect(invoke).not.toHaveBeenCalledWith('asset_cas_job_prepare', expect.anything())
        const chunks = invoke.mock.calls
            .filter(([command]) => command === 'asset_cas_job_upload_chunk')
            .map(([, args]) => (args?.data as number[]).length)
        expect(chunks).toEqual([64 * 1024, 3])
    })

    it('assembles large CAS reads from bounded range responses', async () => {
        const source = new Uint8Array(NATIVE_CAS_IPC_CHUNK_BYTES + 5)
        source[source.length - 1] = 9
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'asset_cas_stat_object') return source.byteLength
            if (command === 'asset_cas_read_object_range') {
                return Array.from(source.subarray(Number(args?.start), Number(args?.endExclusive)))
            }
            throw new Error(`Unexpected command ${command}`)
        })

        await expect(createNativeImmutablePayloadCas(invoke).readObject('11'.repeat(32)))
            .resolves.toEqual(source)
        const rangeCalls = invoke.mock.calls.filter(
            ([command]) => command === 'asset_cas_read_object_range',
        )
        expect(rangeCalls).toHaveLength(2)
        expect(rangeCalls.map(([, args]) => Number(args?.endExclusive) - Number(args?.start)))
            .toEqual([NATIVE_CAS_IPC_CHUNK_BYTES, 5])
    })

    it('forwards configurable inlay options and accepts truthful PNG metadata', async () => {
        const invoke = vi.fn(async () => ({
            data: [4, 5, 6],
            outputSize: 3,
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

    it('streams large Inlay encoder input and output through bounded native media commands', async () => {
        const transferSize = 64 * 1024 + 1
        const source = new Uint8Array(transferSize)
        source.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
        const output = new Uint8Array(transferSize)
        output[transferSize - 1] = 7
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_media_inlay_input_open') return { capacity: 64 * 1024 }
            if (command === 'native_media_inlay_input_chunk') {
                return Number(args?.offset) + (args?.data as number[]).length
            }
            if (command === 'native_media_encode_inlay_finish') {
                return {
                    data: null,
                    outputId: 'encoded-output',
                    outputSize: transferSize,
                    metadata: {
                        key: 'large-inlay', kind: 'inlay', size: transferSize,
                        mime: 'image/png', name: 'large.png', ext: 'png',
                        inlayType: 'image', width: 1, height: 1,
                    },
                }
            }
            if (command === 'native_media_inlay_output_read') {
                return Array.from(output.subarray(
                    Number(args?.start),
                    Number(args?.endExclusive),
                ))
            }
            return undefined
        })

        await expect(createNativeNewInlayImageEncoder(invoke).encodeNewInlayImage(
            'large-inlay',
            source,
            { name: 'large.png', options: { format: 'png', quality: 85, maxDimension: 0, skipReencode: false } },
        )).resolves.toMatchObject({ data: output })

        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'native_media_inlay_input_open',
            'native_media_inlay_input_chunk',
            'native_media_inlay_input_chunk',
            'native_media_encode_inlay_finish',
            'native_media_inlay_input_cancel',
            'native_media_inlay_output_read',
            'native_media_inlay_output_read',
            'native_media_inlay_output_cancel',
        ])
        expect(invoke).not.toHaveBeenCalledWith(
            'native_media_encode_inlay_image',
            expect.anything(),
        )
    })

    it('normalizes ignored original conversion options before invoking native code', async () => {
        const source = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
        const invoke = vi.fn(async () => ({
            data: Array.from(source),
            outputSize: source.byteLength,
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
            outputSize: 1,
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
