import { isTauri } from '../platform'
import isEqual from 'lodash/isEqual'
import type { Chat, Database, character } from './database.svelte'
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
import { projectSelectionIndexes } from './persistentSelectionBoundary'
import type { WorkingSetResidencyRegistry } from './workingSetResidency'
import { getResidentPresetIds } from './persistentIdentityHooks'

type CompleteCharacter = character

export function patchWorkingSetRoot(target: Database, root: PersistentRoot): void {
    const targetRecord = target as unknown as Record<string, unknown>
    const rootRecord = root as unknown as Record<string, unknown>
    for (const key of Object.keys(targetRecord)) {
        if ((isTauri && key === 'account') || key === 'characters' || key === 'botPresets' ||
            key === 'pluginCustomStorage' || key === 'pluginStorageMeta') continue
        if (!Object.hasOwn(rootRecord, key)) delete targetRecord[key]
    }
    for (const key of Object.keys(rootRecord)) {
        if (isTauri && key === 'account') continue
        let value = rootRecord[key]
        if (key === 'botPresetsId' && typeof value === 'string') value = Math.max(0, target.botPresets.findIndex((preset) => preset?.['id'] === value))
        if (key === 'selectedPersona' && typeof value === 'string') value = Math.max(0, target.personas?.findIndex((persona) => persona.id === value) ?? 0)
        if (!isEqual(targetRecord[key], value)) targetRecord[key] = value
    }
}

const PROFILE_CONVERSATION_PAGE_SIZE = 128

const catalogCharacterMetadata = Symbol('catalogCharacterMetadata')
const catalogPresetMetadata = Symbol('catalogPresetMetadata')

export interface CatalogCharacterMetadata {
    configuredIndex: number
    conversationCount: number
    residency: 'catalog' | 'detail' | 'archived'
    archivedAt?: number
    archivedMessageCount?: number
}

type CatalogCharacter = CompleteCharacter & {
    [catalogCharacterMetadata]?: CatalogCharacterMetadata
}

