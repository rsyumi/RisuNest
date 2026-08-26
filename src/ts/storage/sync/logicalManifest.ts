import { Sha256 } from '@aws-crypto/sha256-js'

import { decodeLogicalRecordKey, MAX_LOGICAL_RECORD_KEY_BYTES } from './logicalRecordKey'

export const LOGICAL_MANIFEST_SCHEMA = 'risunest.logical-manifest/v1' as const
export const MAX_LOGICAL_MANIFEST_BYTES = 64 * 1024 * 1024
export const MAX_LOGICAL_MANIFEST_RECORDS = 250_000
export const MAX_LOGICAL_MANIFEST_OBJECTS = 500_000

const MAX_ID_BYTES = 1024
const MAX_SEQUENCE_DIGITS = 64
const SHA256_PATTERN = /^[0-9a-f]{64}$/
const EMPTY_SHA256 = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
const textEncoder = new TextEncoder()
const textDecoder = new TextDecoder('utf-8', { fatal: true })

export type LogicalManifestRecord =
    | {
          key: string
          state: 'live'
          objectHash: string
          dependencies: string[]
      }
    | {
          key: string
          state: 'tombstone'
          deletedGenerationSequence: string
      }

export interface LogicalManifestObject {
    hash: string
    size: number
}

export interface LogicalManifest {
    schema: typeof LOGICAL_MANIFEST_SCHEMA
    libraryId: string
    generation: string
    generationSequence: string
    parentGeneration: string | null
    sourceRevision: number
    records: LogicalManifestRecord[]
    objects: LogicalManifestObject[]
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) return false
    const prototype = Object.getPrototypeOf(value)
    return prototype === Object.prototype || prototype === null
}

function assertFields(
    value: Record<string, unknown>,
    fields: readonly string[],
    description: string,
): void {
    const actual = Object.keys(value).sort()
    const expected = [...fields].sort()
    if (
        actual.length !== expected.length
        || actual.some((field, index) => field !== expected[index])
    ) {
        throw new TypeError(`${description} fields are invalid`)
    }
}

function boundedString(value: unknown, description: string, allowEmpty = false): string {
    if (typeof value !== 'string' || (!allowEmpty && value.length === 0)) {
        throw new TypeError(`${description} must be ${allowEmpty ? 'a string' : 'a nonempty string'}`)
    }
    const bytes = textEncoder.encode(value)
    if (bytes.byteLength > MAX_ID_BYTES || textDecoder.decode(bytes) !== value) {
        throw new TypeError(`${description} is not a bounded Unicode string`)
    }
    return value
}

export function validateGenerationSequence(value: unknown, description: string): string {
    if (
        typeof value !== 'string'
        || value.length > MAX_SEQUENCE_DIGITS
        || !/^(0|[1-9][0-9]*)$/.test(value)
    ) {
        throw new TypeError(`${description} must be a canonical unsigned decimal string`)
    }
    return value
}

function compareGenerationSequences(left: string, right: string): number {
    if (left.length !== right.length) return left.length < right.length ? -1 : 1
    return left < right ? -1 : left > right ? 1 : 0
}

function sha256(value: unknown, description: string): string {
    if (typeof value !== 'string' || !SHA256_PATTERN.test(value)) {
        throw new TypeError(`${description} must be a lowercase SHA-256`)
    }
    return value
}

function sortedAfter(previous: string | undefined, next: string, description: string): void {
    if (previous !== undefined && previous >= next) {
        throw new TypeError(`${description} must be sorted and unique`)
    }
}

function validateRecord(value: unknown): LogicalManifestRecord {
    if (!isPlainObject(value)) throw new TypeError('Logical manifest record must be an object')
    if (value.state === 'live') {
        assertFields(value, ['key', 'state', 'objectHash', 'dependencies'], 'Live record')
        if (!Array.isArray(value.dependencies)) {
            throw new TypeError('Live record dependencies must be an array')
        }
        const dependencies: string[] = []
        let previous: string | undefined
        for (const dependency of value.dependencies) {
            const hash = sha256(dependency, 'Live record dependency')
            sortedAfter(previous, hash, 'Live record dependencies')
            dependencies.push(hash)
            previous = hash
        }
        return {
            key: validateRecordKey(value.key),
            state: 'live',
            objectHash: sha256(value.objectHash, 'Live record objectHash'),
            dependencies,
        }
    }
    if (value.state === 'tombstone') {
        assertFields(value, ['key', 'state', 'deletedGenerationSequence'], 'Tombstone record')
        return {
            key: validateRecordKey(value.key),
            state: 'tombstone',
            deletedGenerationSequence: validateGenerationSequence(
                value.deletedGenerationSequence,
                'Tombstone deletedGenerationSequence',
            ),
        }
    }
    throw new TypeError('Logical manifest record state is invalid')
}

function validateRecordKey(value: unknown): string {
    if (
        typeof value !== 'string'
        || value.length > MAX_LOGICAL_RECORD_KEY_BYTES
    ) {
        throw new TypeError('Logical manifest record key must be a bounded string')
    }
    decodeLogicalRecordKey(value)
    return value
}

