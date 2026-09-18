import type { Chat, Database, character, groupChat } from './database.svelte'
import type { ReplacementChangeSet } from './persistentDataRuntime'
import {
    CONTENT_CHANGE_PAGE_LIMIT,
    type CharacterDetail,
    type CharacterSummary,
    type ContentChangeKey,
    type PersistentRevisionReader,
} from './persistentDataStore'
import { assertPinnedRevision } from './persistentRecordIterator'
import {
    createCatalogCharacterStub,
    advanceCatalogPresetWorkingSetRevision,
    createCatalogPresetWorkingSet,
    createPinnedDetailOnlyCharacter,
    createPinnedResidentCharacter,
    getCatalogCharacterMetadata,
    getCatalogPresetMetadata,
    isCatalogPresetWorkingSet,
    readPinnedActivePreset,
    type PinnedScalableWorkingSetOptions,
} from './workingSetCatalog'

type CompleteCharacter = character | groupChat

/// Past this many locators a reprojection reads less than the targeted pass.
export const TARGETED_INVALIDATION_KEY_LIMIT = 256

export interface DeferredConversationTarget {
    characterId: string
    conversationId: string
}

export interface TargetedWorkingSetInvalidationOptions
    extends PinnedScalableWorkingSetOptions {
    /** The conversation a reply is being generated into, if any. */
    deferredConversation?: DeferredConversationTarget | null
    changeSet?: ReplacementChangeSet
    onPluginStorageChanged?(owner: string, key: string): void
}

export interface TargetedWorkingSetInvalidationResult {
    database: Database
    /** A change was held back from a generating conversation. */
    deferred: boolean
}

const ROOT_OWNER_KINDS = new Set(['root-module-assets', 'persona-embedded-module-assets'])

interface ChangePlan {
    root: boolean
    presets: boolean
    characterIds: Set<string>
    deferredConversation: boolean
}

/// The indirect effect of every locator kind. Anything absent from this
/// dispatch gives up and lets the caller reproject the whole working set.
function planChanges(
    keys: readonly ContentChangeKey[],
    options: TargetedWorkingSetInvalidationOptions,
): ChangePlan | null {
    const changes = options.changeSet
    if (changes?.wholeLibrary) return null
    if (changes && changes.characterIds.length + changes.conversations.length > TARGETED_INVALIDATION_KEY_LIMIT) {
        return null
    }
    const plan: ChangePlan = {
        root: changes?.root ?? false,
        presets: changes?.presets ?? false,
        characterIds: new Set(changes?.characterIds),
        deferredConversation: false,
    }
    const deferral = options.deferredConversation ?? null
    for (const conversation of changes?.conversations ?? []) {
        plan.characterIds.add(conversation.characterId)
        if (deferral?.characterId === conversation.characterId &&
            deferral.conversationId === conversation.conversationId) {
            plan.deferredConversation = true
        }
    }
    for (const key of keys) {
        switch (key.kind) {
            case 'root':
                plan.root = true
                break
            case 'preset':
                plan.presets = true
                break
            case 'character':
                plan.characterIds.add(key.key1)
                break
            case 'conversation':
                plan.characterIds.add(key.key1)
                if (
                    deferral !== null &&
                    deferral.characterId === key.key1 &&
                    deferral.conversationId === key.key2
                ) {
                    plan.deferredConversation = true
                }
                break
            case 'plugin':
                // The scalable working set carries no plugin values, so only the
                // host cache outside it has to be dropped.
                options.onPluginStorageChanged?.(key.key1, key.key2)
                break
            case 'asset':
            case 'inlay':
                // Alias reads reach the store on every call, so there is no
                // WebView cache for a reference change to drop.
                break
            case 'owner':
                if (key.key1 === 'character-additional-assets') {
                    plan.characterIds.add(key.key2)
                    break
                }
                if (ROOT_OWNER_KINDS.has(key.key1)) {
                    plan.root = true
                    break
                }
                return null
            default:
                return null
        }
    }
    return plan
}

function characterIsTrashed(value: CompleteCharacter): boolean {
    return value.trashTime !== undefined
}

/// The catalog order a full projection walks: configured position first, and an
/// active character ahead of a trashed one that shares it.
function compareCatalogPosition(left: CompleteCharacter, right: CompleteCharacter): number {
    const leftIndex = getCatalogCharacterMetadata(left)?.configuredIndex ?? 0
    const rightIndex = getCatalogCharacterMetadata(right)?.configuredIndex ?? 0
    if (leftIndex !== rightIndex) return leftIndex - rightIndex
    return Number(characterIsTrashed(left)) - Number(characterIsTrashed(right))
}

function isProjectedWorkingSet(previous: Database): boolean {
    if (!isCatalogPresetWorkingSet(previous.botPresets)) return false
    return previous.characters.every((value) => getCatalogCharacterMetadata(value) !== undefined)
}

