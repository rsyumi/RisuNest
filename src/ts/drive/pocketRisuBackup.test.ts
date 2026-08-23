import { describe, expect, it } from 'vitest'
import {
    classifyPocketRisuEntry,
    decodePocketRisuInlaySidecar,
    PocketRisuInlayImporter,
} from './pocketRisuBackup'
import type { BlobWriteMetadata } from '../storage/blobStore'

const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value))

describe('classifyPocketRisuEntry', () => {
    it('classifies inlay data entries with an extension', () => {
        expect(classifyPocketRisuEntry('inlay/abc-123.webp')).toEqual({
            kind: 'inlay-data',
            id: 'abc-123',
            ext: 'webp',
        })
    })

    it('classifies extensionless inlay entries as legacy', () => {
        expect(classifyPocketRisuEntry('inlay/abc-123')).toEqual({
            kind: 'inlay-legacy',
            id: 'abc-123',
        })
    })

    it('classifies sidecar and legacy info entries', () => {
        expect(classifyPocketRisuEntry('inlay_sidecar/abc-123')).toEqual({
            kind: 'inlay-sidecar',
            id: 'abc-123',
        })
        expect(classifyPocketRisuEntry('inlay_info/abc-123')).toEqual({
            kind: 'inlay-info',
            id: 'abc-123',
        })
    })

    it('skips thumbnail and key-value metadata namespaces', () => {
        expect(classifyPocketRisuEntry('inlay_thumb/abc-123.webp')).toEqual({ kind: 'skip' })
        expect(classifyPocketRisuEntry('inlay_meta/inlay_meta/abc')).toEqual({ kind: 'skip' })
    })

    it('skips nested or empty inlay ids instead of storing them as assets', () => {
        expect(classifyPocketRisuEntry('inlay/nested/evil.png')).toEqual({ kind: 'skip' })
        expect(classifyPocketRisuEntry('inlay/')).toEqual({ kind: 'skip' })
        expect(classifyPocketRisuEntry('inlay_sidecar/nested/evil')).toEqual({ kind: 'skip' })
    })

    it('leaves every other backup entry to the generic handlers', () => {
        expect(classifyPocketRisuEntry('database.risudat')).toBeNull()
        expect(classifyPocketRisuEntry('coldstorage/9f8b7c6d-1a2b-3c4d-5e6f-a1b2c3d4e5f6.json')).toBeNull()
        expect(classifyPocketRisuEntry('abcdef.png')).toBeNull()
        expect(classifyPocketRisuEntry('inlay_6162.risuinlay')).toBeNull()
    })
})

describe('decodePocketRisuInlaySidecar', () => {
    it('decodes a valid sidecar', () => {
        expect(decodePocketRisuInlaySidecar(encode({
            ext: 'webp',
            name: 'picture.webp',
            type: 'image',
            width: 640,
            height: 480,
        }))).toEqual({
            ext: 'webp',
            name: 'picture.webp',
            type: 'image',
            width: 640,
            height: 480,
        })
    })

    it('rejects malformed or unknown-typed sidecars', () => {
        expect(decodePocketRisuInlaySidecar(encode({ ext: 'webp', name: 'x', type: 'archive' }))).toBeNull()
        expect(decodePocketRisuInlaySidecar(encode({ name: 'x', type: 'image' }))).toBeNull()
        expect(decodePocketRisuInlaySidecar(encode([1, 2, 3]))).toBeNull()
        expect(decodePocketRisuInlaySidecar(new TextEncoder().encode('not json'))).toBeNull()
    })
})

type PutCall = { id: string, data: Uint8Array, metadata: BlobWriteMetadata }

function makeImporter() {
    const puts: PutCall[] = []
    const importer = new PocketRisuInlayImporter(async (id, data, metadata) => {
        puts.push({ id, data, metadata })
    })
    return { importer, puts }
}

