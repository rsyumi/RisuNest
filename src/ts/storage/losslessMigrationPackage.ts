export const losslessMigrationMagic = new TextEncoder().encode('RISUMIGRATION\0')
export const losslessMigrationVersion = 1

export type LosslessMigrationEntryKind = 'database' | 'asset' | 'inlay' | 'cold'
export type LosslessMigrationInlayType = 'image' | 'video' | 'audio' | 'signature'
export type EmptyMigrationMetadata = Readonly<Record<string, never>>

export interface AssetMigrationMetadata {
    readonly kind: 'asset'
    readonly mime: string
    readonly name: string
    readonly ext: string
}

export interface InlayMigrationMetadata {
    readonly kind: 'inlay'
    readonly mime: string
    readonly name: string
    readonly ext: string
    readonly inlayType: LosslessMigrationInlayType
    readonly width?: number
    readonly height?: number
}

export type LosslessMigrationMetadata = EmptyMigrationMetadata | AssetMigrationMetadata | InlayMigrationMetadata

export interface LosslessMigrationInputEntry {
    readonly kind: LosslessMigrationEntryKind
    readonly id: string
    readonly metadata: LosslessMigrationMetadata
    readonly data: Uint8Array
}

export interface LosslessMigrationManifestEntry {
    readonly kind: LosslessMigrationEntryKind
    readonly id: string
    readonly metadata: LosslessMigrationMetadata
    readonly size: number
    readonly sha256: string
}

export interface LosslessMigrationManifest {
    readonly version: 1
    readonly entries: readonly LosslessMigrationManifestEntry[]
}

export type DecodedLosslessMigrationEntry = LosslessMigrationManifestEntry & { readonly data: Uint8Array }

export interface DecodedLosslessMigrationPackage {
    readonly version: 1
    readonly manifest: LosslessMigrationManifest
    entries(): AsyncIterable<DecodedLosslessMigrationEntry>
}

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue }

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

function canonicalize(value: JsonValue): JsonValue {
    if (Array.isArray(value)) return value.map(canonicalize)
    if (!isPlainObject(value)) return value
    return Object.fromEntries(
        Object.keys(value).sort().map((key) => [key, canonicalize(value[key] as JsonValue)]),
    )
}

function canonicalStringify(value: JsonValue): string {
    return JSON.stringify(canonicalize(value))
}

function isEntryKind(value: unknown): value is LosslessMigrationEntryKind {
    return value === 'database' || value === 'asset' || value === 'inlay' || value === 'cold'
}

function hasExactKeys(value: Record<string, unknown>, required: readonly string[], optional: readonly string[] = []): boolean {
    const keys = Object.keys(value)
    return required.every((key) => keys.includes(key))
        && keys.every((key) => required.includes(key) || optional.includes(key))
}

function validateCommonBlobMetadata(value: Record<string, unknown>): void {
    if (typeof value.mime !== 'string' || !value.mime.trim()) throw new Error('Migration entry metadata requires a MIME string')
    if (typeof value.name !== 'string' || !value.name) throw new Error('Migration entry metadata requires a name string')
    if (typeof value.ext !== 'string') throw new Error('Migration entry metadata requires an extension string')
}

function validateMetadata(kind: LosslessMigrationEntryKind, value: unknown): LosslessMigrationMetadata {
    if (!isPlainObject(value)) throw new Error('Migration entry metadata must be an object')
    if (kind === 'database' || kind === 'cold') {
        if (Object.keys(value).length !== 0) throw new Error(`${kind} migration metadata must be empty`)
        return {}
    }
    if (kind === 'asset') {
        if (!hasExactKeys(value, ['kind', 'mime', 'name', 'ext']) || value.kind !== 'asset') {
            throw new Error('Asset migration metadata has missing, mismatched, or unexpected fields')
        }
        validateCommonBlobMetadata(value)
        return { kind: 'asset', mime: value.mime as string, name: value.name as string, ext: value.ext as string }
    }
    if (!hasExactKeys(value, ['kind', 'mime', 'name', 'ext', 'inlayType'], ['width', 'height']) || value.kind !== 'inlay') {
        throw new Error('Inlay migration metadata has missing, mismatched, or unexpected fields')
    }
    validateCommonBlobMetadata(value)
    if (value.inlayType !== 'image' && value.inlayType !== 'video'
        && value.inlayType !== 'audio' && value.inlayType !== 'signature') {
        throw new Error('Inlay migration metadata has an unsupported inlay type')
    }
    for (const dimension of ['width', 'height'] as const) {
        const size = value[dimension]
        if (size !== undefined && (typeof size !== 'number' || !Number.isFinite(size) || size < 0)) {
            throw new Error(`Inlay migration metadata ${dimension} must be finite and nonnegative`)
        }
    }
    return {
        kind: 'inlay',
        mime: value.mime as string,
        name: value.name as string,
        ext: value.ext as string,
        inlayType: value.inlayType,
        ...(value.width === undefined ? {} : { width: value.width as number }),
        ...(value.height === undefined ? {} : { height: value.height as number }),
    }
}

function isWellFormedUnicode(value: string): boolean {
    for (let index = 0; index < value.length; index++) {
        const code = value.charCodeAt(index)
        if (code >= 0xd800 && code <= 0xdbff) {
            if (index + 1 >= value.length) return false
            const next = value.charCodeAt(index + 1)
            if (next < 0xdc00 || next > 0xdfff) return false
            index++
        } else if (code >= 0xdc00 && code <= 0xdfff) {
            return false
        }
    }
    return true
}

