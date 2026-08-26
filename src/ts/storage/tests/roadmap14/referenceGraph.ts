import { createHash } from 'node:crypto'

import { decodeColdStoragePayload } from '../../../process/coldstorageData'
import type { Database } from '../../database.svelte'
import type {
    Roadmap14CardFixture,
    Roadmap14Payload,
} from './losslessCorpus'

export type ReferenceOwnerKind =
    | 'root'
    | 'preset'
    | 'plugin-storage'
    | 'character'
    | 'group'
    | 'conversation'
    | 'module'
    | 'persona'
    | 'loadout'
    | 'folder'
    | 'card'
    | 'cold'

export type ReferenceTargetKind =
    | 'asset'
    | 'inlay'
    | 'cold'
    | 'character'
    | 'preset'
    | 'persona'
    | 'module'
    | 'loadout'
    | 'folder'
    | 'card'
    | 'conversation'

export type ReferenceResolutionStatus =
    | 'present'
    | 'expected-missing'
    | 'unexpected-missing'
    | 'external'
    | 'invalid'

export interface ReferenceOwner {
    kind: ReferenceOwnerKind
    id: string
}

export interface ReferenceTarget {
    kind: ReferenceTargetKind
    key: string
    metadata: Record<string, string | number | boolean | null>
}

export interface ReferenceEdge {
    owner: ReferenceOwner
    path: string
    occurrence: number
    target: ReferenceTarget
    status: ReferenceResolutionStatus
}

export type ExpectedMissingReferences = Partial<
    Record<ReferenceTargetKind, readonly string[]>
>

export interface ReferenceGraphInput {
    database: Database
    payloads: readonly Roadmap14Payload[]
    coldPayloads: readonly Roadmap14Payload[]
    cards: readonly Roadmap14CardFixture[]
    expectedMissing: ExpectedMissingReferences
}

export interface ReferenceGraphSummary {
    present: ReferenceEdge[]
    expectedMissing: ReferenceEdge[]
    unexpectedMissing: ReferenceEdge[]
    external: ReferenceEdge[]
    invalid: ReferenceEdge[]
}

interface ReferenceIndexes {
    present: Record<ReferenceTargetKind, Set<string>>
    expectedMissing: Record<ReferenceTargetKind, Set<string>>
    conversationsByCharacter: Map<string, Set<string>>
    foldersByCharacter: Map<string, Set<string>>
}

const coldStorageHeader = '\uEF01COLDSTORAGE\uEF01'
const inlayPattern = /\{\{(?:inlay|inlayed|inlayeddata)::(.+?)\}\}/g

function targetSets(): Record<ReferenceTargetKind, Set<string>> {
    return {
        asset: new Set(),
        inlay: new Set(),
        cold: new Set(),
        character: new Set(),
        preset: new Set(),
        persona: new Set(),
        module: new Set(),
        loadout: new Set(),
        folder: new Set(),
        card: new Set(),
        conversation: new Set(),
    }
}

function createIndexes(input: ReferenceGraphInput): ReferenceIndexes {
    const present = targetSets()
    const expectedMissing = targetSets()
    const conversationsByCharacter = new Map<string, Set<string>>()
    const foldersByCharacter = new Map<string, Set<string>>()

    for (const payload of input.payloads) present[payload.kind].add(payload.key)
    for (const character of input.database.characters) {
        if (character?.chaId) present.character.add(character.chaId)
        const conversations = new Set<string>()
        for (const chat of character?.chats ?? []) {
            if (chat.id) conversations.add(chat.id)
        }
        conversationsByCharacter.set(character.chaId, conversations)
        const folders = new Set<string>()
        for (const folder of character?.chatFolders ?? []) {
            if (folder.id) folders.add(folder.id)
        }
        foldersByCharacter.set(character.chaId, folders)
    }
    for (const preset of input.database.botPresets ?? []) {
        if (preset.name) present.preset.add(preset.name)
    }
    for (const module of input.database.modules ?? []) {
        if (module.id) present.module.add(module.id)
    }
    for (const persona of input.database.personas ?? []) {
        if (persona.id) present.persona.add(persona.id)
        if (persona.embeddedModule?.id) present.module.add(persona.embeddedModule.id)
    }
    for (const loadout of input.database.loadouts ?? []) {
        if (loadout.name) present.loadout.add(loadout.name)
    }
    for (const item of input.database.characterOrder ?? []) {
        if (typeof item !== 'string' && item.id) present.folder.add(item.id)
    }
    for (const card of input.cards) present.card.add(card.id)
    for (const kind of Object.keys(input.expectedMissing) as ReferenceTargetKind[]) {
        for (const key of input.expectedMissing[kind] ?? []) {
            expectedMissing[kind].add(key)
        }
    }
    return {
        present,
        expectedMissing,
        conversationsByCharacter,
        foldersByCharacter,
    }
}

