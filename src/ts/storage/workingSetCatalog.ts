import type { Chat, Database, character, groupChat } from './database.svelte'
import {
    createConversationSummaryStub,
    createConversationSummaryStubFromChat,
} from './conversationResidency'
import type {
    CharacterSummary,
    CharacterDetail,
    ConversationSummary,
    PersistentDataStore,
    PersistentRoot,
    PersistentRevisionReader,
    PresetCatalog,
    PresetSummary,
} from './persistentDataStore'
import {
    assertPinnedRevision,
    iteratePinnedCharacters,
    iteratePinnedCharacterSummaries,
    iteratePinnedConversations,
    releasePersistentRevisionLease,
} from './persistentRecordIterator'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import type { WorkingSetResidencyRegistry } from './workingSetResidency'

type CompleteCharacter = character | groupChat

const PROFILE_CONVERSATION_PAGE_SIZE = 128

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

export function createPresetCatalogWorkingSetFromValues(
    presets: Database['botPresets'],
    revision: number,
    activeConfiguredIndex: number | undefined,
): Database['botPresets'] {
    const catalog: PresetCatalog = {
        revision,
        items: presets.map((preset, configuredIndex) => ({
            id: String(configuredIndex),
            configuredIndex,
            name: preset.name ?? '',
            image: preset.image,
        })),
    }
    const activeSummary = catalog.items.find(
        (summary) => summary.configuredIndex === activeConfiguredIndex,
    )
    return createCatalogPresetWorkingSet(
        catalog,
        activeSummary ? {
            summary: activeSummary,
            value: { ...presets[activeSummary.configuredIndex] },
        } : null,
    )
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

async function readPinnedPresets(
    reader: PersistentRevisionReader,
): Promise<{ catalog: PresetCatalog; values: Database['botPresets'] }> {
    const catalog = await reader.queryPresets()
    assertPinnedRevision(reader.revision, catalog.revision, 'Preset catalog')
    const values: Database['botPresets'] = []
    for (const summary of catalog.items) {
        const preset = await reader.readPreset(summary.id)
        if (!preset) throw new Error(`Missing preset ${summary.id}`)
        assertPinnedRevision(reader.revision, preset.revision, `Preset ${summary.id}`)
        values[summary.configuredIndex] = preset.value
    }
    return { catalog, values }
}

async function readPinnedActivePreset(
    reader: PersistentRevisionReader,
    configuredIndex: number,
): Promise<{ catalog: PresetCatalog; active: ActiveCatalogPreset | null }> {
    const catalog = await reader.queryPresets()
    assertPinnedRevision(reader.revision, catalog.revision, 'Preset catalog')
    const summary = catalog.items.find((candidate) =>
        candidate.configuredIndex === configuredIndex)
    if (!summary) return { catalog, active: null }
    const preset = await reader.readPreset(summary.id)
    if (!preset) throw new Error(`Missing preset ${summary.id}`)
    assertPinnedRevision(reader.revision, preset.revision, `Preset ${summary.id}`)
    return {
        catalog,
        active: {
            summary,
            value: preset.value,
        },
    }
}

async function readPinnedPluginStorage(
    reader: PersistentRevisionReader,
): Promise<Database['pluginCustomStorage']> {
    const catalog = await reader.queryPluginStorage()
    assertPinnedRevision(reader.revision, catalog.revision, 'Plugin storage catalog')
    const values: Database['pluginCustomStorage'] = {}
    for (const summary of catalog.items) {
        const value = await reader.readPluginStorage(summary.key)
        if (!value) throw new Error(`Missing plugin storage value for ${summary.key}`)
        assertPinnedRevision(
            reader.revision,
            value.revision,
            `Plugin storage value ${summary.key}`,
        )
        defineOwnEnumerableProperty(values, summary.key, value.value)
    }
    return values
}

export async function materializePinnedCompatibilityDatabase(
    reader: PersistentRevisionReader,
): Promise<Database> {
    const root = await reader.readRoot()
    assertPinnedRevision(reader.revision, root.revision, 'Root')
    const { values: botPresets } = await readPinnedPresets(reader)
    const pluginCustomStorage = await readPinnedPluginStorage(reader)
    const characters: Database['characters'] = []
    for await (const record of iteratePinnedCharacters(reader)) {
        const chats: Chat[] = []
        for await (const conversation of iteratePinnedConversations(reader, record.summary.id)) {
            chats.push(conversation.value)
        }
        characters.push({ ...record.detail, chats } as CompleteCharacter)
    }
    return {
        ...root.value,
        botPresets,
        pluginCustomStorage,
        characters,
    } as Database
}

export interface PinnedScalableWorkingSetOptions {
    selectedCharacterId: string | null
    selectedConversationId?: string | null
    activeCharacterIds?: ReadonlySet<string>
}

async function readPinnedConversationSummaries(
    reader: PersistentRevisionReader,
    characterId: string,
): Promise<ConversationSummary[]> {
    const summaries: ConversationSummary[] = []
    let cursor: string | undefined
    do {
        const page = await reader.queryConversations({
            characterId,
            order: 'configured',
            limit: PROFILE_CONVERSATION_PAGE_SIZE,
            cursor,
        })
        assertPinnedRevision(
            reader.revision,
            page.revision,
            `Conversation page for ${characterId}`,
        )
        for (const summary of page.items) {
            if (summary.characterId !== characterId) {
                throw new Error(`Conversation ${summary.id} returned mismatched character ID`)
            }
            summaries.push(summary)
        }
        cursor = page.nextCursor
    } while (cursor !== undefined)
    return summaries
}

async function createPinnedResidentCharacter(
    reader: PersistentRevisionReader,
    summary: CharacterSummary,
    detail: CharacterDetail,
    selectedConversationId?: string | null,
): Promise<CompleteCharacter> {
    const summaries = await readPinnedConversationSummaries(reader, summary.id)
    const chats = summaries.map(createConversationSummaryStub)
    let selectedIndex = -1
    if (selectedConversationId) {
        selectedIndex = summaries.findIndex((conversation) =>
            conversation.id === selectedConversationId)
    }
    if (selectedIndex < 0) {
        selectedIndex = summaries.findIndex((conversation) =>
            conversation.configuredIndex === (detail.chatPage ?? 0))
    }
    if (selectedIndex < 0 && summaries.length > 0) {
        selectedIndex = 0
    }
    if (selectedIndex >= 0) {
        const selected = summaries[selectedIndex]
        const value = await reader.readConversation(summary.id, selected.id)
        if (!value) throw new Error(`Missing conversation ${selected.id}`)
        assertPinnedRevision(reader.revision, value.revision, `Conversation ${selected.id}`)
        if (value.value.id !== selected.id) {
            throw new Error(`Conversation ${selected.id} returned mismatched ID`)
        }
        chats[selectedIndex] = value.value
    }
    const character = createPinnedDetailOnlyCharacter(summary, detail)
    character.chats = chats
    character.chatPage = selectedIndex < 0 ? 0 : selectedIndex
    return character
}

function createPinnedDetailOnlyCharacter(
    summary: CharacterSummary,
    detail: CharacterDetail,
): CompleteCharacter {
    const database = {
        characters: [createCatalogCharacterStub(summary)],
    }
    return hydrateWorkingSetCharacterDetail(database, 0, detail)
}

export async function projectPinnedScalableWorkingSet(
    reader: PersistentRevisionReader,
    options: PinnedScalableWorkingSetOptions,
): Promise<Database> {
    const root = await reader.readRoot()
    assertPinnedRevision(reader.revision, root.revision, 'Root')
    const presets = await readPinnedActivePreset(reader, root.value.botPresetsId)
    const residentIds = new Set(options.activeCharacterIds)
    if (options.selectedCharacterId) residentIds.add(options.selectedCharacterId)
    let selectedDetail: CharacterDetail | null = null
    if (options.selectedCharacterId) {
        const value = await reader.readCharacter(options.selectedCharacterId)
        if (value) {
            assertPinnedRevision(
                reader.revision,
                value.revision,
                `Character ${options.selectedCharacterId}`,
            )
            selectedDetail = value.value
            if (value.value.type === 'group') {
                for (const id of value.value.characters) residentIds.add(id)
            }
        }
    }

    const characters: Database['characters'] = []
    for await (const summary of iteratePinnedCharacterSummaries(reader)) {
        if (!residentIds.has(summary.id)) {
            characters.push(createCatalogCharacterStub(summary))
            continue
        }
        let detail = summary.id === options.selectedCharacterId ? selectedDetail : null
        if (!detail) {
            const value = await reader.readCharacter(summary.id)
            if (!value) throw new Error(`Missing character detail for ${summary.id}`)
            assertPinnedRevision(reader.revision, value.revision, `Character ${summary.id}`)
            detail = value.value
        }
        characters.push(summary.id === options.selectedCharacterId
            ? await createPinnedResidentCharacter(
                reader,
                summary,
                detail,
                options.selectedConversationId,
            )
            : createPinnedDetailOnlyCharacter(summary, detail))
    }

    return {
        ...root.value,
        pluginCustomStorage: {},
        botPresets: createCatalogPresetWorkingSet(
            presets.catalog,
            presets.active,
        ),
        characters,
    } as Database
}

export async function projectScalableWorkingSetAtRevision(
    store: Pick<PersistentDataStore, 'acquireRevision'>,
    revision: number,
    options: PinnedScalableWorkingSetOptions,
): Promise<Database> {
    const lease = await store.acquireRevision(revision)
    let primaryError: unknown
    try {
        assertPinnedRevision(revision, lease.revision, 'Revision lease')
        return await projectPinnedScalableWorkingSet(lease, options)
    } catch (error) {
        primaryError = error
        throw error
    } finally {
        try {
            await releasePersistentRevisionLease(lease)
        } catch (error) {
            if (primaryError === undefined) throw error
        }
    }
}

export function projectCompleteScalableWorkingSet(
    database: Database,
    selectedCharacterId: string | null,
    revision: number,
    activeCharacterIds?: ReadonlySet<string>,
    selectedConversationId?: string | null,
): Database {
    const { characters, botPresets, pluginCustomStorage: _pluginCustomStorage, ...root } = database
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
    const projected = projectCatalogWorkingSet(
        root,
        summaries,
        createPresetCatalogWorkingSetFromValues(botPresets, revision, root.botPresetsId),
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
                        ? { ...conversation, message: [...conversation.message] }
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
