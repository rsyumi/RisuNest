import { describe, expect, it } from 'vitest'
import {
    decodeLosslessMigrationPackage,
    encodeLosslessMigrationPackage,
    losslessMigrationMagic,
    type LosslessMigrationInputEntry,
} from '../losslessMigrationPackage'

const encoder = new TextEncoder()

function fixture(): LosslessMigrationInputEntry[] {
    return [
        { kind: 'cold', id: 'chat-b', metadata: { compression: 'fflate' }, data: new Uint8Array([41, 42]) },
        { kind: 'asset', id: 'assets/voice.mp3', metadata: { ext: 'mp3', kind: 'asset', mime: 'audio/mpeg', name: 'voice.mp3' }, data: new Uint8Array([20, 21]) },
        { kind: 'inlay', id: 'signature-id', metadata: { ext: 'json', inlayType: 'signature', kind: 'inlay', mime: 'application/json', name: 'signature' }, data: encoder.encode('{"ok":true}') },
        { kind: 'database', id: 'database.risudat', metadata: { format: 'risudat' }, data: new Uint8Array([82, 73, 83, 85]) },
        { kind: 'asset', id: 'assets/photo.jpg', metadata: { ext: 'jpg', kind: 'asset', mime: 'image/jpeg', name: 'photo.jpg' }, data: new Uint8Array([10, 11, 12]) },
        { kind: 'inlay', id: 'audio-id', metadata: { ext: 'mp3', inlayType: 'audio', kind: 'inlay', mime: 'audio/mpeg', name: 'audio' }, data: new Uint8Array([31, 32]) },
        { kind: 'asset', id: 'assets/movie.webm', metadata: { ext: 'webm', kind: 'asset', mime: 'video/webm', name: 'movie.webm' }, data: new Uint8Array([22, 23, 24]) },
        { kind: 'inlay', id: 'image-id', metadata: { height: 48, ext: 'png', width: 64, inlayType: 'image', kind: 'inlay', mime: 'image/png', name: 'image' }, data: new Uint8Array([30]) },
        { kind: 'asset', id: 'assets/photo.png', metadata: { ext: 'png', kind: 'asset', mime: 'image/png', name: 'photo.png' }, data: new Uint8Array([1, 2, 3]) },
        { kind: 'cold', id: 'character-a', metadata: { compression: 'fflate' }, data: new Uint8Array([40]) },
        { kind: 'inlay', id: 'video-id', metadata: { ext: 'webm', inlayType: 'video', kind: 'inlay', mime: 'video/webm', name: 'video' }, data: new Uint8Array([33, 34, 35]) },
        { kind: 'asset', id: 'assets/empty.bin', metadata: { ext: 'bin', kind: 'asset', mime: 'application/octet-stream', name: 'empty.bin' }, data: new Uint8Array() },
    ]
}

async function collect(bytes: Uint8Array) {
    const decoded = await decodeLosslessMigrationPackage(bytes)
    const entries = []
    for await (const entry of decoded.entries()) entries.push(entry)
    return { decoded, entries }
}

function readManifest(packageBytes: Uint8Array): { manifest: Record<string, unknown>; frameOffset: number } {
    const view = new DataView(packageBytes.buffer, packageBytes.byteOffset, packageBytes.byteLength)
    const manifestLength = view.getUint32(losslessMigrationMagic.byteLength + 4, true)
    const start = losslessMigrationMagic.byteLength + 8
    return {
        manifest: JSON.parse(new TextDecoder().decode(packageBytes.subarray(start, start + manifestLength))),
        frameOffset: start + manifestLength,
    }
}

function withManifest(packageBytes: Uint8Array, manifest: Record<string, unknown>): Uint8Array {
    const old = readManifest(packageBytes)
    const encodedManifest = encoder.encode(JSON.stringify(manifest))
    const result = new Uint8Array(losslessMigrationMagic.byteLength + 8 + encodedManifest.byteLength + packageBytes.byteLength - old.frameOffset)
    result.set(losslessMigrationMagic)
    const view = new DataView(result.buffer)
    view.setUint32(losslessMigrationMagic.byteLength, 1, true)
    view.setUint32(losslessMigrationMagic.byteLength + 4, encodedManifest.byteLength, true)
    result.set(encodedManifest, losslessMigrationMagic.byteLength + 8)
    result.set(packageBytes.subarray(old.frameOffset), losslessMigrationMagic.byteLength + 8 + encodedManifest.byteLength)
    return result
}