function isExternalAssetKey(key: string): boolean {
    return /^(?:https?:|data:|blob:|file:|content:|tauri:)/i.test(key)
        || /^[a-z]:[\\/]/i.test(key)
}

function displayKey(value: unknown): string {
    if (typeof value === 'string') return value
    if (value === undefined) return '<undefined>'
    try {
        return JSON.stringify(value) ?? String(value)
    } catch {
        return String(value)
    }
}

function classifyReference(
    owner: ReferenceOwner,
    kind: ReferenceTargetKind,
    rawKey: unknown,
    key: string,
    metadata: ReferenceTarget['metadata'],
    indexes: ReferenceIndexes,
): ReferenceResolutionStatus {
    if (typeof rawKey !== 'string' || key === '') return 'invalid'
    if (kind === 'asset' && isExternalAssetKey(key)) return 'external'
    const characterId = typeof metadata.characterId === 'string'
        ? metadata.characterId
        : owner.kind === 'character' || owner.kind === 'group'
            ? owner.id
            : undefined
    if (kind === 'conversation' && characterId) {
        if (indexes.conversationsByCharacter.get(characterId)?.has(key)) return 'present'
        if (indexes.expectedMissing.conversation.has(key)) return 'expected-missing'
        return 'unexpected-missing'
    }
    if (kind === 'folder' && characterId) {
        if (indexes.foldersByCharacter.get(characterId)?.has(key)) return 'present'
        if (indexes.expectedMissing.folder.has(key)) return 'expected-missing'
        return 'unexpected-missing'
    }
    if (indexes.present[kind].has(key)) return 'present'
    if (indexes.expectedMissing[kind].has(key)) return 'expected-missing'
    return 'unexpected-missing'
}

function propertyPath(path: string, key: string): string {
    return /^[A-Za-z_$][\w$]*$/.test(key)
        ? `${path}.${key}`
        : `${path}[${JSON.stringify(key)}]`
}

class GraphCollector {
    readonly edges: ReferenceEdge[] = []
    readonly #occurrences = new Map<string, number>()

    constructor(private readonly indexes: ReferenceIndexes) {}

    emit(
        owner: ReferenceOwner,
        path: string,
        kind: ReferenceTargetKind,
        rawKey: unknown,
        metadata: ReferenceTarget['metadata'] = {},
    ): void {
        const ownerKey = `${owner.kind}:${owner.id}`
        const occurrence = this.#occurrences.get(ownerKey) ?? 0
        this.#occurrences.set(ownerKey, occurrence + 1)
        const key = displayKey(rawKey)
        this.edges.push({
            owner,
            path,
            occurrence,
            target: { kind, key, metadata },
            status: classifyReference(owner, kind, rawKey, key, metadata, this.indexes),
        })
    }

    optional(
        owner: ReferenceOwner,
        path: string,
        kind: ReferenceTargetKind,
        key: unknown,
        metadata: ReferenceTarget['metadata'] = {},
    ): void {
        if (key === undefined || key === null || key === '') return
        this.emit(owner, path, kind, key, metadata)
    }
}

function scanInlays(
    value: unknown,
    owner: ReferenceOwner,
    basePath: string,
    collector: GraphCollector,
    seen: WeakSet<object> = new WeakSet(),
): void {
    if (typeof value === 'string') {
        let tokenIndex = 0
        for (const match of value.matchAll(inlayPattern)) {
            collector.emit(owner, basePath, 'inlay', match[1], {
                tokenIndex,
                offset: match.index,
            })
            tokenIndex += 1
        }
        return
    }
    if (!value || typeof value !== 'object' || seen.has(value)) return
    seen.add(value)
    if (Array.isArray(value)) {
        value.forEach((item, index) => {
            scanInlays(item, owner, `${basePath}[${index}]`, collector, seen)
        })
        return
    }
    for (const key of Object.keys(value)) {
        scanInlays(
            (value as Record<string, unknown>)[key],
            owner,
            propertyPath(basePath, key),
            collector,
            seen,
        )
    }
}

function emitAsset(
    collector: GraphCollector,
    owner: ReferenceOwner,
    path: string,
    key: unknown,
    metadata: ReferenceTarget['metadata'],
): void {
    collector.optional(owner, path, 'asset', key, metadata)
}

