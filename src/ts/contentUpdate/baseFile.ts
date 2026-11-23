// Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/ts/characterUpdateBaseFile.ts), GPL-3.0.

import { Buffer } from 'buffer'
import { unzipSync } from 'fflate'
import { defaultSdDataFunc, type character, type customscript, type loreBook, type triggerscript } from '../storage/database.svelte'
import { AppendableBuffer } from '../appendableBuffer'
import { PngChunk } from '../pngChunk'
import { convertCharbook, convertOffSpecCards } from '../characterCards'
import type { RisuModule } from '../process/modules'
import { convertCharacterToModule } from '../interchangeability'
import { convertJsonToModule } from '../process/moduleImportLocation'
import { decodeImportUtf8 } from '../importUtf8'
import { normalizeOnboarding } from '../onboarding'
import { decodeRPack } from '../rpack/rpack_js'
import { MAX_CONTENT_METADATA_BYTES } from '../storage/contentImportLimits'

// Reads an earlier version of a character or module file as the base of an
// update, without installing it or saving its assets. Only the text the card
// carries is read, so asset fields compare as empty in the base.

export class UnsupportedUpdateBaseFileError extends Error {
    constructor() {
        super('Unsupported update base file')
    }
}

type Card = { spec?: string, data?: Record<string, any> }

const text = (data: Uint8Array) => decodeImportUtf8(data)
const isRecord = (value: unknown) => !!value && typeof value === 'object' && !Array.isArray(value)

/**
 * What an earlier file holds besides its assets: the card and module text a
 * base is read from. The native apps read these without the rest of the file.
 */
export interface UpdateBaseParts {
    /** A whole JSON or lorebook file. */
    file?: Uint8Array
    /** card.json of a CHARX. */
    card?: Uint8Array
    /** The decoded metadata of a RISUM file, alone or inside a CHARX. */
    module?: Uint8Array
    /** The card text chunks of a PNG, in base64. */
    ccv3?: string
    chara?: string
}

export type UpdateBaseFormat = 'json' | 'png' | 'charx' | 'risum'

/** The reader a file goes to by its name, as the importers pick one. */
export function updateBaseFormat(name: string): UpdateBaseFormat | null {
    const lower = name.toLowerCase()
    if (lower.endsWith('.risum')) return 'risum'
    if (lower.endsWith('.json') || lower.endsWith('.lorebook')) return 'json'
    if (lower.endsWith('.png')) return 'png'
    if (lower.endsWith('.charx') || lower.endsWith('.jpg') || lower.endsWith('.jpeg')) return 'charx'
    return null
}

async function pngText(data: Uint8Array): Promise<UpdateBaseParts> {
    const parts: UpdateBaseParts = {}
    for await (const chunk of PngChunk.readGenerator(data)) {
        if (!chunk || chunk instanceof AppendableBuffer) break
        if (chunk.key === 'ccv3') parts.ccv3 = chunk.value
        else if (chunk.key === 'chara') parts.chara = chunk.value
    }
    return parts
}

function unzipCharx(data: Uint8Array): Record<string, Uint8Array> {
    const filter = (file: { name: string, originalSize: number }) => {
        if (file.name !== 'card.json' && file.name !== 'module.risum') return false
        // The importer refuses a card or module larger than this, so it is not inflated here.
        if (file.originalSize > MAX_CONTENT_METADATA_BYTES) throw new UnsupportedUpdateBaseFileError()
        return true
    }
    let failure: unknown = null
    const read = (from: number) => {
        try {
            const files = unzipSync(from ? data.subarray(from) : data, { filter })
            if (files['card.json']) return files
        } catch (error) {
            if (error instanceof UnsupportedUpdateBaseFileError) throw error
            failure ??= error
        }
        return null
    }
    const whole = read(0)
    if (whole) return whole
    // A JPEG with a CHARX appended: read from the local file header the CHARX starts at, past image bytes that look like one.
    for (let i = 1; i + 3 < data.length; i++) {
        if (data[i] !== 0x50 || data[i + 1] !== 0x4b || data[i + 2] !== 3 || data[i + 3] !== 4) continue
        const files = read(i)
        if (files) return files
    }
    if (failure) throw failure
    return {}
}

/** The metadata of a RISUM file, decoded, without its assets. */
async function risumMetadata(data: Uint8Array): Promise<Uint8Array> {
    const buffer = Buffer.from(data)
    if (buffer.length < 6 || buffer.readUInt8(0) !== 111 || buffer.readUInt8(1) !== 0) throw new UnsupportedUpdateBaseFileError()
    const length = buffer.readUInt32LE(2)
    return decodeRPack(buffer.subarray(6, 6 + length))
}

