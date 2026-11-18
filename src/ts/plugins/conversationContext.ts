import type {
    Chat,
    Message,
    RisuPersona,
    botPreset,
    customscript,
    loreBook,
    loreSettings,
} from '../storage/database.svelte'
import type {
    CharacterDetail,
    ConversationWindow,
    DataRevision,
    PersistentConversationMetadata,
    PersistentRevisionReader,
    PersistentRoot,
} from '../storage/persistentDataStore'
import {
    assertPinnedRevision,
    iterateUnarchivedPinnedCharacterSummaries,
} from '../storage/persistentRecordIterator'
import { defineOwnEnumerableProperty } from '../storage/ownEnumerableProperty'
import { effectiveAuthorNote, matchingAuthorNotePresetId } from '../authorNotePresets'
import { selectConversationModules } from '../process/moduleSelection'
import {
    isPlainObject,
    isPluginMessageField,
    pluginMessageWindow,
    requiredId,
    throwIfAborted,
    type PluginMessageWindow,
    type PluginMessageWindowInput,
} from './pluginQueryInput'

export const CONVERSATION_CONTEXT_MAX_CHAT_VARIABLES = 256
export const CONVERSATION_CONTEXT_MAX_EXTRA_FIELDS = 16

const CONVERSATION_PAGE_SIZE = 128

const CHARACTER_FIELDS = [
    'chaId', 'name', 'nickname', 'type', 'desc', 'personality', 'scenario', 'firstMessage',
    'alternateGreetings', 'translatorNote', 'defaultVariables', 'modules', 'characters',
] as const
const CONVERSATION_FIELDS = ['id', 'name', 'note', 'modules', 'bindedPersona'] as const

export interface ConversationContextInclude {
    character: boolean
    lore: boolean
    persona: boolean
    globals: boolean
}

export interface ConversationContextInput {
    characterId?: string
    conversationId?: string
    include?: Partial<ConversationContextInclude>
    chatVariables?: string[] | 'all'
    messages?: PluginMessageWindowInput & { extraFields?: string[] }
    signal?: AbortSignal
}

export interface ConversationContextRequest {
    /** Null reads the selected conversation. */
    target: { characterId: string; conversationId: string } | null
    include: ConversationContextInclude
    chatVariables?: string[] | 'all'
    messages?: { window: PluginMessageWindow; extraFields: string[] }
}

export interface ConversationContextModule {
    id: string
    name: string
    namespace?: string
    lorebook?: loreBook[]
}

export interface ConversationContext {
    revision: DataRevision
    characterId: string
    conversationId: string
    characterIndex: number
    chatIndex: number
    selected: boolean
    messageCount: number
    character?: Record<string, unknown>
    conversation: Record<string, unknown>
    lore?: {
        globalLore?: loreBook[]
        loreSettings?: loreSettings
        localLore?: loreBook[]
        modules: ConversationContextModule[]
    }
    persona?: { id?: string; name: string; personaPrompt: string } | null
    chatVariables?: Record<string, string | number | boolean>
    /** Null when the requested anchor is not in the conversation. */
    messages?: (Omit<ConversationWindow, 'messages'> & {
        messages: (Message & { index: number })[]
    }) | null
    globals?: {
        username: string
        globalChatVariables: Record<string, string>
        templateDefaultVariables: string
        customPromptTemplateToggle: string
        presetRegex: customscript[]
        loreBookDepth: number
    }
}

function optionalBoolean(value: unknown, name: string, fallback: boolean): boolean {
    if (value === undefined) return fallback
    if (typeof value !== 'boolean') throw new TypeError(`${name} must be a boolean`)
    return value
}

function stringList(value: unknown, name: string, maximum: number): string[] {
    if (!Array.isArray(value)) throw new TypeError(`${name} must be an array`)
    if (value.length > maximum) throw new RangeError(`${name} accepts at most ${maximum} entries`)
    for (const entry of value) {
        if (typeof entry !== 'string') throw new TypeError(`${name} entries must be strings`)
    }
    return [...value]
}

