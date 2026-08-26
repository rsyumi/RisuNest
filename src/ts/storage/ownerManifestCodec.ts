import { Sha256 } from '@aws-crypto/sha256-js'

export type AssetTuple = readonly [string, string, string]

export interface OwnerManifestEntry {
    tuple: AssetTuple
    payloadHash: Uint8Array | null
}

export type OwnerManifestProperty =
    | { present: false }
    | { present: true; entries: readonly OwnerManifestEntry[] }

const MAGIC = Uint8Array.of(0x52, 0x4f, 0x4d, 0x46)
const VERSION = 1
const HEADER_BYTES = 9
const MIN_ENTRY_BYTES = 13
const MAX_U32 = 0xffff_ffff
const MAX_INITIAL_CAPACITY = 64 * 1024 * 1024

export const OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES = MAX_U32

class ByteWriter {
    private bytes: Uint8Array
    private length = 0

    constructor(
        estimatedLength: number,
        private readonly maximumLength: number,
    ) {
        this.bytes = new Uint8Array(
            Math.min(
                maximumLength,
                MAX_INITIAL_CAPACITY,
                Math.max(HEADER_BYTES, estimatedLength),
            ),
        )
    }

    writeByte(value: number): void {
        this.ensureCapacity(1)
        this.bytes[this.length] = value
        this.length += 1
    }

    writeU32(value: number): void {
        this.ensureCapacity(4)
        new DataView(this.bytes.buffer).setUint32(this.length, value, true)
        this.length += 4
    }

    writeBytes(value: Uint8Array): void {
        this.ensureCapacity(value.byteLength)
        this.bytes.set(value, this.length)
        this.length += value.byteLength
    }

    finish(): Uint8Array {
        return this.bytes.slice(0, this.length)
    }

    private ensureCapacity(additionalBytes: number): void {
        const requiredLength = this.length + additionalBytes
        if (requiredLength > this.maximumLength) {
            throw new Error('owner manifest exceeds V1 size limit')
        }
        if (requiredLength <= this.bytes.byteLength) {
            return
        }

        let capacity = this.bytes.byteLength
        while (capacity < requiredLength) {
            capacity = Math.min(
                this.maximumLength,
                Math.max(requiredLength, capacity * 2),
            )
        }
        const grown = new Uint8Array(capacity)
        grown.set(this.bytes)
        this.bytes = grown
    }
}

class ByteReader {
    private offset = 0

    constructor(private readonly bytes: Uint8Array) {}

    get remaining(): number {
        return this.bytes.byteLength - this.offset
    }

    readByte(context: string): number {
        return this.readBytes(1, context)[0]
    }

    readU32(context: string): number {
        const bytes = this.readBytes(4, context)
        return new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, true)
    }

    readBytes(length: number, context: string): Uint8Array {
        if (length > this.remaining) {
            throw new Error(`truncated ${context}`)
        }
        const result = this.bytes.subarray(this.offset, this.offset + length)
        this.offset += length
        return result
    }
}

function isWellFormedUtf16(value: string): boolean {
    for (let index = 0; index < value.length; index += 1) {
        const codeUnit = value.charCodeAt(index)
        if (codeUnit >= 0xd800 && codeUnit <= 0xdbff) {
            const next = value.charCodeAt(index + 1)
            if (!(next >= 0xdc00 && next <= 0xdfff)) {
                return false
            }
            index += 1
        } else if (codeUnit >= 0xdc00 && codeUnit <= 0xdfff) {
            return false
        }
    }
    return true
}

function writeString(
    writer: ByteWriter,
    encoder: TextEncoder,
    value: string,
): void {
    if (!isWellFormedUtf16(value)) {
        throw new Error(
            'owner manifest string cannot be represented exactly as UTF-8',
        )
    }
    const bytes = encoder.encode(value)
    if (bytes.byteLength > MAX_U32) {
        throw new Error('owner manifest string exceeds V1 size limit')
    }
    writer.writeU32(bytes.byteLength)
    writer.writeBytes(bytes)
}

function readString(reader: ByteReader, decoder: TextDecoder): string {
    const length = reader.readU32('string length')
    const bytes = reader.readBytes(length, 'string')
    try {
        return decoder.decode(bytes)
    } catch {
        throw new Error('invalid UTF-8 in owner manifest string')
    }
}

