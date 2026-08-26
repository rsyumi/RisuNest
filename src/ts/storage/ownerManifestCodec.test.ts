import { deflateSync, inflateSync } from 'fflate'
import { describe, expect, it } from 'vitest'
import golden from './tests/fixtures/ownerManifestV1Golden.json'
import {
    decodeOwnerManifest,
    decodeOwnerManifestProperty,
    encodeOwnerManifest,
    encodeOwnerManifestProperty,
    ownerManifestIdentity,
    type OwnerManifestEntry,
} from './ownerManifestCodec'

function fromHex(value: string): Uint8Array {
    return Uint8Array.from(
        value.match(/.{2}/g)?.map((byte) => Number.parseInt(byte, 16)) ?? [],
    )
}

function toHex(value: Uint8Array): string {
    return Array.from(value, (byte) => byte.toString(16).padStart(2, '0')).join(
        '',
    )
}

function goldenEntries(): OwnerManifestEntry[] {
    return golden.entries.map((entry) => ({
        tuple: entry.tuple as [string, string, string],
        payloadHash:
            entry.payloadHashHex === null ? null : fromHex(entry.payloadHashHex),
    }))
}

describe('owner manifest V1 codec', () => {
    it('keeps property absence separate from the canonical tuple bytes', () => {
        expect(encodeOwnerManifestProperty({ present: false })).toBeNull()

        const emptyBytes = encodeOwnerManifestProperty({
            present: true,
            entries: [],
        })
        expect(toHex(emptyBytes!)).toBe('524f4d460100000000')
        expect(decodeOwnerManifestProperty(false, null)).toEqual({
            present: false,
        })
        expect(decodeOwnerManifestProperty(true, emptyBytes)).toEqual({
            present: true,
            entries: [],
        })
        expect(() => decodeOwnerManifestProperty(false, emptyBytes)).toThrow(
            'absent property cannot have manifest bytes',
        )
        expect(() => decodeOwnerManifestProperty(true, null)).toThrow(
            'present property requires manifest bytes',
        )
    })

    it('matches the golden bytes without normalizing order, duplicates, paths, or case', async () => {
        const entries = goldenEntries()
        const canonicalBytes = encodeOwnerManifest(entries)

        expect(toHex(canonicalBytes)).toBe(golden.canonicalHex)
        expect(decodeOwnerManifest(canonicalBytes)).toEqual(entries)
        expect(await ownerManifestIdentity(canonicalBytes)).toBe(golden.manifestHash)

        const compressed = deflateSync(canonicalBytes, { level: 6 })
        expect(await ownerManifestIdentity(inflateSync(compressed))).toBe(
            golden.manifestHash,
        )
    })

    it('preserves a leading BOM in every tuple position', () => {
        const decoded = decodeOwnerManifest(fromHex(golden.canonicalHex))

        expect(decoded.at(-1)?.tuple).toEqual([
            '\ufeffname',
            '\ufeffpath',
            '\ufeffextension',
        ])
    })

    it('rejects payload hashes that are not exactly 32 bytes', () => {
        expect(() =>
            encodeOwnerManifest([
                {
                    tuple: ['name', 'path', 'extension'],
                    payloadHash: new Uint8Array(31),
                },
            ]),
        ).toThrow('payload hash must be 32 bytes')
    })

    it('enforces the aggregate limit at the exact empty-manifest boundary', () => {
        const emptyBytes = encodeOwnerManifest([], 9)
        expect(toHex(emptyBytes)).toBe('524f4d460100000000')
        expect(decodeOwnerManifest(emptyBytes, 9)).toEqual([])
        expect(() => encodeOwnerManifest([], 8)).toThrow(
            'owner manifest exceeds V1 size limit',
        )
        expect(() => decodeOwnerManifest(emptyBytes, 8)).toThrow(
            'owner manifest exceeds V1 size limit',
        )
    })

    it('rejects ill-formed JavaScript strings instead of normalizing them', () => {
        expect(() =>
            encodeOwnerManifest([
                {
                    tuple: ['\ud800', 'path', 'extension'],
                    payloadHash: null,
                },
            ]),
        ).toThrow('string cannot be represented exactly as UTF-8')
    })

    it.each([
        ['invalid UTF-8', '524f4d46010100000001000000ff000000000000000000'],
        ['truncated field', '524f4d4601010000000200000061'],
        ['count too large', '524f4d460102000000'],
        ['count too small', `524f4d460102000000${golden.canonicalHex.slice(18)}`],
        [
            'invalid hash marker',
            '524f4d46010100000000000000000000000000000002',
        ],
        ['trailing bytes', '524f4d46010000000000'],
    ])('rejects %s', (_name, malformedHex) => {
        expect(() => decodeOwnerManifest(fromHex(malformedHex))).toThrow()
    })
})