/** Validates plugin input before any read; invalid input rejects as the query methods do. */
export function normalizeConversationContextInput(input: unknown): ConversationContextRequest {
    if (input === undefined) input = {}
    if (!isPlainObject(input)) throw new TypeError('Conversation context input must be an object')
    const value = input as ConversationContextInput
    let target: ConversationContextRequest['target'] = null
    if (value.characterId !== undefined || value.conversationId !== undefined) {
        if (value.characterId === undefined || value.conversationId === undefined) {
            throw new RangeError('characterId and conversationId must be given together')
        }
        requiredId(value.characterId, 'characterId')
        requiredId(value.conversationId, 'conversationId')
        target = { characterId: value.characterId, conversationId: value.conversationId }
    }
    if (value.include !== undefined && !isPlainObject(value.include)) {
        throw new TypeError('include must be an object')
    }
    const include = value.include ?? {}
    const request: ConversationContextRequest = {
        target,
        include: {
            character: optionalBoolean(include.character, 'include.character', true),
            lore: optionalBoolean(include.lore, 'include.lore', false),
            persona: optionalBoolean(include.persona, 'include.persona', false),
            globals: optionalBoolean(include.globals, 'include.globals', false),
        },
    }
    if (value.chatVariables !== undefined) {
        request.chatVariables = value.chatVariables === 'all'
            ? 'all'
            : stringList(value.chatVariables, 'chatVariables', CONVERSATION_CONTEXT_MAX_CHAT_VARIABLES)
    }
    if (value.messages !== undefined) {
        if (!isPlainObject(value.messages)) throw new TypeError('messages must be an object')
        const extraFields = value.messages.extraFields === undefined
            ? []
            : stringList(value.messages.extraFields, 'messages.extraFields', CONVERSATION_CONTEXT_MAX_EXTRA_FIELDS)
        for (const field of extraFields) {
            if (!isPluginMessageField(field)) {
                throw new RangeError(`messages.extraFields entry ${field} is not a plugin message field`)
            }
        }
        request.messages = { window: pluginMessageWindow(value.messages), extraFields }
    }
    return request
}

function pickFields(
    source: Record<string, unknown>,
    fields: readonly string[],
): Record<string, unknown> {
    const result: Record<string, unknown> = {}
    for (const field of fields) {
        if (Object.hasOwn(source, field) && source[field] !== undefined) result[field] = source[field]
    }
    return result
}

export async function findCharacterIndex(
    reader: PersistentRevisionReader,
    characterId: string,
): Promise<number | null> {
    let position = 0
    for await (const summary of iterateUnarchivedPinnedCharacterSummaries(reader)) {
        if (summary.id === characterId) return position
        position += 1
    }
    return null
}

export async function findChatIndex(
    reader: PersistentRevisionReader,
    characterId: string,
    conversationId: string,
): Promise<number | null> {
    let position = 0
    let cursor: string | undefined
    do {
        const page = await reader.queryConversations({
            characterId,
            order: 'configured',
            limit: CONVERSATION_PAGE_SIZE,
            cursor,
        })
        assertPinnedRevision(reader.revision, page.revision, `Conversation page for ${characterId}`)
        for (const summary of page.items) {
            if (summary.id === conversationId) return position
            position += 1
        }
        cursor = page.nextCursor
    } while (cursor !== undefined)
    return null
}

async function readPinnedPreset(reader: PersistentRevisionReader, id: string): Promise<botPreset | null> {
    const preset = await reader.readPreset(id)
    if (!preset) return null
    assertPinnedRevision(reader.revision, preset.revision, `Preset ${id}`)
    return preset.value
}

/** The conversation's bound preset when it names one in the library, otherwise the global selection. */
async function readActivePreset(
    reader: PersistentRevisionReader,
    root: PersistentRoot,
    conversation: Omit<Chat, 'message'>,
): Promise<botPreset | null> {
    if (typeof conversation.promptPresetId === 'string' && conversation.promptPresetId) {
        const bound = await readPinnedPreset(reader, conversation.promptPresetId)
        if (bound) return bound
    }
    const selection = root.botPresetsId
    let id: string | undefined
    if (typeof selection === 'string') {
        id = selection
    } else if (typeof selection === 'number' && selection >= 0) {
        const catalog = await reader.queryPresets()
        assertPinnedRevision(reader.revision, catalog.revision, 'Preset catalog')
        id = catalog.items.find((item) => item.configuredIndex === selection)?.id
    }
    if (!id) return null
    return readPinnedPreset(reader, id)
}