function validateMaximumCanonicalBytes(maximumCanonicalBytes: number): void {
    if (
        !Number.isInteger(maximumCanonicalBytes) ||
        maximumCanonicalBytes < 0 ||
        maximumCanonicalBytes > OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES
    ) {
        throw new Error('invalid owner manifest V1 size limit')
    }
}

export function encodeOwnerManifest(
    entries: readonly OwnerManifestEntry[],
    maximumCanonicalBytes = OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES,
): Uint8Array {
    if (entries.length > MAX_U32) {
        throw new Error('owner manifest entry count exceeds V1 limit')
    }

    validateMaximumCanonicalBytes(maximumCanonicalBytes)
    const estimatedLength = Math.min(
        maximumCanonicalBytes,
        HEADER_BYTES + entries.length * 64,
    )
    const writer = new ByteWriter(estimatedLength, maximumCanonicalBytes)
    const encoder = new TextEncoder()
    writer.writeBytes(MAGIC)
    writer.writeByte(VERSION)
    writer.writeU32(entries.length)

    for (const entry of entries) {
        writeString(writer, encoder, entry.tuple[0])
        writeString(writer, encoder, entry.tuple[1])
        writeString(writer, encoder, entry.tuple[2])
        if (entry.payloadHash === null) {
            writer.writeByte(0)
            continue
        }
        if (entry.payloadHash.byteLength !== 32) {
            throw new Error('payload hash must be 32 bytes')
        }
        writer.writeByte(1)
        writer.writeBytes(entry.payloadHash)
    }

    return writer.finish()
}

export function decodeOwnerManifest(
    bytes: Uint8Array,
    maximumCanonicalBytes = OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES,
): OwnerManifestEntry[] {
    validateMaximumCanonicalBytes(maximumCanonicalBytes)
    if (bytes.byteLength > maximumCanonicalBytes) {
        throw new Error('owner manifest exceeds V1 size limit')
    }
    const reader = new ByteReader(bytes)
    const magic = reader.readBytes(MAGIC.byteLength, 'owner manifest magic')
    if (!magic.every((byte, index) => byte === MAGIC[index])) {
        throw new Error('invalid owner manifest magic')
    }
    if (reader.readByte('owner manifest version') !== VERSION) {
        throw new Error('unsupported owner manifest version')
    }

    const entryCount = reader.readU32('owner manifest entry count')
    if (entryCount > Math.floor(reader.remaining / MIN_ENTRY_BYTES)) {
        throw new Error('owner manifest entry count exceeds remaining bytes')
    }

    const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true })
    const entries: OwnerManifestEntry[] = []
    for (let index = 0; index < entryCount; index += 1) {
        const tuple: AssetTuple = [
            readString(reader, decoder),
            readString(reader, decoder),
            readString(reader, decoder),
        ]
        const hashMarker = reader.readByte('payload hash marker')
        let payloadHash: Uint8Array | null
        if (hashMarker === 0) {
            payloadHash = null
        } else if (hashMarker === 1) {
            payloadHash = reader.readBytes(32, 'payload hash').slice()
        } else {
            throw new Error('invalid payload hash marker')
        }
        entries.push({ tuple, payloadHash })
    }

    if (reader.remaining !== 0) {
        throw new Error('trailing bytes after owner manifest')
    }
    return entries
}

export function encodeOwnerManifestProperty(
    property: OwnerManifestProperty,
): Uint8Array | null {
    return property.present ? encodeOwnerManifest(property.entries) : null
}

export function decodeOwnerManifestProperty(
    present: boolean,
    bytes: Uint8Array | null,
): OwnerManifestProperty {
    if (!present) {
        if (bytes !== null) {
            throw new Error('absent property cannot have manifest bytes')
        }
        return { present: false }
    }
    if (bytes === null) {
        throw new Error('present property requires manifest bytes')
    }
    return { present: true, entries: decodeOwnerManifest(bytes) }
}

export async function ownerManifestIdentity(bytes: Uint8Array): Promise<string> {
    const sha256 = new Sha256()
    sha256.update(bytes)
    const digest = await sha256.digest()
    return Array.from(digest, (byte) =>
        byte.toString(16).padStart(2, '0'),
    ).join('')
}
