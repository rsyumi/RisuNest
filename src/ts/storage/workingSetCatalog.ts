import type { Database, character, groupChat } from './database.svelte'
import { createConversationSummaryStubFromChat } from './conversationResidency'
import type {
    CharacterSummary,
    CharacterDetail,
    PersistentRoot,
    PresetCatalog,
    PresetSummary,
} from './persistentDataStore'
import type { WorkingSetResidencyRegistry } from './workingSetResidency'

type CompleteCharacter = character | groupChat

const catalogCharacterMetadata = Symbol('catalogCharacterMetadata')
const catalogPresetMetadata = Symbol('catalogPresetMetadata')

export interface CatalogCharacterMetadata {
    configuredIndex: number
    conversationCount: number
    residency: 'catalog' | 'detail'
}

type CatalogCharacter = CompleteCharacter & {
    [catalogCharacterMetadata]?: CatalogCharacterMetadata
}

export interface CatalogPresetMetadata {
    activeConfiguredIndex: number | null
    catalogRevision: number
    residency: 'selected-only'
}

type CatalogPresetWorkingSet = Database['botPresets'] & {
    [catalogPresetMetadata]?: CatalogPresetMetadata
}

export interface ActiveCatalogPreset {
    summary: PresetSummary
    value: Database['botPresets'][number]
}

export function createCatalogCharacterStub(summary: CharacterSummary): CompleteCharacter {
    const stub = {
        chaId: summary.id,
        name: summary.name,
        type: summary.type ?? 'character',
        chats: [],
        lastInteraction: summary.recentAt,
        ...(summary.image === undefined ? {} : { image: summary.image }),
        ...(summary.creatorNotes === undefined ? {} : { creatorNotes: summary.creatorNotes }),
        ...(summary.trashTime === undefined ? {} : { trashTime: summary.trashTime }),
    } as CompleteCharacter

    Object.defineProperty(stub, catalogCharacterMetadata, {
        configurable: false,
        enumerable: false,
        value: {
            configuredIndex: summary.configuredIndex,
            conversationCount: summary.conversationCount,
            residency: 'catalog',
        } satisfies CatalogCharacterMetadata,
        writable: false,
    })
    return stub
}

export function getCatalogCharacterMetadata(
    value: CompleteCharacter,
): CatalogCharacterMetadata | undefined {
    return (value as CatalogCharacter)[catalogCharacterMetadata]
}

export function isCatalogCharacterStub(value: CompleteCharacter): boolean {
    return getCatalogCharacterMetadata(value)?.residency === 'catalog'
}

export function getCatalogConversationCount(value: CompleteCharacter): number {
    return getCatalogCharacterMetadata(value)?.conversationCount ?? value.chats.length
}

const catalogCharacterFields = [
    'chaId',
    'name',
    'type',
    'image',
    'creatorNotes',
    'trashTime',
    'lastInteraction',
] as const

export function patchWorkingSetCharacterDetail(
    target: CompleteCharacter,
    detail: CharacterDetail,
): void {
    const targetRecord = target as unknown as Record<string, unknown>
    const detailRecord = detail as unknown as Record<string, unknown>
    if (isCatalogCharacterStub(target)) {
        for (const key of catalogCharacterFields) {
            if (Object.hasOwn(detailRecord, key) && detailRecord[key] !== undefined) {
                targetRecord[key] = detailRecord[key]
            } else {
                delete targetRecord[key]
            }
        }
        return
    }
    for (const key of Object.keys(targetRecord)) {
        if (key !== 'chats' && !Object.hasOwn(detailRecord, key)) delete targetRecord[key]
    }
    Object.assign(targetRecord, detailRecord)
}

export function hydrateWorkingSetCharacterDetail(
    database: Pick<Database, 'characters'>,
    index: number,
    detail: CharacterDetail,
): CompleteCharacter {
    const target = database.characters[index]
    const hydrated = {
        ...detail,
        chats: target.chats,
    } as CompleteCharacter
    const metadata = getCatalogCharacterMetadata(target)
    if (metadata) {
        Object.defineProperty(hydrated, catalogCharacterMetadata, {
            configurable: false,
            enumerable: false,
            value: {
                ...metadata,
                residency: 'detail',
            } satisfies CatalogCharacterMetadata,
            writable: false,
        })
    }
    database.characters[index] = hydrated
    return hydrated
}

