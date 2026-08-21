export const losslessMigrationMagic = new TextEncoder().encode('RISUMIGRATION\0')
export const losslessMigrationVersion = 1

export type LosslessMigrationEntryKind = 'database' | 'asset' | 'inlay' | 'cold'
export type LosslessMigrationMetadataValue =
    | null
    | boolean
    | number
    | string
    | LosslessMigrationMetadataValue[]
    | { [key: string]: LosslessMigrationMetadataValue }
export type LosslessMigrationMetadata = { [key: string]: LosslessMigrationMetadataValue }

export interface LosslessMigrationInputEntry {
    kind: LosslessMigrationEntryKind
    id: string
    metadata: LosslessMigrationMetadata
    data: Uint8Array
}

export interface LosslessMigrationManifestEntry {
    kind: LosslessMigrationEntryKind
    id: string
    metadata: LosslessMigrationMetadata
    size: number
    sha256: string
}

export interface LosslessMigrationManifest {
    version: 1
    entries: LosslessMigrationManifestEntry[]
}

export interface DecodedLosslessMigrationEntry extends LosslessMigrationManifestEntry {
    data: Uint8Array
}

export interface DecodedLosslessMigrationPackage {
    version: 1
    manifest: LosslessMigrationManifest
    entries(): AsyncIterable<DecodedLosslessMigrationEntry>
}

const kindOrder: Record<LosslessMigrationEntryKind, number> = {
    database: 0,
    asset: 1,
    inlay: 2,
    cold: 3,
}
const headerLength = losslessMigrationMagic.byteLength + 8
const frameHeaderLength = 8
const sha256Pattern = /^[0-9a-f]{64}$/