function emitRequiredAsset(
    collector: GraphCollector,
    owner: ReferenceOwner,
    path: string,
    key: unknown,
    metadata: ReferenceTarget['metadata'],
): void {
    collector.emit(owner, path, 'asset', key, metadata)
}

function scanAssetTuples(
    collector: GraphCollector,
    owner: ReferenceOwner,
    basePath: string,
    tuples: readonly (readonly unknown[])[] | undefined,
): void {
    for (const [index, tuple] of (tuples ?? []).entries()) {
        emitRequiredAsset(collector, owner, `${basePath}[${index}][1]`, tuple[1], {
            name: typeof tuple[0] === 'string' ? tuple[0] : '',
            ext: typeof tuple[2] === 'string' ? tuple[2] : '',
        })
    }
}

function scanModule(
    module: Record<string, unknown>,
    owner: ReferenceOwner,
    collector: GraphCollector,
    basePath = '$',
    includeInlays = true,
): void {
    scanAssetTuples(
        collector,
        owner,
        `${basePath}.assets`,
        module.assets as readonly (readonly unknown[])[] | undefined,
    )
    emitAsset(collector, owner, `${basePath}.icon`, module.icon, { field: 'icon' })
    if (includeInlays) scanInlays(module, owner, basePath, collector)
}

function scanCharacterAssets(
    character: Database['characters'][number],
    owner: ReferenceOwner,
    basePath: string,
    collector: GraphCollector,
): void {
    emitAsset(collector, owner, `${basePath}.image`, character.image, { field: 'image' })
    for (const [index, emotion] of (character.emotionImages ?? []).entries()) {
        emitRequiredAsset(
            collector,
            owner,
            `${basePath}.emotionImages[${index}][1]`,
            emotion[1],
            { name: emotion[0], field: 'emotionImages' },
        )
    }
    scanAssetTuples(
        collector,
        owner,
        `${basePath}.additionalAssets`,
        character.additionalAssets,
    )
    if (character.type !== 'group') {
        for (const [name, key] of Object.entries(character.vits?.files ?? {})) {
            emitRequiredAsset(
                collector,
                owner,
                propertyPath(`${basePath}.vits.files`, name),
                key,
                { name, field: 'vits.files' },
            )
        }
        for (const [index, asset] of (character.ccAssets ?? []).entries()) {
            emitRequiredAsset(
                collector,
                owner,
                `${basePath}.ccAssets[${index}].uri`,
                asset.uri,
                { name: asset.name, ext: asset.ext, mediaType: asset.type },
            )
        }
    }
}

function scanDatabaseRoot(
    database: Database,
    collector: GraphCollector,
): void {
    const owner: ReferenceOwner = { kind: 'root', id: 'database' }
    emitAsset(collector, owner, '$.userIcon', database.userIcon, { field: 'userIcon' })
    emitAsset(collector, owner, '$.customBackground', database.customBackground, {
        field: 'customBackground',
    })
    for (const [index, moduleId] of (database.enabledModules ?? []).entries()) {
        collector.emit(owner, `$.enabledModules[${index}]`, 'module', moduleId)
    }
    const selectedPreset = database.botPresets?.[database.botPresetsId]
    collector.emit(
        owner,
        '$.botPresetsId',
        'preset',
        selectedPreset?.name ?? `#${database.botPresetsId}`,
        { index: database.botPresetsId },
    )
    const selectedPersona = database.personas?.[database.selectedPersona]
    collector.emit(
        owner,
        '$.selectedPersona',
        'persona',
        selectedPersona?.id ?? `#${database.selectedPersona}`,
        { index: database.selectedPersona },
    )
    collector.optional(
        owner,
        '$.lastLoadedLoadoutName',
        'loadout',
        database.lastLoadedLoadoutName,
    )

    for (const [index, item] of (database.characterOrder ?? []).entries()) {
        if (typeof item === 'string') {
            collector.emit(owner, `$.characterOrder[${index}]`, 'character', item)
            continue
        }
        const folderOwner: ReferenceOwner = { kind: 'folder', id: item.id }
        emitAsset(collector, folderOwner, '$.img', item.img, { field: 'img' })
        emitAsset(collector, folderOwner, '$.imgFile', item.imgFile, { field: 'imgFile' })
        for (const [characterIndex, characterId] of item.data.entries()) {
            collector.emit(
                folderOwner,
                `$.data[${characterIndex}]`,
                'character',
                characterId,
            )
        }
        scanInlays(item, folderOwner, '$', collector)
    }

    const collectionKeys = new Set([
        'characters',
        'botPresets',
        'pluginCustomStorage',
        'modules',
        'personas',
        'loadouts',
        'characterOrder',
    ])
    for (const key of Object.keys(database)) {
        if (collectionKeys.has(key)) continue
        scanInlays(
            (database as unknown as Record<string, unknown>)[key],
            owner,
            propertyPath('$', key),
            collector,
        )
    }
}