export function createCatalogPresetWorkingSet(
    catalog: PresetCatalog,
    active: ActiveCatalogPreset | null,
): Database['botPresets'] {
    const presets: Database['botPresets'] = []
    for (const summary of catalog.items) {
        presets[summary.configuredIndex] = {
            name: summary.name,
            ...(summary.image === undefined ? {} : { image: summary.image }),
        } as Database['botPresets'][number]
    }
    if (active) presets[active.summary.configuredIndex] = active.value

    Object.defineProperty(presets, catalogPresetMetadata, {
        configurable: false,
        enumerable: false,
        value: {
            activeConfiguredIndex: active?.summary.configuredIndex ?? null,
            catalogRevision: catalog.revision,
            residency: 'selected-only',
        } satisfies CatalogPresetMetadata,
        writable: false,
    })
    return presets
}

export function getCatalogPresetMetadata(
    presets: Database['botPresets'],
): CatalogPresetMetadata | undefined {
    return (presets as CatalogPresetWorkingSet)[catalogPresetMetadata]
}

export function isCatalogPresetWorkingSet(presets: Database['botPresets']): boolean {
    if (!presets) return false
    return getCatalogPresetMetadata(presets)?.residency === 'selected-only'
}

export function hasIncompletePersistentWorkingSet(
    database: Pick<Database, 'characters' | 'botPresets'>,
    residency?: Pick<
        WorkingSetResidencyRegistry,
        'isCharacterReleased' | 'hasReleasedConversations'
    >,
): boolean {
    if (isCatalogPresetWorkingSet(database.botPresets)) return true
    return database.characters.some((character) => (
        isCatalogCharacterStub(character) ||
        residency?.isCharacterReleased(character.chaId) === true ||
        residency?.hasReleasedConversations(character.chaId) === true
    ))
}

export function projectCatalogWorkingSet(
    root: PersistentRoot,
    summaries: readonly CharacterSummary[],
    presets: Database['botPresets'],
): Database {
    const characters = [...summaries]
        .sort((left, right) => left.configuredIndex - right.configuredIndex)
        .map(createCatalogCharacterStub)
    return {
        ...root,
        pluginCustomStorage: {},
        botPresets: presets,
        characters,
    } as Database
}

export function projectCompleteScalableWorkingSet(
    database: Database,
    selectedCharacterId: string | null,
    revision: number,
    activeCharacterIds?: ReadonlySet<string>,
    selectedConversationId?: string | null,
): Database {
    const complete = structuredClone(database)
    const { characters, botPresets, pluginCustomStorage: _pluginCustomStorage, ...root } = complete
    const summaries: CharacterSummary[] = characters.map((character, configuredIndex) => ({
        id: character.chaId,
        name: character.name,
        image: character.image,
        configuredIndex,
        recentAt: character.lastInteraction ?? 0,
        trashed: character.trashTime !== undefined,
        conversationCount: character.chats.length,
        type: character.type,
        creatorNotes: character.creatorNotes ?? '',
        trashTime: character.trashTime,
    }))
    const presetCatalog: PresetCatalog = {
        revision,
        items: botPresets.map((preset, configuredIndex) => ({
            id: String(configuredIndex),
            configuredIndex,
            name: preset.name ?? '',
            image: preset.image,
        })),
    }
    const activeSummary = presetCatalog.items.find(
        (summary) => summary.configuredIndex === root.botPresetsId,
    )
    const projected = projectCatalogWorkingSet(
        root,
        summaries,
        createCatalogPresetWorkingSet(
            presetCatalog,
            activeSummary ? {
                summary: activeSummary,
                value: botPresets[activeSummary.configuredIndex],
            } : null,
        ),
    )
    const residentIds = new Set(activeCharacterIds)
    if (selectedCharacterId) residentIds.add(selectedCharacterId)
    for (let index = 0; index < characters.length; index++) {
        if (residentIds.has(characters[index].chaId)) {
            const character = characters[index]
            const pinnedConversationId = character.chaId === selectedCharacterId
                ? selectedConversationId ?? character.chats[character.chatPage ?? 0]?.id
                : undefined
            projected.characters[index] = {
                ...character,
                chats: character.chats.map((conversation, conversationIndex) =>
                    conversation.id === pinnedConversationId
                        ? conversation
                        : createConversationSummaryStubFromChat(
                            character.chaId,
                            conversation,
                            conversationIndex,
                        ),
                ),
            } as typeof character
        }
    }
    return projected
}
