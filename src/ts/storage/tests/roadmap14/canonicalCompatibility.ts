import { createHash } from 'node:crypto'

export const ABSENT = Symbol('roadmap14-absent')

export type CanonicalValue =
    | { tag: 'absent' }
    | { tag: 'array-hole' }
    | { tag: 'undefined' }
    | { tag: 'null' }
    | { tag: 'boolean'; value: boolean }
    | { tag: 'number'; value: number }
    | { tag: 'string'; value: string }
    | { tag: 'array'; items: CanonicalValue[] }
    | { tag: 'object'; entries: [string, CanonicalValue][] }

export interface SemanticMismatch {
    path: string
    kind:
        | 'tag'
        | 'scalar'
        | 'array-length'
        | 'object-key-order'
        | 'object-key-set'
    expected?: unknown
    actual?: unknown
}

export interface SemanticComparison {
    equal: boolean
    mismatches: SemanticMismatch[]
}

function assertSupportedNumber(value: number): void {
    if (!Number.isFinite(value)) {
        throw new TypeError('Canonical compatibility values require finite numbers')
    }
}

export function toCanonicalValue(
    value: unknown | typeof ABSENT,
    ancestors: ReadonlySet<object> = new Set(),
): CanonicalValue {
    if (value === ABSENT) return { tag: 'absent' }
    if (value === undefined) return { tag: 'undefined' }
    if (value === null) return { tag: 'null' }
    if (typeof value === 'boolean') return { tag: 'boolean', value }
    if (typeof value === 'number') {
        assertSupportedNumber(value)
        return { tag: 'number', value }
    }
    if (typeof value === 'string') return { tag: 'string', value }
    if (typeof value !== 'object') {
        throw new TypeError(`Unsupported canonical compatibility value: ${typeof value}`)
    }
    if (ancestors.has(value)) {
        throw new TypeError('Canonical compatibility values cannot contain cycles')
    }

    const nextAncestors = new Set(ancestors)
    nextAncestors.add(value)
    if (Array.isArray(value)) {
        return {
            tag: 'array',
            items: Array.from({ length: value.length }, (_, index) => index in value
                ? toCanonicalValue(value[index], nextAncestors)
                : { tag: 'array-hole' }),
        }
    }

    return {
        tag: 'object',
        entries: Object.keys(value).map((key) => [
            key,
            toCanonicalValue((value as Record<string, unknown>)[key], nextAncestors),
        ]),
    }
}

function childPath(path: string, key: string): string {
    return `${path}[${JSON.stringify(key)}]`
}

function compareCanonicalValues(
    expected: CanonicalValue,
    actual: CanonicalValue,
    path: string,
    mismatches: SemanticMismatch[],
): void {
    if (expected.tag !== actual.tag) {
        mismatches.push({
            path,
            kind: 'tag',
            expected: expected.tag,
            actual: actual.tag,
        })
        return
    }

    if (expected.tag === 'boolean' && actual.tag === 'boolean') {
        if (expected.value !== actual.value) {
            mismatches.push({ path, kind: 'scalar', expected: expected.value, actual: actual.value })
        }
        return
    }
    if (expected.tag === 'number' && actual.tag === 'number') {
        if (!Object.is(expected.value, actual.value)) {
            mismatches.push({ path, kind: 'scalar', expected: expected.value, actual: actual.value })
        }
        return
    }
    if (expected.tag === 'string' && actual.tag === 'string') {
        if (expected.value !== actual.value) {
            mismatches.push({ path, kind: 'scalar', expected: expected.value, actual: actual.value })
        }
        return
    }
    if (expected.tag === 'array' && actual.tag === 'array') {
        if (expected.items.length !== actual.items.length) {
            mismatches.push({
                path,
                kind: 'array-length',
                expected: expected.items.length,
                actual: actual.items.length,
            })
        }
        const sharedLength = Math.min(expected.items.length, actual.items.length)
        for (let index = 0; index < sharedLength; index += 1) {
            compareCanonicalValues(
                expected.items[index],
                actual.items[index],
                `${path}[${index}]`,
                mismatches,
            )
        }
        return
    }
    if (expected.tag === 'object' && actual.tag === 'object') {
        const expectedKeys = expected.entries.map(([key]) => key)
        const actualKeys = actual.entries.map(([key]) => key)
        const sameSet = expectedKeys.length === actualKeys.length
            && expectedKeys.every((key) => actualKeys.includes(key))
        if (!sameSet) {
            mismatches.push({
                path,
                kind: 'object-key-set',
                expected: expectedKeys,
                actual: actualKeys,
            })
            return
        }
        if (expectedKeys.some((key, index) => key !== actualKeys[index])) {
            mismatches.push({
                path,
                kind: 'object-key-order',
                expected: expectedKeys,
                actual: actualKeys,
            })
        }
        const actualByKey = new Map(actual.entries)
        for (const [key, expectedValue] of expected.entries) {
            compareCanonicalValues(
                expectedValue,
                actualByKey.get(key)!,
                childPath(path, key),
                mismatches,
            )
        }
    }
}

