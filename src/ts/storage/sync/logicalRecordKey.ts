export const MAX_LOGICAL_RECORD_KEY_BYTES = 64 * 1024

const KEY_PREFIX = 'r1'
const textEncoder = new TextEncoder()
const textDecoder = new TextDecoder('utf-8', { fatal: true })
const base64urlAlphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_'

export type LogicalRecordLocator =
    | { kind: 'root' }
    | { kind: 'preset'; presetId: string }
    | { kind: 'plugin'; storageKey: string }
    | { kind: 'character'; characterId: string }
    | { kind: 'conversation'; characterId: string; conversationId: string }
    | { kind: 'asset'; logicalKey: string }
    | { kind: 'inlay'; logicalKey: string }
    | { kind: 'cold'; logicalKey: string }

function encodeBase64url(bytes: Uint8Array): string {
    let encoded = ''
    for (let index = 0; index < bytes.length; index += 3) {
        const first = bytes[index]
        const second = bytes[index + 1]
        const third = bytes[index + 2]
        encoded += base64urlAlphabet[first >> 2]
        encoded += base64urlAlphabet[((first & 0x03) << 4) | ((second ?? 0) >> 4)]
        if (second !== undefined) {
            encoded += base64urlAlphabet[((second & 0x0f) << 2) | ((third ?? 0) >> 6)]
        }
        if (third !== undefined) encoded += base64urlAlphabet[third & 0x3f]
    }
    return encoded
}

function decodeBase64url(encoded: string): Uint8Array {
    if (!/^[A-Za-z0-9_-]+$/.test(encoded) || encoded.length % 4 === 1) {
        throw new TypeError('Logical record key components are not canonical base64url')
    }
    const bytes = new Uint8Array(Math.floor(encoded.length * 6 / 8))
    let outputIndex = 0
    let accumulator = 0
    let bits = 0
    for (const character of encoded) {
        const value = base64urlAlphabet.indexOf(character)
        accumulator = (accumulator << 6) | value
        bits += 6
        if (bits >= 8) {
            bits -= 8
            bytes[outputIndex++] = (accumulator >> bits) & 0xff
        }
    }
    if (bits > 0 && (accumulator & ((1 << bits) - 1)) !== 0) {
        throw new TypeError('Logical record key components have nonzero base64url padding bits')
    }
    if (encodeBase64url(bytes) !== encoded) {
        throw new TypeError('Logical record key components are not canonical base64url')
    }
    return bytes
}

function validateComponent(value: unknown, description: string, allowEmpty: boolean): string {
    if (typeof value !== 'string' || (!allowEmpty && value.length === 0)) {
        throw new TypeError(`Logical record ${description} must be ${allowEmpty ? 'a string' : 'a nonempty string'}`)
    }
    const bytes = textEncoder.encode(value)
    if (bytes.byteLength > MAX_LOGICAL_RECORD_KEY_BYTES) {
        throw new TypeError(`Logical record ${description} exceeds the key limit`)
    }
    if (textDecoder.decode(bytes) !== value) {
        throw new TypeError(`Logical record ${description} must contain valid Unicode`)
    }
    return value
}

function validateColdLogicalKey(value: unknown): string {
    const key = validateComponent(value, 'logicalKey', false)
    if (key.includes('\0')) {
        throw new TypeError('Logical record cold logicalKey cannot contain NUL')
    }
    return key
}

function locatorParts(locator: LogicalRecordLocator): { kind: string; components: string[] } {
    switch (locator.kind) {
        case 'root':
            return { kind: 'root', components: [] }
        case 'preset':
            return {
                kind: locator.kind,
                components: [validateComponent(locator.presetId, 'presetId', false)],
            }
        case 'plugin':
            return {
                kind: locator.kind,
                components: [validateComponent(locator.storageKey, 'storageKey', true)],
            }
        case 'character':
            return {
                kind: locator.kind,
                components: [validateComponent(locator.characterId, 'characterId', false)],
            }
        case 'conversation':
            return {
                kind: locator.kind,
                components: [
                    validateComponent(locator.characterId, 'characterId', false),
                    validateComponent(locator.conversationId, 'conversationId', false),
                ],
            }
        case 'asset':
        case 'inlay':
            return {
                kind: locator.kind,
                components: [validateComponent(locator.logicalKey, 'logicalKey', true)],
            }
        case 'cold':
            return {
                kind: locator.kind,
                components: [validateColdLogicalKey(locator.logicalKey)],
            }
    }
}

export function encodeLogicalRecordKey(locator: LogicalRecordLocator): string {
    const { kind, components } = locatorParts(locator)
    const encoded = components.length === 0
        ? `${KEY_PREFIX}:${kind}`
        : `${KEY_PREFIX}:${kind}:${encodeBase64url(textEncoder.encode(JSON.stringify(components)))}`
    if (encoded.length > MAX_LOGICAL_RECORD_KEY_BYTES) {
        throw new TypeError('Logical record key exceeds the encoded key limit')
    }
    return encoded
}

function locatorFromParts(kind: string, components: unknown[]): LogicalRecordLocator {
    switch (kind) {
        case 'preset':
            if (components.length === 1) {
                return { kind, presetId: validateComponent(components[0], 'presetId', false) }
            }
            break
        case 'plugin':
            if (components.length === 1) {
                return { kind, storageKey: validateComponent(components[0], 'storageKey', true) }
            }
            break
        case 'character':
            if (components.length === 1) {
                return { kind, characterId: validateComponent(components[0], 'characterId', false) }
            }
            break
        case 'conversation':
            if (components.length === 2) {
                return {
                    kind,
                    characterId: validateComponent(components[0], 'characterId', false),
                    conversationId: validateComponent(components[1], 'conversationId', false),
                }
            }
            break
        case 'asset':
        case 'inlay':
            if (components.length === 1) {
                return { kind, logicalKey: validateComponent(components[0], 'logicalKey', true) }
            }
            break
        case 'cold':
            if (components.length === 1) {
                return { kind, logicalKey: validateColdLogicalKey(components[0]) }
            }
            break
    }
    throw new TypeError('Logical record key kind or component arity is invalid')
}

export function decodeLogicalRecordKey(encoded: string): LogicalRecordLocator {
    if (typeof encoded !== 'string' || encoded.length > MAX_LOGICAL_RECORD_KEY_BYTES) {
        throw new TypeError('Logical record key must be a bounded string')
    }
    if (encoded === `${KEY_PREFIX}:root`) return { kind: 'root' }
    const match = /^r1:([a-z]+):([A-Za-z0-9_-]+)$/.exec(encoded)
    if (!match) throw new TypeError('Logical record key format is invalid')

    let components: unknown
    try {
        components = JSON.parse(textDecoder.decode(decodeBase64url(match[2])))
    } catch (error) {
        if (error instanceof TypeError) throw error
        throw new TypeError('Logical record key components are invalid')
    }
    if (!Array.isArray(components)) {
        throw new TypeError('Logical record key components must be an array')
    }
    const locator = locatorFromParts(match[1], components)
    if (encodeLogicalRecordKey(locator) !== encoded) {
        throw new TypeError('Logical record key is not canonical')
    }
    return locator
}
