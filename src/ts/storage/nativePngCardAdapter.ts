import { Buffer } from 'buffer'

export interface PreparedNativePngCardMetadata {
    chara?: string
    ccv3?: string
}

export interface PreparedNativePngCardDecodeDependencies {
    hash(bytes: Uint8Array): Promise<string>
    decrypt(bytes: Uint8Array, password: string): Promise<Uint8Array | ArrayBuffer>
    requestPassword(): Promise<string | null>
}

export type DecodedPreparedNativePngCard = Record<string, unknown>

export class UnsupportedPreparedNativeCharacterCardError extends TypeError {
    readonly code = 'unsupported-character-card'

    constructor(message = 'Prepared content is not a supported character card') {
        super(message)
        this.name = 'UnsupportedPreparedNativeCharacterCardError'
    }
}

export class InvalidPreparedNativePngCardError extends TypeError {
    readonly code = 'invalid-png-card-metadata'

    constructor(message: string) {
        super(message)
        this.name = 'InvalidPreparedNativePngCardError'
    }
}

function decodeStandardBase64(value: string, label: string): Uint8Array {
    if (
        value.length === 0
        || value.length % 4 !== 0
        || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)
    ) {
        throw new InvalidPreparedNativePngCardError(`${label} is not valid standard base64`)
    }
    return Uint8Array.from(Buffer.from(value, 'base64'))
}

function parseJsonObject(bytes: Uint8Array, label: string): DecodedPreparedNativePngCard {
    let value: unknown
    try {
        value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
    }
    catch {
        throw new InvalidPreparedNativePngCardError(`${label} is not valid UTF-8 JSON`)
    }
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        throw new InvalidPreparedNativePngCardError(`${label} must decode to a JSON object`)
    }
    return value as DecodedPreparedNativePngCard
}

function normalizeV2CharacterVersion(card: DecodedPreparedNativePngCard): void {
    if (card.spec !== 'chara_card_v2') return
    const data = card.data
    if (typeof data !== 'object' || data === null || Array.isArray(data)) return
    const characterVersion = (data as Record<string, unknown>).character_version
    if (typeof characterVersion === 'number') {
        (data as Record<string, unknown>).character_version = characterVersion.toString()
    }
}

function assertNoInlinePayloads(card: DecodedPreparedNativePngCard): void {
    const data = card.data
    if (typeof data !== 'object' || data === null || Array.isArray(data)) return
    const cardData = data as Record<string, unknown>

    if (card.spec === 'chara_card_v3' && Array.isArray(cardData.assets)) {
        for (const value of cardData.assets) {
            if (
                typeof value === 'object'
                && value !== null
                && !Array.isArray(value)
                && typeof (value as Record<string, unknown>).uri === 'string'
                && ((value as Record<string, unknown>).uri as string).startsWith('data:')
            ) {
                throw new UnsupportedPreparedNativeCharacterCardError(
                    'Prepared native PNG cannot activate an inline data URI payload',
                )
            }
        }
        return
    }

    if (card.spec !== 'chara_card_v2') return
    const extensions = cardData.extensions
    if (typeof extensions !== 'object' || extensions === null || Array.isArray(extensions)) return
    const risuai = (extensions as Record<string, unknown>).risuai
    if (typeof risuai !== 'object' || risuai === null || Array.isArray(risuai)) return
    const risu = risuai as Record<string, unknown>
    const references: unknown[] = []
    for (const field of ['emotions', 'additionalAssets'] as const) {
        const tuples = risu[field]
        if (!Array.isArray(tuples)) continue
        for (const tuple of tuples) {
            if (Array.isArray(tuple)) references.push(tuple[1])
        }
    }
    const vits = risu.vits
    if (typeof vits === 'object' && vits !== null && !Array.isArray(vits)) {
        references.push(...Object.values(vits))
    }
    if (references.some((reference) => typeof reference === 'string' && !reference.startsWith('__asset:'))) {
        throw new UnsupportedPreparedNativeCharacterCardError(
            'Prepared native PNG cannot activate an inline v2 base64 payload',
        )
    }
}

async function decodeRcc(
    encoded: string,
    dependencies: PreparedNativePngCardDecodeDependencies,
): Promise<DecodedPreparedNativePngCard | null> {
    const parts = encoded.split('||')
    if (parts.length !== 5 || parts[0] !== 'rcc' || parts[1] !== 'rccv1') {
        throw new InvalidPreparedNativePngCardError('RCC envelope is invalid')
    }
    const encrypted = decodeStandardBase64(parts[2], 'RCC encrypted payload')
    if (await dependencies.hash(encrypted) !== parts[3]) {
        throw new InvalidPreparedNativePngCardError('RCC encrypted payload hash does not match')
    }
    const envelope = parseJsonObject(
        decodeStandardBase64(parts[4], 'RCC envelope metadata'),
        'RCC envelope metadata',
    )
    let password = 'RISU_NONE'
    if (envelope.usePassword === true) {
        const requested = await dependencies.requestPassword()
        if (!requested) return null
        password = requested
    }
    let decrypted: Uint8Array | ArrayBuffer
    try {
        decrypted = await dependencies.decrypt(encrypted, password)
    }
    catch {
        throw new InvalidPreparedNativePngCardError('RCC payload could not be decrypted')
    }
    return parseJsonObject(
        decrypted instanceof Uint8Array ? decrypted : new Uint8Array(decrypted),
        'RCC payload',
    )
}

export async function decodePreparedNativePngCardMetadata(
    metadata: PreparedNativePngCardMetadata,
    dependencies: PreparedNativePngCardDecodeDependencies,
): Promise<DecodedPreparedNativePngCard | null> {
    const selected = metadata.ccv3 ?? metadata.chara
    if (!selected) {
        throw new InvalidPreparedNativePngCardError('PNG card metadata is missing')
    }
    const card = selected.startsWith('rcc||')
        ? await decodeRcc(selected, dependencies)
        : parseJsonObject(
            decodeStandardBase64(selected, 'PNG card metadata'),
            'PNG card metadata',
        )
    if (!card) return null
    normalizeV2CharacterVersion(card)
    assertNoInlinePayloads(card)
    return card
}