describe('PocketRisuInlayImporter', () => {
    const bytes = new Uint8Array([1, 2, 3, 4])
    const sidecar = encode({ ext: 'webp', name: 'pic.webp', type: 'image', width: 10, height: 20 })

    it('pairs data with its sidecar regardless of entry order', async () => {
        for (const order of [['data', 'sidecar'], ['sidecar', 'data']] as const) {
            const { importer, puts } = makeImporter()
            for (const step of order) {
                if (step === 'data') {
                    await importer.add({ kind: 'inlay-data', id: 'a', ext: 'webp' }, bytes)
                } else {
                    await importer.add({ kind: 'inlay-sidecar', id: 'a' }, sidecar)
                }
            }
            await importer.finish()
            expect(puts).toHaveLength(1)
            expect(puts[0].id).toBe('a')
            expect(puts[0].data).toEqual(bytes)
            expect(puts[0].metadata).toEqual({
                kind: 'inlay',
                inlayType: 'image',
                mime: 'image/webp',
                name: 'pic.webp',
                ext: 'webp',
                width: 10,
                height: 20,
            })
        }
    })

    it('imports data without a sidecar using extension-derived metadata', async () => {
        const { importer, puts } = makeImporter()
        await importer.add({ kind: 'inlay-data', id: 'b', ext: 'mp3' }, bytes)
        await importer.finish()
        expect(puts).toHaveLength(1)
        expect(puts[0].metadata).toMatchObject({
            kind: 'inlay',
            inlayType: 'audio',
            mime: 'audio/mpeg',
            ext: 'mp3',
        })
    })

    it('drops data whose type cannot be determined', async () => {
        const { importer, puts } = makeImporter()
        await importer.add({ kind: 'inlay-data', id: 'c', ext: 'exe' }, bytes)
        await importer.finish()
        expect(puts).toHaveLength(0)
        expect(importer.failedIds).toEqual(['c'])
    })

    it('imports the legacy JSON data URI form', async () => {
        const { importer, puts } = makeImporter()
        const payload = Buffer.from([9, 8, 7]).toString('base64')
        await importer.add({ kind: 'inlay-legacy', id: 'd' }, encode({
            name: 'old.png',
            data: `data:image/png;base64,${payload}`,
            ext: 'png',
            type: 'image',
            width: 3,
            height: 1,
        }))
        await importer.finish()
        expect(puts).toHaveLength(1)
        expect(puts[0].data).toEqual(new Uint8Array([9, 8, 7]))
        expect(puts[0].metadata).toEqual({
            kind: 'inlay',
            inlayType: 'image',
            mime: 'image/png',
            name: 'old.png',
            ext: 'png',
            width: 3,
            height: 1,
        })
    })

    it('imports the legacy signature form as JSON bytes', async () => {
        const { importer, puts } = makeImporter()
        const signature = JSON.stringify({ signatures: [], source: 'x' })
        await importer.add({ kind: 'inlay-legacy', id: 'e' }, encode({
            name: 'sig',
            data: signature,
            ext: 'json',
            type: 'signature',
        }))
        await importer.finish()
        expect(puts).toHaveLength(1)
        expect(new TextDecoder().decode(puts[0].data)).toBe(signature)
        expect(puts[0].metadata).toMatchObject({ inlayType: 'signature', mime: 'application/json' })
    })

    it('treats a non-JSON legacy entry as raw bytes paired with inlay_info metadata', async () => {
        const { importer, puts } = makeImporter()
        await importer.add({ kind: 'inlay-legacy', id: 'f' }, bytes)
        await importer.add({ kind: 'inlay-info', id: 'f' }, encode({ ext: 'webp', name: 'f.webp', type: 'image' }))
        await importer.finish()
        expect(puts).toHaveLength(1)
        expect(puts[0].data).toEqual(bytes)
        expect(puts[0].metadata).toMatchObject({ inlayType: 'image', ext: 'webp', name: 'f.webp' })
    })

    it('ignores sidecars that never receive data', async () => {
        const { importer, puts } = makeImporter()
        await importer.add({ kind: 'inlay-sidecar', id: 'g' }, sidecar)
        await importer.finish()
        expect(puts).toHaveLength(0)
    })

    it('records a failed id when the store write throws', async () => {
        const importer = new PocketRisuInlayImporter(async () => {
            throw new Error('quota')
        })
        await importer.add({ kind: 'inlay-data', id: 'h', ext: 'png' }, bytes)
        await importer.finish()
        expect(importer.failedIds).toEqual(['h'])
    })
})