function validateLogicalId(kind: LosslessMigrationEntryKind, id: unknown): asserts id is string {
    if (typeof id !== 'string' || !id || !isWellFormedUnicode(id)) {
        throw new Error('Migration entry has unsafe or malformed Unicode in its logical ID')
    }
    if (id !== id.normalize('NFC')) throw new Error('Migration entry logical ID must use normalized Unicode')
    if (/[\u0000-\u001f\u007f-\u009f]/.test(id) || /%(?:2e|2f|5c)/i.test(id)
        || id.includes('\\') || id.startsWith('/') || /^[a-z]:/i.test(id)) {
        throw new Error('Migration entry has an unsafe logical ID')
    }
    const segments = id.split('/')
    if (segments.some((segment) => {
        const compatible = segment.normalize('NFKC')
        return !segment || segment === '.' || segment === '..'
            || compatible.includes('/') || compatible.includes('\\')
            || compatible === '.' || compatible === '..'
            || /[\u0000-\u001f\u007f-\u009f]/.test(compatible)
            || /%(?:2e|2f|5c)/i.test(compatible)
    })) {
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
    for await (const entry of input) {
        if (!isPlainObject(entry) || !isEntryKind(entry.kind)) {
            throw new Error('Migration entry has an unsupported kind or shape')
        }
        validateLogicalId(entry.kind, entry.id)
        const metadata = validateMetadata(entry.kind, entry.metadata)
        if (!(entry.data instanceof Uint8Array)) throw new Error('Migration entry data must be a Uint8Array')
        entries.push({
            kind: entry.kind,
            id: entry.id,
            metadata,
            data: entry.data.slice(),
        } as LosslessMigrationInputEntry)
    }
    return entries
}

export async function encodeLosslessMigrationPackage(
    input: Iterable<LosslessMigrationInputEntry> | AsyncIterable<LosslessMigrationInputEntry>,
): Promise<Uint8Array> {
    const entries = await collectInput(input)
    validateEntryIdentity(entries)
    entries.sort(compareEntries)

    const manifestEntries: LosslessMigrationManifestEntry[] = []
    for (const entry of entries) {
        manifestEntries.push({
            kind: entry.kind,
            id: entry.id,
            metadata: entry.metadata,
            size: entry.data.byteLength,
            sha256: await sha256(entry.data),
        })
    }
    const manifest: LosslessMigrationManifest = { version: losslessMigrationVersion, entries: manifestEntries }
    const manifestBytes = new TextEncoder().encode(canonicalStringify(manifest as unknown as JsonValue))
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
    if (!isPlainObject(raw) || !hasExactKeys(raw, ['version', 'entries'])
        || raw.version !== losslessMigrationVersion || !Array.isArray(raw.entries)) {
        throw new Error('Migration package manifest has an unsupported version or shape')
    }
    const entries: LosslessMigrationManifestEntry[] = raw.entries.map((value, index) => {
        if (!isPlainObject(value)) throw new Error(`Migration manifest entry ${index} is malformed`)
        if (!hasExactKeys(value, ['kind', 'id', 'metadata', 'size', 'sha256'])) {
            throw new Error(`Migration manifest entry ${index} has unexpected fields`)
        }
        if (!isEntryKind(value.kind)) throw new Error(`Migration manifest entry ${index} has an unsupported kind`)
        validateLogicalId(value.kind, value.id)
        const metadata = validateMetadata(value.kind, value.metadata)
        if (!Number.isSafeInteger(value.size) || (value.size as number) < 0) {
            throw new Error(`Migration manifest entry ${index} has an invalid size`)
        }
        if (typeof value.sha256 !== 'string' || !sha256Pattern.test(value.sha256)) {
            throw new Error(`Migration manifest entry ${index} has an invalid SHA-256 hash`)
        }
        return {
            kind: value.kind,
            id: value.id,
            metadata,
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
    const ownedBytes = bytes.slice()
    const { manifest: parsedManifest, frameOffset } = parseManifest(ownedBytes)
    const view = new DataView(ownedBytes.buffer, ownedBytes.byteOffset, ownedBytes.byteLength)
    const payloads: Uint8Array[] = []
    let offset = frameOffset
    for (const entry of parsedManifest.entries) {
        if (ownedBytes.byteLength - offset < frameHeaderLength) throw new Error('Migration package is missing or truncated framed entries')
        const length = readUint64(view, offset)
        offset += frameHeaderLength
        if (length !== entry.size) throw new Error(`Migration entry ${entry.id} frame size mismatch`)
        if (length > ownedBytes.byteLength - offset) throw new Error(`Migration entry ${entry.id} payload is truncated or length overflows package`)
        const data = ownedBytes.subarray(offset, offset + length)
        offset += length
        if (await sha256(data) !== entry.sha256) throw new Error(`Migration entry ${entry.id} hash mismatch`)
        payloads.push(data)
    }
    if (offset !== ownedBytes.byteLength) throw new Error('Migration package contains extra framed entries or trailing bytes')

    const entries = parsedManifest.entries.map((entry) => Object.freeze({
        ...entry,
        metadata: Object.freeze({ ...entry.metadata }),
    })) as readonly LosslessMigrationManifestEntry[]
    const manifest = Object.freeze({
        version: losslessMigrationVersion,
        entries: Object.freeze(entries),
    }) satisfies LosslessMigrationManifest

    return Object.freeze({
        version: losslessMigrationVersion,
        manifest,
        async *entries() {
            for (let index = 0; index < entries.length; index++) {
                yield Object.freeze({ ...entries[index], data: payloads[index].slice() })
            }
        },
    })
}
