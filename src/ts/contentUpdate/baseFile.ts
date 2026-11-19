// Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/ts/characterUpdateBaseFile.ts), GPL-3.0.

import { Buffer } from 'buffer'
import { unzipSync } from 'fflate'
import type { character, customscript, loreBook, triggerscript } from '../storage/database.svelte'
import { AppendableBuffer } from '../appendableBuffer'
import { PngChunk } from '../pngChunk'
import { convertCharbook, convertOffSpecCards } from '../characterCards'
import type { RisuModule } from '../process/modules'
import { convertCharacterToModule } from '../interchangeability'
import { convertJsonToModule } from '../process/moduleImportLocation'
import { decodeImportUtf8 } from '../importUtf8'
import { normalizeOnboarding } from '../onboarding'
import { decodeRPack } from '../rpack/rpack_js'

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

async function cardFromPng(data: Uint8Array): Promise<Card> {
    let chara = ''
    let ccv3 = ''
    for await (const chunk of PngChunk.readGenerator(data)) {
        if (!chunk || chunk instanceof AppendableBuffer) break
        if (chunk.key === 'ccv3') ccv3 = chunk.value
        else if (chunk.key === 'chara') chara = chunk.value
    }
    const encoded = ccv3 || chara
    if (!encoded || encoded.startsWith('rcc||')) throw new UnsupportedUpdateBaseFileError()
    return JSON.parse(text(Buffer.from(encoded, 'base64')))
}

function unzipCharx(data: Uint8Array) {
    const filter = (file: { name: string }) => file.name === 'card.json' || file.name === 'module.risum'
    try {
        return unzipSync(data, { filter })
    } catch (error) {
        // A JPEG with a CHARX appended: read from the first local file header.
        for (let i = 0; i + 3 < data.length; i++) {
            if (data[i] === 0x50 && data[i + 1] === 0x4b && data[i + 2] === 3 && data[i + 3] === 4) return unzipSync(data.subarray(i), { filter })
        }
        throw error
    }
}

/** The module record of a RISUM file, without its assets. */
async function moduleFromRisum(data: Uint8Array): Promise<RisuModule> {
    const buffer = Buffer.from(data)
    if (buffer.length < 6 || buffer.readUInt8(0) !== 111 || buffer.readUInt8(1) !== 0) throw new UnsupportedUpdateBaseFileError()
    const length = buffer.readUInt32LE(2)
    const main = JSON.parse(text(await decodeRPack(buffer.subarray(6, 6 + length))))
    if (main?.type !== 'risuModule' || !main.module) throw new UnsupportedUpdateBaseFileError()
    return main.module
}

/** The card fields a character import sets from text, as importCharacterCardSpec maps them. */
function cardToCharacter(card: Card, module: RisuModule | null): Partial<character> {
    // An older card, through the importer's old-format reader.
    if (card?.spec !== 'chara_card_v2' && card?.spec !== 'chara_card_v3') return convertOffSpecCards(card as any)
    const data = card.data
    if (!data) throw new UnsupportedUpdateBaseFileError()
    const risu = data.extensions?.risuai
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
        replaceGlobalNote: data.post_history_instructions ?? '',
        globalLore: lore,
        loreSettings,
        loreExt,
        customscript: (module?.regex ?? risu?.customScripts ?? []) as customscript[],
        triggerscript: (module?.trigger ?? risu?.triggerscript ?? []) as triggerscript[],
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
        viewScreen: risu?.viewScreen,
        utilityBot: risu?.utilityBot,
        sdData: risu?.sdData,
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

/** Reads a character or module file as the base of an update. */
export async function readUpdateBaseFile(kind: 'character' | 'module', file: { name: string, data: Uint8Array }): Promise<Partial<character> | Partial<RisuModule>> {
    const name = file.name.toLowerCase()
    let card: Card | null = null
    let module: RisuModule | null = null
    if (name.endsWith('.risum')) {
        module = await moduleFromRisum(file.data)
    } else if (name.endsWith('.json') || name.endsWith('.lorebook')) {
        const parsed = JSON.parse(text(file.data))
        // A lorebook or regex file is the module its import converts it to.
        module = parsed?.type === 'risuModule' ? parsed : kind === 'module' ? (await convertJsonToModule(parsed))?.module ?? null : null
        // Other JSON is a card; the JSON importer takes an older one only with these fields.
        const spec = parsed?.spec === 'chara_card_v2' || parsed?.spec === 'chara_card_v3'
        if (!module && (spec || ((parsed?.char_name || parsed?.name) && (parsed.char_persona || parsed.description) && (parsed.char_greeting || parsed.first_mes)))) card = parsed
    } else if (name.endsWith('.png')) {
        card = await cardFromPng(file.data)
    } else if (name.endsWith('.charx') || name.endsWith('.jpg') || name.endsWith('.jpeg')) {
        const files = unzipCharx(file.data)
        if (!files['card.json']) throw new UnsupportedUpdateBaseFileError()
        card = JSON.parse(text(files['card.json']))
        // The CHARX importer takes a V3 card only.
        if (card?.spec !== 'chara_card_v3') throw new UnsupportedUpdateBaseFileError()
        if (files['module.risum']) module = await moduleFromRisum(files['module.risum'])
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