/** The active preset's value for a preset-mirrored root field, or the stored root value without a preset. */
function presetMirror<T>(
    preset: botPreset | null,
    root: PersistentRoot,
    presetField: string,
    rootField: string,
): T | undefined {
    const source = preset
        ? preset as unknown as Record<string, unknown>
        : root as unknown as Record<string, unknown>
    const field = preset ? presetField : rootField
    return Object.hasOwn(source, field) ? source[field] as T : undefined
}

function resolvePersonas(root: PersistentRoot, conversation: Omit<Chat, 'message'>) {
    const personas: RisuPersona[] = root.personas ?? []
    const bound = conversation.bindedPersona
        ? personas.find((persona) => persona.id === conversation.bindedPersona) ?? null
        : null
    const selection = root.selectedPersona
    const selectedIndex = typeof selection === 'string'
        ? Math.max(0, personas.findIndex((persona) => persona.id === selection))
        : selection
    const selected = personas[selectedIndex] ?? null
    return { bound, selected }
}

function effectiveGlobalChatVariables(
    root: PersistentRoot,
    conversation: Omit<Chat, 'message'>,
): Record<string, string> {
    const variables: Record<string, string> = {
        ...(root.explicitGlobalChatVariables ?? root.globalChatVariables ?? {}),
    }
    if (!root.disableToggleBinding && conversation.savedToggleValues !== undefined) {
        for (const key of Object.keys(variables)) if (key.startsWith('toggle_')) delete variables[key]
        for (const [key, value] of Object.entries(conversation.savedToggleValues)) {
            if (key.startsWith('toggle_')) defineOwnEnumerableProperty(variables, key, value)
        }
    }
    for (const [key, value] of Object.entries(conversation.GLGlobalVariables ?? {})) {
        if (value && value !== 'null') defineOwnEnumerableProperty(variables, key, value)
    }
    return variables
}

function projectMessages(
    window: ConversationWindow,
    extraFields: readonly string[],
): NonNullable<ConversationContext['messages']> {
    const kept = new Set(extraFields)
    return {
        ...window,
        messages: window.messages.map((message, offset) => {
            const projected: Record<string, unknown> = {}
            for (const [key, value] of Object.entries(message)) {
                if (key.startsWith('__') && !kept.has(key)) continue
                defineOwnEnumerableProperty(projected, key, value)
            }
            projected.index = window.startIndex + offset
            return projected as unknown as Message & { index: number }
        }),
    }
}

/**
 * Reads every requested part from the reader's one revision. A missing target, or no
 * selected conversation when the request names none, reads as null.
 */