function validateObject(value: unknown): LogicalManifestObject {
    if (!isPlainObject(value)) throw new TypeError('Logical manifest object must be an object')
    assertFields(value, ['hash', 'size'], 'Logical manifest object')
    if (!Number.isSafeInteger(value.size) || (value.size as number) < 0) {
        throw new TypeError('Logical manifest object size must be a nonnegative safe integer')
    }
    const hash = sha256(value.hash, 'Logical manifest object hash')
    const size = value.size as number
    if ((size === 0) !== (hash === EMPTY_SHA256)) {
        throw new TypeError('Logical manifest empty object must use the SHA-256 of empty bytes')
    }
    return { hash, size }
}

export function validateLogicalManifest(value: unknown): LogicalManifest {
    if (!isPlainObject(value)) throw new TypeError('Logical manifest must be an object')
    assertFields(value, [
        'schema',
        'libraryId',
        'generation',
        'generationSequence',
        'parentGeneration',
        'sourceRevision',
        'records',
        'objects',
    ], 'Logical manifest')
    if (value.schema !== LOGICAL_MANIFEST_SCHEMA) {
        throw new TypeError('Logical manifest schema is unsupported')
    }
    if (!Number.isSafeInteger(value.sourceRevision) || (value.sourceRevision as number) < 0) {
        throw new TypeError('Logical manifest sourceRevision must be a nonnegative safe integer')
    }
    if (!Array.isArray(value.records) || value.records.length > MAX_LOGICAL_MANIFEST_RECORDS) {
        throw new TypeError('Logical manifest records exceed the count limit')
    }
    if (!Array.isArray(value.objects) || value.objects.length > MAX_LOGICAL_MANIFEST_OBJECTS) {
        throw new TypeError('Logical manifest objects exceed the count limit')
    }
    const generationSequence = validateGenerationSequence(
        value.generationSequence,
        'Logical manifest generationSequence',
    )

    const records: LogicalManifestRecord[] = []
    let previousRecordKey: string | undefined
    for (const input of value.records) {
        const record = validateRecord(input)
        sortedAfter(previousRecordKey, record.key, 'Logical manifest records')
        records.push(record)
        previousRecordKey = record.key
    }
    for (const record of records) {
        if (
            record.state === 'tombstone'
            && compareGenerationSequences(record.deletedGenerationSequence, generationSequence) > 0
        ) {
            throw new TypeError(
                'Tombstone deletedGenerationSequence cannot exceed manifest generationSequence',
            )
        }
    }

    const objects: LogicalManifestObject[] = []
    const objectHashes = new Set<string>()
    let previousObjectHash: string | undefined
    for (const input of value.objects) {
        const object = validateObject(input)
        sortedAfter(previousObjectHash, object.hash, 'Logical manifest objects')
        objects.push(object)
        objectHashes.add(object.hash)
        previousObjectHash = object.hash
    }

    const reachable = new Set<string>()
    for (const record of records) {
        if (record.state === 'tombstone') continue
        reachable.add(record.objectHash)
        for (const dependency of record.dependencies) reachable.add(dependency)
    }
    for (const hash of reachable) {
        if (!objectHashes.has(hash)) {
            throw new TypeError(`Logical manifest is missing referenced object ${hash}`)
        }
    }
    for (const hash of objectHashes) {
        if (!reachable.has(hash)) {
            throw new TypeError(`Logical manifest object ${hash} is unreachable`)
        }
    }

    return {
        schema: LOGICAL_MANIFEST_SCHEMA,
        libraryId: boundedString(value.libraryId, 'Logical manifest libraryId'),
        generation: boundedString(value.generation, 'Logical manifest generation'),
        generationSequence,
        parentGeneration: value.parentGeneration === null
            ? null
            : boundedString(value.parentGeneration, 'Logical manifest parentGeneration'),
        sourceRevision: value.sourceRevision as number,
        records,
        objects,
    }
}

export function encodeLogicalManifest(value: LogicalManifest): Uint8Array {
    const bytes = textEncoder.encode(JSON.stringify(validateLogicalManifest(value)))
    if (bytes.byteLength > MAX_LOGICAL_MANIFEST_BYTES) {
        throw new TypeError('Logical manifest exceeds the byte limit')
    }
    return bytes
}

export function decodeLogicalManifest(bytes: Uint8Array): LogicalManifest {
    if (!(bytes instanceof Uint8Array) || bytes.byteLength > MAX_LOGICAL_MANIFEST_BYTES) {
        throw new TypeError('Logical manifest bytes exceed the byte limit')
    }
    let text: string
    let parsed: unknown
    try {
        text = textDecoder.decode(bytes)
        parsed = JSON.parse(text)
    } catch {
        throw new TypeError('Logical manifest bytes are not valid UTF-8 JSON')
    }
    const manifest = validateLogicalManifest(parsed)
    if (new TextDecoder().decode(encodeLogicalManifest(manifest)) !== text) {
        throw new TypeError('Logical manifest bytes are not canonical')
    }
    return manifest
}

export async function hashLogicalManifest(manifest: LogicalManifest): Promise<string> {
    const digest = new Sha256()
    digest.update(encodeLogicalManifest(manifest))
    return [...await digest.digest()]
        .map((value) => value.toString(16).padStart(2, '0'))
        .join('')
}