function isPlainObject(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function validateMetadataValue(value: unknown, label: string): asserts value is LosslessMigrationMetadataValue {
    if (value === null || typeof value === 'string' || typeof value === 'boolean') return
    if (typeof value === 'number') {
        if (Number.isFinite(value)) return
        throw new Error(`${label} contains a non-finite number`)
    }
    if (Array.isArray(value)) {
        value.forEach((item, index) => validateMetadataValue(item, `${label}[${index}]`))
        return
    }
    if (isPlainObject(value)) {
        for (const [key, item] of Object.entries(value)) {
            if (!key || key.includes('\0')) throw new Error(`${label} contains an unsafe key`)
            validateMetadataValue(item, `${label}.${key}`)
        }
        return
    }
    throw new Error(`${label} must contain JSON values only`)
}

function validateMetadata(value: unknown): asserts value is LosslessMigrationMetadata {
    if (!isPlainObject(value)) throw new Error('Migration entry metadata must be an object')
    validateMetadataValue(value, 'Migration entry metadata')
}

function canonicalize(value: LosslessMigrationMetadataValue): LosslessMigrationMetadataValue {
    if (Array.isArray(value)) return value.map(canonicalize)
    if (!isPlainObject(value)) return value
    return Object.fromEntries(
        Object.keys(value).sort().map((key) => [key, canonicalize(value[key] as LosslessMigrationMetadataValue)]),
    )
}

function canonicalStringify(value: LosslessMigrationMetadataValue): string {
    return JSON.stringify(canonicalize(value))
}

function isEntryKind(value: unknown): value is LosslessMigrationEntryKind {
    return value === 'database' || value === 'asset' || value === 'inlay' || value === 'cold'
}

function validateLogicalId(kind: LosslessMigrationEntryKind, id: unknown): asserts id is string {
    if (typeof id !== 'string' || !id || id.includes('\0') || id.includes('\\')
        || id.startsWith('/') || /^[a-z]:/i.test(id)) {
        throw new Error('Migration entry has an unsafe logical ID')
    }
    const segments = id.split('/')
    if (segments.some((segment) => !segment || segment === '.' || segment === '..')) {
        throw new Error('Migration entry has an unsafe logical ID')
    }
    if (kind === 'database' && id !== 'database.risudat') {
        throw new Error('Database migration entry must use database.risudat')
    }
    if (kind === 'asset' && (!id.startsWith('assets/') || segments.length < 2)) {
        throw new Error('Asset migration entry has an unsafe logical ID')
    }
}

function compareEntries(
    left: Pick<LosslessMigrationManifestEntry, 'kind' | 'id'>,
    right: Pick<LosslessMigrationManifestEntry, 'kind' | 'id'>,
): number {
    return kindOrder[left.kind] - kindOrder[right.kind] || (left.id < right.id ? -1 : left.id > right.id ? 1 : 0)
}

function validateEntryIdentity(
    entries: readonly Pick<LosslessMigrationManifestEntry, 'kind' | 'id'>[],
): void {
    const ids = new Set<string>()
    let databaseCount = 0
    for (const entry of entries) {
        validateLogicalId(entry.kind, entry.id)
        const identity = `${entry.kind}\0${entry.id}`
        if (ids.has(identity)) throw new Error(`Migration manifest contains duplicate ID ${entry.id}`)
        ids.add(identity)
        if (entry.kind === 'database') databaseCount++
    }
    if (databaseCount !== 1) throw new Error('Migration package must contain exactly one database entry')
}

async function sha256(data: Uint8Array): Promise<string> {
    const digest = await globalThis.crypto.subtle.digest('SHA-256', data as BufferSource)
    return Array.from(new Uint8Array(digest), (value) => value.toString(16).padStart(2, '0')).join('')
}

function writeUint64(view: DataView, offset: number, value: number): void {
    if (!Number.isSafeInteger(value) || value < 0) throw new RangeError('Migration frame length overflow')
    view.setUint32(offset, value >>> 0, true)
    view.setUint32(offset + 4, Math.floor(value / 0x100000000), true)
}

function readUint64(view: DataView, offset: number): number {
    const low = view.getUint32(offset, true)
    const high = view.getUint32(offset + 4, true)
    const value = high * 0x100000000 + low
    if (!Number.isSafeInteger(value)) throw new RangeError('Migration frame length overflow')
    return value
}

async function collectInput(
    input: Iterable<LosslessMigrationInputEntry> | AsyncIterable<LosslessMigrationInputEntry>,
): Promise<LosslessMigrationInputEntry[]> {
    const entries: LosslessMigrationInputEntry[] = []
    for await (const entry of input) entries.push(entry)
    return entries
}

export async function encodeLosslessMigrationPackage(
    input: Iterable<LosslessMigrationInputEntry> | AsyncIterable<LosslessMigrationInputEntry>,
): Promise<Uint8Array> {
    const entries = await collectInput(input)
    for (const entry of entries) {
        if (!isEntryKind(entry.kind)) throw new Error('Migration entry has an unsupported kind')
        validateLogicalId(entry.kind, entry.id)
        validateMetadata(entry.metadata)
        if (!(entry.data instanceof Uint8Array)) throw new Error('Migration entry data must be a Uint8Array')
    }
    validateEntryIdentity(entries)
    entries.sort(compareEntries)

    const manifestEntries: LosslessMigrationManifestEntry[] = []
    for (const entry of entries) {
        manifestEntries.push({
            kind: entry.kind,
            id: entry.id,
            metadata: canonicalize(entry.metadata) as LosslessMigrationMetadata,
            size: entry.data.byteLength,
            sha256: await sha256(entry.data),
        })
    }
    const manifest: LosslessMigrationManifest = { version: losslessMigrationVersion, entries: manifestEntries }
    const manifestBytes = new TextEncoder().encode(canonicalStringify(manifest as unknown as LosslessMigrationMetadataValue))
    if (manifestBytes.byteLength > 0xffffffff) throw new RangeError('Migration manifest length overflow')
    const payloadLength = entries.reduce((total, entry) => total + frameHeaderLength + entry.data.byteLength, 0)
    const totalLength = headerLength + manifestBytes.byteLength + payloadLength
    if (!Number.isSafeInteger(totalLength)) throw new RangeError('Migration package length overflow')

    const result = new Uint8Array(totalLength)
    result.set(losslessMigrationMagic)
    const view = new DataView(result.buffer)
    view.setUint32(losslessMigrationMagic.byteLength, losslessMigrationVersion, true)
    view.setUint32(losslessMigrationMagic.byteLength + 4, manifestBytes.byteLength, true)
    result.set(manifestBytes, headerLength)
    let offset = headerLength + manifestBytes.byteLength
    for (const entry of entries) {
        writeUint64(view, offset, entry.data.byteLength)
        offset += frameHeaderLength
        result.set(entry.data, offset)
        offset += entry.data.byteLength
    }
    return result
}

function parseManifest(bytes: Uint8Array): { manifest: LosslessMigrationManifest; frameOffset: number } {
    if (bytes.byteLength < headerLength) throw new Error('Migration package is truncated')
    for (let index = 0; index < losslessMigrationMagic.byteLength; index++) {
        if (bytes[index] !== losslessMigrationMagic[index]) throw new Error('Invalid migration package magic')
    }
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const version = view.getUint32(losslessMigrationMagic.byteLength, true)
    if (version !== losslessMigrationVersion) throw new Error(`Unsupported migration package version ${version}`)
    const manifestLength = view.getUint32(losslessMigrationMagic.byteLength + 4, true)
    const frameOffset = headerLength + manifestLength
    if (frameOffset > bytes.byteLength) throw new Error('Migration package manifest is truncated')

    let raw: unknown
    try {
        raw = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(headerLength, frameOffset)))
    } catch {
        throw new Error('Migration package manifest is malformed')
    }
    if (!isPlainObject(raw) || raw.version !== losslessMigrationVersion || !Array.isArray(raw.entries)) {
        throw new Error('Migration package manifest has an unsupported version or shape')
    }
    const entries: LosslessMigrationManifestEntry[] = raw.entries.map((value, index) => {
        if (!isPlainObject(value)) throw new Error(`Migration manifest entry ${index} is malformed`)
        if (!isEntryKind(value.kind)) throw new Error(`Migration manifest entry ${index} has an unsupported kind`)
        validateLogicalId(value.kind, value.id)
        validateMetadata(value.metadata)
        if (!Number.isSafeInteger(value.size) || (value.size as number) < 0) {
            throw new Error(`Migration manifest entry ${index} has an invalid size`)
        }
        if (typeof value.sha256 !== 'string' || !sha256Pattern.test(value.sha256)) {
            throw new Error(`Migration manifest entry ${index} has an invalid SHA-256 hash`)
        }
        return {
            kind: value.kind,
            id: value.id,
            metadata: value.metadata,
            size: value.size as number,
            sha256: value.sha256,
        }
    })
    validateEntryIdentity(entries)
    for (let index = 1; index < entries.length; index++) {
        if (compareEntries(entries[index - 1], entries[index]) >= 0) {
            throw new Error('Migration manifest entries are not in canonical order')
        }
    }
    return { manifest: { version: losslessMigrationVersion, entries }, frameOffset }
}