/// Applies one bounded change window to the working set. Returns null when the
/// window cannot be applied in full and the caller must reproject instead.
export async function applyTargetedWorkingSetInvalidation(
    previous: Database,
    keys: readonly ContentChangeKey[],
    reader: PersistentRevisionReader,
    options: TargetedWorkingSetInvalidationOptions,
): Promise<TargetedWorkingSetInvalidationResult | null> {
    if (keys.length > TARGETED_INVALIDATION_KEY_LIMIT) return null
    if (!isProjectedWorkingSet(previous)) return null
    const plan = planChanges(keys, options)
    if (!plan) return null

    const positions = new Map<string, number>()
    previous.characters.forEach((value, position) => positions.set(value.chaId, position))

    let rootValue: Record<string, unknown> | null = null
    if (plan.root) {
        const root = await reader.readRoot()
        assertPinnedRevision(reader.revision, root.revision, 'Root')
        rootValue = root.value as unknown as Record<string, unknown>
    }

    const botPresetsId = (rootValue?.botPresetsId ?? previous.botPresetsId) as number
    let botPresets: Database['botPresets']
    if (plan.presets || botPresetsId !== previous.botPresetsId ||
        (getCatalogPresetMetadata(previous.botPresets)?.activeConfiguredIndex ?? null) !==
            (previous.botPresets[botPresetsId] ? botPresetsId : null)) {
        const presets = await readPinnedActivePreset(reader, botPresetsId)
        botPresets = createCatalogPresetWorkingSet(presets.catalog, presets.active)
    } else {
        botPresets = advanceCatalogPresetWorkingSetRevision(previous.botPresets, reader.revision)
    }

    const selectedCharacterId = options.selectedCharacterId
    const residentIds = new Set(options.activeCharacterIds)
    let selectedSummary: CharacterSummary | null = null
    let selectedDetail: CharacterDetail | null = null
    const previousSelected = selectedCharacterId
        ? previous.characters[positions.get(selectedCharacterId) ?? -1]
        : null
    const refreshSelected = !!selectedCharacterId && (
        !previousSelected ||
        getCatalogCharacterMetadata(previousSelected)?.residency !== 'detail' ||
        plan.characterIds.has(selectedCharacterId)
    )
    if (selectedCharacterId) {
        residentIds.add(selectedCharacterId)
        if (!refreshSelected && previousSelected?.type === 'group') {
            for (const member of previousSelected.characters) residentIds.add(member)
        }
    }
    if (selectedCharacterId && refreshSelected) {
        plan.characterIds.add(selectedCharacterId)
        selectedSummary = await reader.readCharacterSummary(selectedCharacterId)
        if (selectedSummary && !selectedSummary.archived) {
            const value = await reader.readCharacter(selectedCharacterId)
            if (value) {
                assertPinnedRevision(
                    reader.revision,
                    value.revision,
                    `Character ${selectedCharacterId}`,
                )
                selectedDetail = value.value
                if (value.value.type === 'group') {
                    for (const member of value.value.characters) residentIds.add(member)
                }
            }
        }
    }

    // A residency change carries no locator of its own, so the members whose
    // residency no longer matches the request join the same pass.
    const pending = new Set(plan.characterIds)
    for (const value of previous.characters) {
        const metadata = getCatalogCharacterMetadata(value)
        const resident = metadata?.residency === 'detail'
        const wanted = residentIds.has(value.chaId) && metadata?.residency !== 'archived'
        if (resident !== wanted) pending.add(value.chaId)
    }

    let deferred = false
    const replacements = new Map<string, CompleteCharacter>()
    const removed = new Set<string>()
    for (const id of pending) {
        const summary =
            id === selectedCharacterId
                ? selectedSummary
                : await reader.readCharacterSummary(id)
        if (!summary) {
            removed.add(id)
            continue
        }
        if (summary.archived || !residentIds.has(id)) {
            replacements.set(id, createCatalogCharacterStub(summary))
            continue
        }
        if (id === selectedCharacterId) {
            if (!selectedDetail) throw new Error(`Missing character detail for ${id}`)
            replacements.set(
                id,
                await createPinnedResidentCharacter(
                    reader,
                    summary,
                    selectedDetail,
                    options.selectedConversationId,
                    plan.deferredConversation
                        ? (conversationId) => {
                            const target = options.deferredConversation
                            if (!target || target.conversationId !== conversationId) return null
                            const existing = previous.characters[
                                positions.get(id) ?? -1
                            ]?.chats.find((candidate) => candidate.id === conversationId)
                            if (!existing) return null
                            deferred = true
                            return existing as Chat
                        }
                        : undefined,
                ),
            )
            continue
        }
        const value = await reader.readCharacter(id)
        if (!value) throw new Error(`Missing character detail for ${id}`)
        assertPinnedRevision(reader.revision, value.revision, `Character ${id}`)
        replacements.set(id, createPinnedDetailOnlyCharacter(summary, value.value))
    }

    const characters: Database['characters'] = []
    for (const value of previous.characters) {
        if (removed.has(value.chaId)) continue
        characters.push(replacements.get(value.chaId) ?? value)
    }
    for (const [id, value] of replacements) {
        if (!positions.has(id)) characters.push(value)
    }
    characters.sort(compareCatalogPosition)

    const {
        characters: _characters,
        botPresets: _botPresets,
        pluginCustomStorage: _pluginCustomStorage,
        ...carriedRoot
    } = previous
    return {
        database: {
            ...(rootValue ?? carriedRoot),
            pluginCustomStorage: {},
            botPresets,
            characters,
        } as Database,
        deferred,
    }
}

/// Reads one bounded window through the lease that projects it. Returns null
/// when the consumer needs a rebuild or the window is larger than the limit.
export async function readWorkingSetChangeWindow(
    reader: PersistentRevisionReader,
    limit = TARGETED_INVALIDATION_KEY_LIMIT,
): Promise<ContentChangeKey[] | null> {
    if (!reader.readWorkingSetChangeWindow || !reader.readWorkingSetChangePage) return null
    const window = await reader.readWorkingSetChangeWindow()
    if (window.afterRevision === null) return null
    assertPinnedRevision(reader.revision, window.revision, 'Content change window')
    const keys: ContentChangeKey[] = []
    let after: ContentChangeKey | null = null
    for (;;) {
        const page: ContentChangeKey[] = await reader.readWorkingSetChangePage(
            window.afterRevision,
            after,
            Math.min(CONTENT_CHANGE_PAGE_LIMIT, limit + 1),
        )
        keys.push(...page)
        if (keys.length > limit) return null
        if (page.length === 0) return keys
        after = page[page.length - 1]
    }
}