export function compareSemanticValues(expected: unknown, actual: unknown): SemanticComparison {
    const mismatches: SemanticMismatch[] = []
    compareCanonicalValues(
        toCanonicalValue(expected),
        toCanonicalValue(actual),
        '$',
        mismatches,
    )
    return { equal: mismatches.length === 0, mismatches }
}

function uint32(value: number): Uint8Array {
    const bytes = new Uint8Array(4)
    new DataView(bytes.buffer).setUint32(0, value, false)
    return bytes
}

function concatBytes(parts: readonly Uint8Array[]): Uint8Array {
    const result = new Uint8Array(parts.reduce((length, part) => length + part.byteLength, 0))
    let offset = 0
    for (const part of parts) {
        result.set(part, offset)
        offset += part.byteLength
    }
    return result
}

function lengthDelimited(bytes: Uint8Array): Uint8Array {
    return concatBytes([uint32(bytes.byteLength), bytes])
}

function tagged(tag: string, parts: readonly Uint8Array[] = []): Uint8Array {
    return concatBytes([new TextEncoder().encode(tag), ...parts])
}

export function canonicalValueBytes(value: CanonicalValue): Uint8Array {
    switch (value.tag) {
        case 'absent':
            return tagged('A')
        case 'array-hole':
            return tagged('H')
        case 'undefined':
            return tagged('U')
        case 'null':
            return tagged('N')
        case 'boolean':
            return tagged(value.value ? 'T' : 'F')
        case 'number': {
            const bytes = new Uint8Array(8)
            new DataView(bytes.buffer).setFloat64(0, value.value, false)
            return tagged('D', [lengthDelimited(bytes)])
        }
        case 'string':
            return tagged('S', [lengthDelimited(new TextEncoder().encode(value.value))])
        case 'array':
            return tagged('L', [
                uint32(value.items.length),
                ...value.items.map((item) => lengthDelimited(canonicalValueBytes(item))),
            ])
        case 'object':
            return tagged('O', [
                uint32(value.entries.length),
                ...value.entries.flatMap(([key, item]) => [
                    lengthDelimited(new TextEncoder().encode(key)),
                    lengthDelimited(canonicalValueBytes(item)),
                ]),
            ])
    }
}

export function canonicalBytes(value: unknown | typeof ABSENT): Uint8Array {
    return canonicalValueBytes(toCanonicalValue(value))
}

export function canonicalValueSha256(value: CanonicalValue): string {
    return createHash('sha256').update(canonicalValueBytes(value)).digest('hex')
}

export function canonicalSha256(value: unknown | typeof ABSENT): string {
    return createHash('sha256').update(canonicalBytes(value)).digest('hex')
}