function scanDatabaseCollections(
    database: Database,
    collector: GraphCollector,
): void {
    for (const module of database.modules ?? []) {
        scanModule(
            module as unknown as Record<string, unknown>,
            { kind: 'module', id: module.id },
            collector,
        )
    }
    for (const [index, persona] of (database.personas ?? []).entries()) {
        const owner: ReferenceOwner = {
            kind: 'persona',
            id: persona.id ?? `#${index}`,
        }
        emitAsset(collector, owner, '$.icon', persona.icon, { field: 'icon' })
        if (persona.embeddedModule) {
            collector.emit(owner, '$.embeddedModule.id', 'module', persona.embeddedModule.id)
            scanModule(
                persona.embeddedModule as unknown as Record<string, unknown>,
                owner,
                collector,
                '$.embeddedModule',
                false,
            )
        }
        scanInlays(persona, owner, '$', collector)
    }
    for (const [index, preset] of (database.botPresets ?? []).entries()) {
        const owner: ReferenceOwner = {
            kind: 'preset',
            id: preset.name ?? `#${index}`,
        }
        emitAsset(collector, owner, '$.image', preset.image, { field: 'image' })
        scanInlays(preset, owner, '$', collector)
    }
    for (const [key, value] of Object.entries(database.pluginCustomStorage ?? {})) {
        scanInlays(value, { kind: 'plugin-storage', id: key }, '$', collector)
    }
    for (const loadout of database.loadouts ?? []) {
        const owner: ReferenceOwner = { kind: 'loadout', id: loadout.name }
        for (const [index, characterId] of loadout.characterIds.entries()) {
            collector.emit(owner, `$.characterIds[${index}]`, 'character', characterId)
        }
        for (const [index, moduleId] of loadout.modules.entries()) {
            collector.emit(owner, `$.modules[${index}]`, 'module', moduleId)
        }
        collector.emit(owner, '$.presetName', 'preset', loadout.presetName)
        collector.emit(owner, '$.personaId', 'persona', loadout.personaId)
        for (const [index, icon] of (loadout.icons ?? []).entries()) {
            emitRequiredAsset(collector, owner, `$.icons[${index}]`, icon, {
                field: 'icons',
            })
        }
        scanInlays(loadout, owner, '$', collector)
    }
}

function scanCharacters(database: Database, collector: GraphCollector): void {
    for (const character of database.characters) {
        const owner: ReferenceOwner = {
            kind: character.type === 'group' ? 'group' : 'character',
            id: character.chaId,
        }
        scanCharacterAssets(character, owner, '$', collector)
        if (character.type === 'group') {
            for (const [index, characterId] of character.characters.entries()) {
                collector.emit(owner, `$.characters[${index}]`, 'character', characterId)
            }
        }
        for (const [index, moduleId] of (character.modules ?? []).entries()) {
            collector.emit(owner, `$.modules[${index}]`, 'module', moduleId)
        }
        if (character.chats.length > 0) {
            const selectedChat = character.chats[character.chatPage]
            collector.emit(
                owner,
                '$.chatPage',
                'conversation',
                selectedChat?.id ?? `#${character.chatPage}`,
                { characterId: character.chaId, index: character.chatPage },
            )
        }
        collector.optional(owner, '$.coldstorage', 'cold', character.coldstorage)
        for (const [index, coldKey] of (character.coldStoragedChats ?? []).entries()) {
            collector.emit(owner, `$.coldStoragedChats[${index}]`, 'cold', coldKey)
        }
        const { chats, ...characterDetail } = character
        scanInlays(characterDetail, owner, '$', collector)

        for (const [chatIndex, chat] of chats.entries()) {
            const chatOwner: ReferenceOwner = {
                kind: 'conversation',
                id: `${character.chaId}/${chat.id ?? `#${chatIndex}`}`,
            }
            for (const [index, moduleId] of (chat.modules ?? []).entries()) {
                collector.emit(chatOwner, `$.modules[${index}]`, 'module', moduleId)
            }
            collector.optional(chatOwner, '$.bindedPersona', 'persona', chat.bindedPersona)
            collector.optional(chatOwner, '$.folderId', 'folder', chat.folderId, {
                characterId: character.chaId,
            })
            const firstMessageData = chat.message?.[0]?.data
            if (typeof firstMessageData === 'string'
                && firstMessageData.startsWith(coldStorageHeader)) {
                collector.emit(
                    chatOwner,
                    '$.message[0].data',
                    'cold',
                    firstMessageData.slice(coldStorageHeader.length),
                    { encoding: 'cold-storage-header' },
                )
            }
            scanInlays(chat, chatOwner, '$', collector)
        }
    }
}