/** The parts of a file read whole, as the web reads a picked file. */
export async function readUpdateBaseParts(format: UpdateBaseFormat, data: Uint8Array): Promise<UpdateBaseParts> {
    switch (format) {
        case 'json': return { file: data }
        case 'png': return pngText(data)
        case 'risum': return { module: await risumMetadata(data) }
        case 'charx': {
            const files = unzipCharx(data)
            return {
                card: files['card.json'],
                module: files['module.risum'] ? await risumMetadata(files['module.risum']) : undefined,
            }
        }
    }
}

const nativeParts = { 1: 'file', 2: 'card', 3: 'module', 4: 'ccv3', 5: 'chara' } as const

/** The parts the native reader returns, each a tag byte, a little-endian length and its bytes. */
export function decodeNativeUpdateBaseParts(data: Uint8Array): UpdateBaseParts {
    const parts: UpdateBaseParts = {}
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength)
    for (let offset = 0; offset < data.length;) {
        const part = nativeParts[data[offset] as keyof typeof nativeParts]
        const length = offset + 5 <= data.length ? view.getUint32(offset + 1, true) : -1
        if (!part || length < 0 || offset + 5 + length > data.length) throw new Error('The native reader returned an invalid update base')
        const bytes = data.subarray(offset + 5, offset + 5 + length)
        if (part === 'ccv3' || part === 'chara') parts[part] = new TextDecoder().decode(bytes)
        else parts[part] = bytes
        offset += 5 + length
    }
    return parts
}

/** Reads the parts of a file by its path in the native apps, which skip its assets. */
async function readNativeUpdateBaseParts(path: string, format: UpdateBaseFormat): Promise<UpdateBaseParts> {
    const { invoke } = await import('@tauri-apps/api/core')
    let response: ArrayBuffer | number[]
    try {
        response = await invoke<ArrayBuffer | number[]>('native_content_update_base_read', { path, format })
    } catch (error) {
        const failure = error as { code?: unknown, message?: unknown } | null
        if (failure?.code === 'unsupported-format') throw new UnsupportedUpdateBaseFileError()
        throw new Error(typeof failure?.message === 'string' ? failure.message : String(error))
    }
    return decodeNativeUpdateBaseParts(response instanceof ArrayBuffer ? new Uint8Array(response) : Uint8Array.from(response))
}

/** The module record of RISUM metadata. */
function moduleFromMetadata(data: Uint8Array): RisuModule {
    const main = JSON.parse(text(data))
    if (main?.type !== 'risuModule' || !isRecord(main.module)) throw new UnsupportedUpdateBaseFileError()
    return main.module
}

/** The card fields a character import sets from text, as importCharacterCardSpec maps them. */
function cardToCharacter(card: Card, module: RisuModule | null): Partial<character> {
    // An older card, through the importer's old-format reader.
    if (card?.spec !== 'chara_card_v2' && card?.spec !== 'chara_card_v3') return convertOffSpecCards(card as any)
    const data = card.data
    // The card metadata the importers need, as the native CHARX reader checks it.
    if (!data || !isRecord(data.extensions) || (data.extensions.risuai !== undefined && !isRecord(data.extensions.risuai))) throw new UnsupportedUpdateBaseFileError()
    // The module of a CHARX gives the card its scripts, and a RisuAI block when it has none, as its import does.
    const risu = module
        ? { ...data.extensions.risuai, customScripts: module.regex ?? [], triggerscript: module.trigger ?? [] }
        : data.extensions.risuai
    let lore: loreBook[] = module?.lorebook ?? []
    let loreSettings: character['loreSettings']
    let loreExt: unknown
    if (data.character_book) {
        const converted = convertCharbook({ lorebook: [], charbook: structuredClone(data.character_book), loresettings: undefined, loreExt: undefined })
        if (!module?.lorebook) lore = converted.lorebook
        loreSettings = converted.loresettings
        loreExt = converted.loreExt
    }
    const extentions: Record<string, unknown> = { ...(data.extensions ?? {}) }
    delete extentions.risuai
    delete extentions.depth_prompt
    const char: Partial<character> = {
        name: data.name ?? '',
        desc: data.description ?? '',
        firstMessage: data.first_mes ?? '',
        alternateGreetings: data.alternate_greetings ?? [],
        personality: data.personality ?? '',
        scenario: data.scenario ?? '',
        exampleMessage: data.mes_example ?? '',
        creatorNotes: data.creator_notes ?? '',
        systemPrompt: data.system_prompt ?? '',
        tags: data.tags ?? [],
        creator: data.creator ?? '',
        characterVersion: `${data.character_version}` || '',
        additionalData: { tag: data.tags ?? [], creator: data.creator, character_version: data.character_version },
        replaceGlobalNote: data.post_history_instructions ?? '',
        globalLore: lore,
        loreSettings,
        loreExt,
        customscript: (risu?.customScripts ?? []) as customscript[],
        triggerscript: (risu?.triggerscript ?? []) as triggerscript[],
        backgroundHTML: risu?.backgroundHTML,
        license: risu?.license,
        private: risu?.private ?? false,
        additionalText: risu?.additionalText ?? '',
        extentions,
        largePortrait: risu?.largePortrait ?? !risu,
        lorePlus: risu?.lorePlus ?? false,
        inlayViewScreen: risu?.inlayViewScreen ?? false,
        newGenData: risu?.newGenData,
        defaultVariables: risu?.defaultVariables ?? '',
        lowLevelAccess: risu?.lowLevelAccess ?? false,
        hideChatIcon: risu?.hideChatIcon ?? false,
        onboarding: normalizeOnboarding(risu?.onboarding),
        source: risu?.source ?? [],
        moduleNamespace: risu?.moduleNamespace,
        customModuleToggle: risu?.toggles ?? {},
        prebuiltAssetCommand: risu?.prebuiltAssetCommand ?? '',
        prebuiltAssetExclude: risu?.prebuiltAssetExclude ?? [],
        prebuiltAssetStyle: risu?.prebuiltAssetStyle ?? '',
        bias: risu?.bias,
        viewScreen: risu?.viewScreen ?? 'none',
        utilityBot: risu?.utilityBot ?? false,
        sdData: risu?.sdData ?? defaultSdDataFunc(),
    }
    if (card.spec === 'chara_card_v3') {
        char.nickname = data.nickname ?? ''
        char.source = data.source ?? risu?.source ?? []
    }
    return char
}