describe('lossless migration package', () => {
    it('round trips every namespace, metadata field, exact byte, and zero-length payload', async () => {
        const original = fixture()
        const packageBytes = await encodeLosslessMigrationPackage(original)
        const { decoded, entries } = await collect(packageBytes)

        expect(packageBytes.subarray(0, losslessMigrationMagic.byteLength)).toEqual(losslessMigrationMagic)
        expect(decoded.version).toBe(1)
        expect(entries.map(({ kind, id }) => `${kind}:${id}`)).toEqual([
            'database:database.risudat',
            'asset:assets/empty.bin',
            'asset:assets/movie.webm',
            'asset:assets/photo.jpg',
            'asset:assets/photo.png',
            'asset:assets/voice.mp3',
            'inlay:audio-id',
            'inlay:image-id',
            'inlay:signature-id',
            'inlay:video-id',
            'cold:character-a',
            'cold:chat-b',
        ])
        for (const entry of entries) {
            const source = original.find((candidate) => candidate.kind === entry.kind && candidate.id === entry.id)
            expect(source).toBeDefined()
            expect(entry.metadata).toEqual(source?.metadata)
            expect(entry.data).toEqual(source?.data)
            expect(entry.size).toBe(source?.data.byteLength)
            expect(entry.sha256).toMatch(/^[0-9a-f]{64}$/)
        }
        expect(entries.find((entry) => entry.id === 'database.risudat')?.sha256)
            .toBe('a55005c93998f995c57b487248d706958a9b90e86c4363ccc16d1fece7a723bf')
        expect(entries.find((entry) => entry.id === 'assets/photo.png')?.sha256)
            .toBe('039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81')
        expect(entries.find((entry) => entry.id === 'assets/empty.bin')?.sha256)
            .toBe('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855')
        expect(entries.find((entry) => entry.id === 'assets/empty.bin')?.data).toEqual(new Uint8Array())
    })

    it('encodes identical logical input deterministically regardless of input order or metadata property order', async () => {
        const first = fixture()
        const second = [...first].reverse().map((entry) => ({
            ...entry,
            metadata: Object.fromEntries(Object.entries(entry.metadata).reverse()),
        }))

        expect(await encodeLosslessMigrationPackage(first)).toEqual(await encodeLosslessMigrationPackage(second))
    })

    it('accepts an async iterable and exposes validated entries as an async iterable', async () => {
        async function* source() {
            for (const entry of fixture()) yield entry
        }
        const { entries } = await collect(await encodeLosslessMigrationPackage(source()))
        expect(entries).toHaveLength(fixture().length)
    })

    it.each([
        ['duplicate namespace ID', (manifest: any) => manifest.entries.push({ ...manifest.entries[0] }), /duplicate/i],
        ['unsafe traversal ID', (manifest: any) => { manifest.entries[1].id = 'assets/../secret' }, /unsafe/i],
        ['unsafe absolute ID', (manifest: any) => { manifest.entries[1].id = '/assets/secret' }, /unsafe/i],
        ['unsupported kind', (manifest: any) => { manifest.entries[1].kind = 'thumbnail' }, /kind/i],
    ])('rejects %s from an untrusted manifest', async (_name, mutate, message) => {
        const bytes = await encodeLosslessMigrationPackage(fixture())
        const { manifest } = readManifest(bytes)
        mutate(manifest)
        await expect(decodeLosslessMigrationPackage(withManifest(bytes, manifest))).rejects.toThrow(message)
    })

    it('rejects an unsupported schema version in the header and manifest', async () => {
        const bytes = await encodeLosslessMigrationPackage(fixture())
        const headerVersion = bytes.slice()
        new DataView(headerVersion.buffer).setUint32(losslessMigrationMagic.byteLength, 2, true)
        await expect(decodeLosslessMigrationPackage(headerVersion)).rejects.toThrow(/version/i)

        const { manifest } = readManifest(bytes)
        manifest.version = 2
        await expect(decodeLosslessMigrationPackage(withManifest(bytes, manifest))).rejects.toThrow(/version/i)
    })

    it('rejects size and literal SHA-256 mismatches before returning a decoded package', async () => {
        const bytes = await encodeLosslessMigrationPackage(fixture())
        const size = readManifest(bytes).manifest as any
        size.entries[0].size += 1
        await expect(decodeLosslessMigrationPackage(withManifest(bytes, size))).rejects.toThrow(/size/i)

        const hash = readManifest(bytes).manifest as any
        hash.entries[0].sha256 = '0'.repeat(64)
        await expect(decodeLosslessMigrationPackage(withManifest(bytes, hash))).rejects.toThrow(/hash/i)
    })

    it('rejects missing and extra framed entries', async () => {
        const bytes = await encodeLosslessMigrationPackage(fixture())
        const { manifest } = readManifest(bytes)
        const missing = structuredClone(manifest) as any
        missing.entries.push({
            id: 'extra-cold', kind: 'cold', metadata: {}, sha256: '0'.repeat(64), size: 0,
        })
        await expect(decodeLosslessMigrationPackage(withManifest(bytes, missing))).rejects.toThrow(/missing|truncated/i)

        const extra = new Uint8Array(bytes.byteLength + 8)
        extra.set(bytes)
        await expect(decodeLosslessMigrationPackage(extra)).rejects.toThrow(/extra/i)
    })

    it('rejects truncation and malformed or overflowing frame lengths', async () => {
        const bytes = await encodeLosslessMigrationPackage(fixture())
        const { frameOffset } = readManifest(bytes)
        await expect(decodeLosslessMigrationPackage(bytes.subarray(0, bytes.byteLength - 1))).rejects.toThrow(/truncated/i)

        const malformed = bytes.slice()
        const view = new DataView(malformed.buffer)
        view.setUint32(frameOffset, 0xffffffff, true)
        view.setUint32(frameOffset + 4, 0x00200000, true)
        await expect(decodeLosslessMigrationPackage(malformed)).rejects.toThrow(/length|overflow/i)
    })

    it('rejects malformed manifest fields and package magic', async () => {
        const bytes = await encodeLosslessMigrationPackage(fixture())
        const badMagic = bytes.slice()
        badMagic[0] ^= 0xff
        await expect(decodeLosslessMigrationPackage(badMagic)).rejects.toThrow(/magic/i)

        const { manifest } = readManifest(bytes)
        ;(manifest as any).entries[0].metadata = []
        await expect(decodeLosslessMigrationPackage(withManifest(bytes, manifest))).rejects.toThrow(/metadata/i)
    })
})