export async function decodeLosslessMigrationPackage(
    bytes: Uint8Array,
): Promise<DecodedLosslessMigrationPackage> {
    if (!(bytes instanceof Uint8Array)) throw new Error('Migration package must be a Uint8Array')
    const { manifest, frameOffset } = parseManifest(bytes)
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const decodedEntries: DecodedLosslessMigrationEntry[] = []
    let offset = frameOffset
    for (const entry of manifest.entries) {
        if (bytes.byteLength - offset < frameHeaderLength) throw new Error('Migration package is missing or truncated framed entries')
        const length = readUint64(view, offset)
        offset += frameHeaderLength
        if (length !== entry.size) throw new Error(`Migration entry ${entry.id} frame size mismatch`)
        if (length > bytes.byteLength - offset) throw new Error(`Migration entry ${entry.id} payload is truncated or length overflows package`)
        const data = bytes.subarray(offset, offset + length)
        offset += length
        if (await sha256(data) !== entry.sha256) throw new Error(`Migration entry ${entry.id} hash mismatch`)
        decodedEntries.push({ ...entry, data })
    }
    if (offset !== bytes.byteLength) throw new Error('Migration package contains extra framed entries or trailing bytes')

    return {
        version: losslessMigrationVersion,
        manifest,
        async *entries() {
            for (const entry of decodedEntries) yield entry
        },
    }
}