function defined<T extends object>(value: T): Partial<T> {
    return Object.fromEntries(Object.entries(value).filter(([, item]) => item !== undefined)) as Partial<T>
}

// Install bookkeeping and asset paths of another install never compare.
const moduleIgnored = ['id', 'assets', 'icon', 'updateBase', 'updateLineage', 'mcp']

/** An earlier file: its bytes on the web, its path where the app reads it natively. */
export type UpdateBaseSource = { name: string, data: Uint8Array } | { name: string, path: string }

/** Reads a character or module file as the base of an update. */
export async function readUpdateBaseFile(kind: 'character' | 'module', file: UpdateBaseSource): Promise<Partial<character> | Partial<RisuModule>> {
    const format = updateBaseFormat(file.name)
    if (!format) throw new UnsupportedUpdateBaseFileError()
    const parts = 'path' in file ? await readNativeUpdateBaseParts(file.path, format) : await readUpdateBaseParts(format, file.data)
    return updateBaseFromParts(kind, format, parts)
}

/** The base an earlier file gives, from the parts of it either reader returns. */
export async function updateBaseFromParts(kind: 'character' | 'module', format: UpdateBaseFormat, parts: UpdateBaseParts): Promise<Partial<character> | Partial<RisuModule>> {
    let card: Card | null = null
    let module: RisuModule | null = null
    if (format === 'risum') {
        if (!parts.module) throw new UnsupportedUpdateBaseFileError()
        module = moduleFromMetadata(parts.module)
    } else if (format === 'json') {
        const parsed = JSON.parse(text(parts.file ?? new Uint8Array()))
        // A lorebook or regex file is the module its import converts it to.
        module = parsed?.type === 'risuModule' ? parsed : kind === 'module' && isRecord(parsed) ? (await convertJsonToModule(parsed))?.module ?? null : null
        // Other JSON is a card; the JSON importer takes an older one only with these fields.
        const spec = parsed?.spec === 'chara_card_v2' || parsed?.spec === 'chara_card_v3'
        if (!module && (spec || ((parsed?.char_name || parsed?.name) && (parsed.char_persona || parsed.description) && (parsed.char_greeting || parsed.first_mes)))) card = parsed
    } else if (format === 'png') {
        const encoded = parts.ccv3 || parts.chara
        if (!encoded || encoded.startsWith('rcc||')) throw new UnsupportedUpdateBaseFileError()
        card = JSON.parse(text(Buffer.from(encoded, 'base64')))
    } else {
        if (!parts.card) throw new UnsupportedUpdateBaseFileError()
        card = JSON.parse(text(parts.card))
        // The CHARX importer takes a V3 card only.
        if (card?.spec !== 'chara_card_v3') throw new UnsupportedUpdateBaseFileError()
        if (parts.module) module = moduleFromMetadata(parts.module)
    }
    if (kind === 'character') {
        if (!card) throw new UnsupportedUpdateBaseFileError()
        return defined(cardToCharacter(card, module))
    }
    // A card, CHARX ones included, imports as a module through the character it holds.
    if (card) module = convertCharacterToModule(cardToCharacter(card, module) as character)
    if (!module) throw new UnsupportedUpdateBaseFileError()
    const out: Record<string, unknown> = { ...module }
    for (const key of moduleIgnored) delete out[key]
    delete out.type
    return defined(out) as Partial<RisuModule>
}