export async function readPinnedConversationContext(
    reader: PersistentRevisionReader,
    request: ConversationContextRequest,
    options: {
        selected: { characterId: string; conversationId: string } | null
        allowPrivate: boolean
        signal?: AbortSignal
    },
): Promise<ConversationContext | null> {
    const target = request.target ?? options.selected
    if (!target) return null
    const { characterId, conversationId } = target
    const { include } = request
    const persona = include.persona && options.allowPrivate
    const globals = include.globals && options.allowPrivate

    const metadataRecord = await reader.readConversationMetadata(characterId, conversationId)
    throwIfAborted(options.signal)
    if (!metadataRecord) return null
    assertPinnedRevision(reader.revision, metadataRecord.revision, `Conversation ${conversationId}`)
    const metadata: PersistentConversationMetadata = metadataRecord.value
    const conversation = metadata.conversation

    const characterIndex = await findCharacterIndex(reader, characterId)
    throwIfAborted(options.signal)
    if (characterIndex === null) return null
    const chatIndex = await findChatIndex(reader, characterId, conversationId)
    throwIfAborted(options.signal)
    if (chatIndex === null) return null

    let detail: CharacterDetail | null = null
    if (include.character || include.lore) {
        const found = await reader.readCharacter(characterId)
        throwIfAborted(options.signal)
        if (!found) return null
        assertPinnedRevision(reader.revision, found.revision, `Character ${characterId}`)
        detail = found.value
    }

    let root: PersistentRoot | null = null
    let preset: botPreset | null = null
    const presetNote = matchingAuthorNotePresetId(conversation) !== null
    if (include.lore || persona || globals || presetNote) {
        const found = await reader.readRoot()
        throwIfAborted(options.signal)
        assertPinnedRevision(reader.revision, found.revision, 'Root')
        root = found.value
        if (include.lore || globals) {
            preset = await readActivePreset(reader, root, conversation)
            throwIfAborted(options.signal)
        }
    }

    const context: ConversationContext = {
        revision: reader.revision,
        characterId,
        conversationId,
        characterIndex,
        chatIndex,
        selected: options.selected?.characterId === characterId
            && options.selected.conversationId === conversationId,
        messageCount: metadata.totalMessages,
        conversation: pickFields(
            {
                ...conversation,
                id: conversation.id ?? conversationId,
                note: effectiveAuthorNote(root?.authorNotePresets, conversation),
            } as Record<string, unknown>,
            CONVERSATION_FIELDS,
        ),
    }

    if (include.character && detail) {
        context.character = pickFields(detail as unknown as Record<string, unknown>, CHARACTER_FIELDS)
    }

    const personas = root ? resolvePersonas(root, conversation) : null
    if (include.lore && detail && root) {
        const modules = selectConversationModules(
            {
                modules: root.modules,
                enabledModules: root.enabledModules,
                moduleIntergration: presetMirror<string>(preset, root, 'moduleIntergration', 'moduleIntergration'),
            },
            conversation.modules,
            detail.modules,
            personas!.bound,
        )
        context.lore = {
            globalLore: detail.globalLore,
            loreSettings: detail.loreSettings,
            localLore: conversation.localLore,
            modules: modules.map((module) => pickFields(
                module as unknown as Record<string, unknown>,
                ['id', 'name', 'namespace', 'lorebook'],
            ) as unknown as ConversationContextModule),
        }
    }

    if (persona && personas) {
        const used = personas.bound ?? personas.selected
        context.persona = used
            ? pickFields(used as unknown as Record<string, unknown>, ['id', 'name', 'personaPrompt']) as NonNullable<ConversationContext['persona']>
            : null
    }

    if (request.chatVariables !== undefined) {
        const scriptstate = conversation.scriptstate ?? {}
        const keys = request.chatVariables === 'all' ? Object.keys(scriptstate) : request.chatVariables
        const variables: Record<string, string | number | boolean> = {}
        for (const key of keys) {
            if (Object.hasOwn(scriptstate, key)) defineOwnEnumerableProperty(variables, key, scriptstate[key])
        }
        context.chatVariables = variables
    }

    if (request.messages) {
        const window = await reader.readConversationWindow({
            characterId,
            conversationId,
            ...request.messages.window,
        })
        throwIfAborted(options.signal)
        if (window) assertPinnedRevision(reader.revision, window.revision, `Conversation ${conversationId} window`)
        context.messages = window ? projectMessages(window.value, request.messages.extraFields) : null
    }

    if (globals && root && personas) {
        context.globals = {
            username: personas.bound
                ? personas.bound.name
                : personas.selected
                    ? personas.selected.name ?? 'User'
                    : root.username ?? 'User',
            globalChatVariables: effectiveGlobalChatVariables(root, conversation),
            templateDefaultVariables: presetMirror<string>(preset, root, 'templateDefaultVariables', 'templateDefaultVariables') ?? '',
            customPromptTemplateToggle: presetMirror<string>(preset, root, 'customPromptTemplateToggle', 'customPromptTemplateToggle') ?? '',
            presetRegex: presetMirror<customscript[]>(preset, root, 'regex', 'presetRegex') ?? [],
            loreBookDepth: root.loreBookDepth ?? 5,
        }
    }

    return context
}