function scanCards(
    cards: readonly Roadmap14CardFixture[],
    collector: GraphCollector,
): void {
    for (const card of cards) {
        const owner: ReferenceOwner = { kind: 'card', id: card.id }
        collector.emit(owner, '$.characterId', 'character', card.characterId)
        collector.emit(owner, '$.moduleId', 'module', card.moduleId)
        collector.emit(owner, '$.personaId', 'persona', card.personaId)
        collector.emit(owner, '$.folderId', 'folder', card.folderId)
        scanAssetTuples(collector, owner, '$.assetKeys', card.assetKeys)
        for (const [index, cardId] of card.relatedCardIds.entries()) {
            collector.emit(owner, `$.relatedCardIds[${index}]`, 'card', cardId)
        }
        scanInlays(card.metadata, owner, '$.metadata', collector)
    }
}

async function scanColdPayloads(
    payloads: readonly Roadmap14Payload[],
    collector: GraphCollector,
): Promise<void> {
    for (const payload of payloads) {
        if (payload.kind !== 'cold') continue
        const owner: ReferenceOwner = { kind: 'cold', id: payload.key }
        const value = await decodeColdStoragePayload(payload.bytes)
        if (value && typeof value === 'object' && !Array.isArray(value)
            && 'character' in value && value.character
            && typeof value.character === 'object') {
            scanCharacterAssets(
                value.character as Database['characters'][number],
                owner,
                '$.value.character',
                collector,
            )
        }
        scanInlays(value, owner, '$.value', collector)
    }
}

export async function buildReferenceGraph(input: ReferenceGraphInput): Promise<ReferenceEdge[]> {
    const collector = new GraphCollector(createIndexes(input))
    scanDatabaseRoot(input.database, collector)
    scanDatabaseCollections(input.database, collector)
    scanCharacters(input.database, collector)
    scanCards(input.cards, collector)
    await scanColdPayloads(input.coldPayloads, collector)
    return collector.edges
}

export function summarizeReferenceGraph(graph: readonly ReferenceEdge[]): ReferenceGraphSummary {
    return {
        present: graph.filter((edge) => edge.status === 'present'),
        expectedMissing: graph.filter((edge) => edge.status === 'expected-missing'),
        unexpectedMissing: graph.filter((edge) => edge.status === 'unexpected-missing'),
        external: graph.filter((edge) => edge.status === 'external'),
        invalid: graph.filter((edge) => edge.status === 'invalid'),
    }
}

export interface PayloadHashMismatch {
    key: string
    expected: string
    actual: string
}

export interface PayloadInventoryValidation {
    valid: boolean
    missing: string[]
    unexpected: string[]
    mismatches: PayloadHashMismatch[]
    duplicates: Array<{ key: string; count: number }>
    cardinality: { expected: number; actual: number }
}

export function validatePayloadInventory(
    payloads: readonly Roadmap14Payload[],
    expectedHashes: Readonly<Record<string, string>>,
    expectedCount = Object.keys(expectedHashes).length,
): PayloadInventoryValidation {
    const counts = new Map<string, number>()
    for (const payload of payloads) {
        counts.set(payload.key, (counts.get(payload.key) ?? 0) + 1)
    }
    const actual = new Map(
        payloads.map((payload) => [
            payload.key,
            createHash('sha256').update(payload.bytes).digest('hex'),
        ]),
    )
    const missing = Object.keys(expectedHashes).filter((key) => !actual.has(key))
    const unexpected = payloads
        .map((payload) => payload.key)
        .filter((key) => !(key in expectedHashes))
    const mismatches = Object.entries(expectedHashes).flatMap(([key, expected]) => {
        const actualHash = actual.get(key)
        return actualHash !== undefined && actualHash !== expected
            ? [{ key, expected, actual: actualHash }]
            : []
    })
    const duplicates = [...counts.entries()]
        .filter(([, count]) => count > 1)
        .map(([key, count]) => ({ key, count }))
    const cardinality = { expected: expectedCount, actual: payloads.length }
    return {
        valid: missing.length === 0
            && unexpected.length === 0
            && mismatches.length === 0
            && duplicates.length === 0
            && cardinality.actual === cardinality.expected,
        missing,
        unexpected,
        mismatches,
        duplicates,
        cardinality,
    }
}