export interface CatalogPresetMetadata {
    activeConfiguredIndex: number | null
    catalogRevision: number
    residency: 'selected-only'
    /** Presets whose stored body the working set holds; every other entry is a summary. */
    loaded: Set<string>
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
            conversationCount: summary.archived?.conversationCount ?? summary.conversationCount,
            residency: summary.archived ? 'archived' : 'catalog',
            ...(summary.archived === undefined
                ? {}
                : {
                      archivedAt: summary.archived.archivedAt,
                      archivedMessageCount: summary.archived.messageCount,
                  }),
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

export function carryCatalogCharacterMetadata(previous: CompleteCharacter, replacement: CompleteCharacter): CompleteCharacter {
    const metadata = getCatalogCharacterMetadata(previous)
    if (metadata && !getCatalogCharacterMetadata(replacement)) {
        Object.defineProperty(replacement, catalogCharacterMetadata, {
            enumerable: false,
            value: { ...metadata, residency: 'detail', conversationCount: replacement.chats.length },
        })
    }
    return replacement
}

export function isCatalogCharacterStub(value: CompleteCharacter): boolean {
    return getCatalogCharacterMetadata(value)?.residency === 'catalog'
}

/// An archived character carries a stub that must never be hydrated.
export function isArchivedCharacter(value: CompleteCharacter): boolean {
    return getCatalogCharacterMetadata(value)?.residency === 'archived'
}

/// Both stub kinds, for callers that must not treat a stub as a complete value.
export function isWorkingSetCharacterStub(value: CompleteCharacter): boolean {
    const residency = getCatalogCharacterMetadata(value)?.residency
    return residency === 'catalog' || residency === 'archived'
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
const catalogCharacterFieldSet: ReadonlySet<string> = new Set(catalogCharacterFields)

/// Copies one stored field into a stub when the stub shows it; other fields stay out of the stub.
export function patchCatalogCharacterField(
    target: CompleteCharacter,
    detail: CharacterDetail | CompleteCharacter,
    field: string,
): void {
    if (!catalogCharacterFieldSet.has(field)) return
    const targetRecord = target as unknown as Record<string, unknown>
    const detailRecord = detail as unknown as Record<string, unknown>
    if (Object.hasOwn(detailRecord, field) && detailRecord[field] !== undefined) {
        targetRecord[field] = detailRecord[field]
    } else {
        delete targetRecord[field]
    }
}

export function patchWorkingSetCharacterDetail(
    target: CompleteCharacter,
    detail: CharacterDetail,
): void {
    const targetRecord = target as unknown as Record<string, unknown>
    const detailRecord = detail as unknown as Record<string, unknown>
    if (isCatalogCharacterStub(target)) {
        for (const key of catalogCharacterFields) patchCatalogCharacterField(target, detail, key)
        return
    }
    for (const key of Object.keys(targetRecord)) {
        if (key !== 'chats' && !Object.hasOwn(detailRecord, key)) delete targetRecord[key]
    }
    // Keep unchanged values so views holding them are not rebuilt.
    for (const key of Object.keys(detailRecord)) {
        if (!isEqual(targetRecord[key], detailRecord[key])) targetRecord[key] = detailRecord[key]
    }
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
    resident: readonly ActiveCatalogPreset[] = [],
): Database['botPresets'] {
    const presets: Database['botPresets'] = []
    for (const summary of catalog.items) {
        presets[summary.configuredIndex] = {
            id: summary.id,
            name: summary.name,
            ...(summary.image === undefined ? {} : { image: summary.image }),
        } as unknown as Database['botPresets'][number]
    }
    const bodies = active ? [...resident, active] : resident
    for (const body of bodies) presets[body.summary.configuredIndex] = body.value

    Object.defineProperty(presets, catalogPresetMetadata, {
        configurable: false,
        enumerable: false,
        value: {
            activeConfiguredIndex: active?.summary.configuredIndex ?? null,
            catalogRevision: catalog.revision,
            residency: 'selected-only',
            loaded: new Set(bodies.map((body) => body.summary.id)),
        } satisfies CatalogPresetMetadata,
        writable: false,
    })
    return presets
}

/**
 * The summary of the preset `selection` names. An ID naming no preset resolves to the first,
 * as the working set's index does.
 */
export function selectedPresetSummary(catalog: PresetCatalog, selection: number | string | undefined): PresetSummary | undefined {
    if (typeof selection !== 'string') return catalog.items.find((summary) => summary.configuredIndex === selection)
    return catalog.items.find((summary) => summary.id === selection) ?? catalog.items[0]
}

/** A catalog working set of complete preset values, holding the bodies of the selection and of the presets applied in its place. */
export function createPresetCatalogWorkingSetFromValues(
    presets: Database['botPresets'],
    revision: number,
    selection: number | string | undefined,
): Database['botPresets'] {
    const catalog: PresetCatalog = {
        revision,
        items: presets.map((preset, configuredIndex) => ({
            id: preset['id'] as string,
            configuredIndex,
            name: preset.name ?? '',
            image: preset.image,
        })),
    }
    const activeSummary = selectedPresetSummary(catalog, selection)
    const resident = new Set(getResidentPresetIds())
    const body = (summary: PresetSummary): ActiveCatalogPreset => ({ summary, value: { ...presets[summary.configuredIndex] } })
    return createCatalogPresetWorkingSet(
        catalog,
        activeSummary ? body(activeSummary) : null,
        catalog.items.filter((summary) => summary !== activeSummary && resident.has(summary.id)).map(body),
    )
}

export function getCatalogPresetMetadata(
    presets: Database['botPresets'],
): CatalogPresetMetadata | undefined {
    return (presets as CatalogPresetWorkingSet)[catalogPresetMetadata]
}

export function advanceCatalogPresetWorkingSetRevision(
    presets: Database['botPresets'],
    revision: number,
): Database['botPresets'] {
    const metadata = getCatalogPresetMetadata(presets)
    if (!metadata) throw new Error('Cannot advance a non-catalog preset working set')
    const advanced = presets.slice()
    Object.defineProperty(advanced, catalogPresetMetadata, {
        configurable: false,
        enumerable: false,
        value: { ...metadata, catalogRevision: revision, loaded: new Set(metadata.loaded) } satisfies CatalogPresetMetadata,
        writable: false,
    })
    return advanced
}

export function isCatalogPresetWorkingSet(presets: Database['botPresets']): boolean {
    if (!presets) return false
    return getCatalogPresetMetadata(presets)?.residency === 'selected-only'
}

/** Whether a working set holds a preset's stored body; only a catalog working set holds summaries. */
export function isCatalogPresetLoaded(presets: Database['botPresets'], id: string): boolean {
    return getCatalogPresetMetadata(presets)?.loaded.has(id) ?? true
}

/** Whether a preset applied in place of the selection is a summary in `presets`. */
export function lacksResidentPresetBody(presets: Database['botPresets']): boolean {
    return getResidentPresetIds().some((id) => presets.some((preset) => preset?.['id'] === id) && !isCatalogPresetLoaded(presets, id))
}

/** Places a preset's stored body, a new object, in a working set. */
export function placeCatalogPresetBody(presets: Database['botPresets'], index: number, value: Database['botPresets'][number]): void {
    getCatalogPresetMetadata(presets)?.loaded.add(value.id!)
    presets[index] = value
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
        botPresetsId: typeof root.botPresetsId === 'string' ? Math.max(0, presets.findIndex((preset) => preset?.['id'] === root.botPresetsId)) : root.botPresetsId,
        selectedPersona: typeof root.selectedPersona === 'string' ? Math.max(0, root.personas?.findIndex((persona) => persona.id === root.selectedPersona) ?? 0) : root.selectedPersona,
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

/** Reads the preset catalog with the body of the selected preset and of the presets applied in its place. */
export async function readPinnedActivePreset(
    reader: PersistentRevisionReader,
    configuredIndex: number | string,
): Promise<{ catalog: PresetCatalog; active: ActiveCatalogPreset | null; resident: ActiveCatalogPreset[] }> {
    const catalog = await reader.queryPresets()
    assertPinnedRevision(reader.revision, catalog.revision, 'Preset catalog')
    const read = async (summary: PresetSummary): Promise<ActiveCatalogPreset> => {
        const preset = await reader.readPreset(summary.id)
        if (!preset) throw new Error(`Missing preset ${summary.id}`)
        assertPinnedRevision(reader.revision, preset.revision, `Preset ${summary.id}`)
        return { summary, value: preset.value }
    }
    const summary = selectedPresetSummary(catalog, configuredIndex)
    const active = summary ? await read(summary) : null
    const resident: ActiveCatalogPreset[] = []
    // Reads until a pass adds none, so a preset applied during a read stays loaded too.
    for (let added = true; added;) {
        const ids = new Set(getResidentPresetIds())
        const next = catalog.items.filter((item) => item !== summary && ids.has(item.id) && !resident.some((body) => body.summary === item))
        for (const item of next) resident.push(await read(item))
        added = next.length > 0
    }
    return { catalog, active, resident }
}

async function readPinnedPluginStorage(
    reader: PersistentRevisionReader,
): Promise<Database['pluginCustomStorage']> {
    const catalog = await reader.queryPluginStorage()
    assertPinnedRevision(reader.revision, catalog.revision, 'Plugin storage catalog')
    const values: Database['pluginCustomStorage'] = {}
    for (const summary of catalog.items) {
        const value = await reader.readPluginStorage(summary.owner, summary.key)
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
    keepConversation?: (conversationId: string) => Chat | null
    selectedCharacterId: string | null
    selectedConversationId?: string | null
    activeCharacterIds?: ReadonlySet<string>
    /** Applies the root preparation boot gives a stored root before it enters the working set. */
    prepareRoot?: (root: PersistentRoot) => Promise<PersistentRoot>
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

export async function createPinnedResidentCharacter(
    reader: PersistentRevisionReader,
    summary: CharacterSummary,
    detail: CharacterDetail,
    selectedConversationId?: string | null,
    keepConversation?: (conversationId: string) => Chat | null,
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
        const kept = keepConversation?.(selected.id) ?? null
        if (kept) {
            chats[selectedIndex] = kept
        } else {
            const value = await reader.readConversation(summary.id, selected.id)
            if (!value) throw new Error(`Missing conversation ${selected.id}`)
            assertPinnedRevision(reader.revision, value.revision, `Conversation ${selected.id}`)
            if (value.value.id !== selected.id) {
                throw new Error(`Conversation ${selected.id} returned mismatched ID`)
            }
            chats[selectedIndex] = value.value
        }
    }
    const character = createPinnedDetailOnlyCharacter(summary, detail)
    character.chats = chats
    character.chatPage = selectedIndex < 0 ? 0 : selectedIndex
    return character
}

export function createPinnedDetailOnlyCharacter(
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
    const rootValue = options.prepareRoot ? await options.prepareRoot(root.value) : root.value
    const residentIds = new Set(options.activeCharacterIds)
    if (options.selectedCharacterId) residentIds.add(options.selectedCharacterId)
    let selectedDetail: CharacterDetail | null = null
    if (options.selectedCharacterId) {
        // An archived character has no detail to read, so the selection keeps its
        // stub instead of failing the whole projection.
        const selectedSummary = await reader.readCharacterSummary(options.selectedCharacterId)
        if (selectedSummary && !selectedSummary.archived) {
            const value = await reader.readCharacter(options.selectedCharacterId)
            if (value) {
                assertPinnedRevision(
                    reader.revision,
                    value.revision,
                    `Character ${options.selectedCharacterId}`,
                )
                selectedDetail = value.value

            }
        }
    }

    const characters: Database['characters'] = []
    for await (const summary of iteratePinnedCharacterSummaries(reader)) {
        if (summary.archived || !residentIds.has(summary.id)) {
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
                options.keepConversation,
            )
            : createPinnedDetailOnlyCharacter(summary, detail))
    }

    // Read last, so a preset applied while the characters were read stays loaded.
    const presets = await readPinnedActivePreset(reader, rootValue.botPresetsId)
    return projectSelectionIndexes({
        ...rootValue,
        pluginCustomStorage: {},
        botPresets: createCatalogPresetWorkingSet(
            presets.catalog,
            presets.active,
            presets.resident,
        ),
        characters,
    } as Database)
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
    const {
        characters,
        botPresets,
        pluginCustomStorage: _pluginCustomStorage,
        pluginStorageMeta: _pluginStorageMeta,
        ...root
    } = database
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
