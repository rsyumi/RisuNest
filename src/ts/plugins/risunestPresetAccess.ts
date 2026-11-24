import { safeStructuredClone } from '../safeStructuredClone'
import {
    addPresetFrom,
    changeToPresetById,
    getDatabase,
    presetTemplate,
    removePresetById,
    updatePresetById,
    type botPreset,
} from '../storage/database.svelte'
import { peekEffectivePresetId } from '../storage/effectiveIdentityState'
import { getPersistentDataRuntime } from '../storage/persistentDataRuntime.svelte'
import { LastPresetRemovalError, PresetNotFoundError } from '../storage/presetWorkingSetOperations'
import {
    isPlainJsonObject,
    jsonKindOf,
    normalizeTarget,
    requireDatabaseRead,
    runDatabaseWrite,
    throwIfAborted,
    type RisunestHostContext,
    type RisunestWriteResult,
} from './risunestHostContext'

// Model presets by ID: list, read, update, create, delete, select.

/** Preset fields holding keys and endpoints. They are never returned, and a write naming one is rejected. */
export const presetConnectionFields: readonly string[] = [
    'openAIKey', 'proxyKey', 'forceReplaceUrl', 'forceReplaceUrl2', 'koboldURL', 'textgenWebUIStreamURL', 'textgenWebUIBlockingURL',
]

/**
 * The other preset fields, read and written by their stored names; `id` is read only. A stored field in
 * neither list is neither returned nor written, so a field new to presets stays unavailable until it is listed.
 */
export const presetDataFields: readonly string[] = ('id name apiType localNetworkMode localNetworkTimeoutSec mainPrompt jailbreak globalNote temperature maxContext maxResponse frequencyPenalty PresensePenalty formatingOrder aiModel subModel currentPluginProvider promptPreprocess bias proxyRequestModel openrouterRequestModel ooba ainconfig NAISettings autoSuggestPrompt autoSuggestPrefix autoSuggestClean promptTemplate NAIadventure NAIappendName localStopStrings customProxyRequestModel reverseProxyOobaArgs top_p promptSettings repetition_penalty min_p top_a openrouterProvider useInstructPrompt customPromptTemplateToggle templateDefaultVariables moduleIntergration top_k instructChatTemplate JinjaTemplate jsonSchemaEnabled jsonSchema strictJsonSchema extractJson messageNameTemplate namedMessageRole seperateParametersEnabled seperateParameters customAPIFormat systemContentReplacement systemRoleReplacement enableCustomFlags customFlags image regex reasonEffort thinkingTokens thinkingType deepseekThinkingType adaptiveThinkingEffort deepseekReasoningEffort outputImageModal seperateModelsForAxModels seperateModels modelTools fallbackModels fallbackWhenBlankResponse verbosity dynamicOutput').split(' ')

const dataFields = new Set(presetDataFields)

export type RisunestPresetSummary = { id: string; name: string; selected: boolean; applied: boolean }
type PresetRecord = Record<string, unknown>
type FieldWriteResult = RisunestWriteResult & { field?: string }

/** Where the preset methods read and write: the store, through the app's preset list operations. */
interface PresetStore {
    list(): Promise<RisunestPresetSummary[]>
    /** The stored record, with the app's pending edits saved first; null when no preset has the ID. */
    read(id: string): Promise<PresetRecord | null>
    update(id: string, edit: (record: PresetRecord) => void): Promise<void>
    create(copyOf: string | undefined, edit: (record: PresetRecord) => void): Promise<string>
    remove(id: string): Promise<void>
    select(id: string): Promise<void>
}

const productionPresetStore: PresetStore = {
    async list() {
        await getPersistentDataRuntime().flushPendingData('plugin-preset-list')
        const db = getDatabase()
        const presets = db.botPresets ?? []
        // The preset the prompt and model settings hold: the open conversation's binding, then the preset chain's pick, then the selection.
        const effective = peekEffectivePresetId(db)
        const applied = presets.some(preset => preset?.id === effective) ? effective : presets[db.botPresetsId]?.id
        return presets.map((preset, index) => ({
            id: preset?.id ?? '',
            name: preset?.name ?? '',
            selected: index === db.botPresetsId,
            applied: applied !== undefined && preset?.id === applied,
        }))
    },
    async read(id) {
        const runtime = getPersistentDataRuntime()
        await runtime.flushPendingData('plugin-preset-read')
        const stored = await runtime.store.readPreset(id)
        return stored ? stored.value as unknown as PresetRecord : null
    },
    update: (id, edit) => updatePresetById(id, preset => edit(preset as unknown as PresetRecord)),
    create: (copyOf, edit) => addPresetFrom(copyOf ?? safeStructuredClone(presetTemplate), preset => edit(preset as unknown as PresetRecord)),
    remove: (id) => removePresetById(id),
    select: (id) => changeToPresetById(id),
}

class PresetFieldKindError extends Error {
    constructor(readonly field: string) {
        super(`Preset field ${field} must keep the kind of its stored value`)
        this.name = 'PresetFieldKindError'
    }
}

function normalizeReadFields(options: unknown): readonly string[] | undefined {
    if (options === undefined) return undefined
    if (!isPlainJsonObject(options)) throw new TypeError('options must be an object')
    const fields = options.fields
    if (fields === undefined) return undefined
    if (!Array.isArray(fields) || fields.some(field => typeof field !== 'string')) {
        throw new TypeError('fields must be an array of strings')
    }
    return fields
}

function normalizeCopySource(copyOf: unknown): string | undefined {
    if (copyOf === undefined) return undefined
    if (typeof copyOf !== 'string' || copyOf.length === 0) throw new TypeError('copyOf must be a non-empty string')
    return copyOf
}

function normalizeFieldValues(input: unknown, name: string): [string, unknown][] {
    if (!isPlainJsonObject(input)) throw new TypeError(`${name} must be an object`)
    return Object.entries(input)
}

/** The first field a write cannot name or set, judged without the stored record. */
function rejectedField(fields: readonly [string, unknown][]): FieldWriteResult | null {
    for (const [field, value] of fields) {
        if (field === 'id') return { status: 'rejected', reason: 'read-only', field }
        if (!dataFields.has(field)) return { status: 'rejected', reason: 'unavailable', field }
        if (value !== null && jsonKindOf(value) === null) return { status: 'rejected', reason: 'type', field }
    }
    return null
}

/** Replaces each field whole; a field with a stored value keeps its kind, and only a stored null takes null. */
function assignFields(record: PresetRecord, fields: readonly [string, unknown][]): void {
    for (const [field, value] of fields) {
        const stored = record[field]
        const kept = value === null
            ? stored === null
            : stored === undefined || stored === null || jsonKindOf(stored) === jsonKindOf(value)
        if (!kept) throw new PresetFieldKindError(field)
    }
    for (const [field, value] of fields) record[field] = safeStructuredClone(value)
}

function readableFields(record: PresetRecord, fields: readonly string[] | undefined): PresetRecord {
    const preset: PresetRecord = {}
    for (const field of fields ?? Object.keys(record)) {
        if (dataFields.has(field) && Object.hasOwn(record, field)) preset[field] = record[field]
    }
    return preset
}

function writeFailure(error: unknown): FieldWriteResult {
    if (error instanceof PresetNotFoundError) return { status: 'not-found' }
    if (error instanceof LastPresetRemovalError) return { status: 'rejected', reason: 'last-preset' }
    if (error instanceof PresetFieldKindError) return { status: 'rejected', reason: 'type', field: error.field }
    throw error
}

function createPresetAccess(context: RisunestHostContext, store: PresetStore) {
    const write = (operation: () => Promise<void>): Promise<FieldWriteResult> => runDatabaseWrite(context, async () => {
        try {
            await operation()
        } catch (error) {
            return writeFailure(error)
        }
        return { status: 'applied' as const }
    })
    return {
        async listPresets(): Promise<{ items: RisunestPresetSummary[]; total: number }> {
            await requireDatabaseRead(context, 'risunestListPresets')
            const items = await store.list()
            throwIfAborted(context.lifetimeSignal)
            return { items, total: items.length }
        },
        async readPreset(target?: unknown, options?: unknown): Promise<{ status: 'ok'; preset: PresetRecord } | { status: 'not-found' }> {
            const { id } = normalizeTarget(target)
            const fields = normalizeReadFields(options)
            await requireDatabaseRead(context, 'risunestReadPreset')
            const record = await store.read(id)
            throwIfAborted(context.lifetimeSignal)
            return record ? { status: 'ok', preset: readableFields(record, fields) } : { status: 'not-found' }
        },
        async updatePreset(target?: unknown, set?: unknown): Promise<FieldWriteResult> {
            const { id } = normalizeTarget(target)
            const fields = normalizeFieldValues(set, 'set')
            return rejectedField(fields) ?? write(() => store.update(id, record => assignFields(record, fields)))
        },
        async createPreset(input?: unknown): Promise<{ status: 'applied'; id: string } | Exclude<FieldWriteResult, { status: 'applied' }>> {
            if (!isPlainJsonObject(input)) throw new TypeError('input must be an object')
            const { name } = input
            if (typeof name !== 'string') throw new TypeError('name must be a string')
            const copyOf = normalizeCopySource(input.copyOf)
            const fields = input.fields === undefined ? [] : normalizeFieldValues(input.fields, 'fields')
            const rejected = rejectedField(fields)
            if (rejected) return rejected as Exclude<FieldWriteResult, { status: 'applied' }>
            return runDatabaseWrite(context, async () => {
                try {
                    const id = await store.create(copyOf, (record) => {
                        record.name = name
                        assignFields(record, fields)
                    })
                    return { status: 'applied' as const, id }
                } catch (error) {
                    return writeFailure(error) as Exclude<FieldWriteResult, { status: 'applied' }>
                }
            })
        },
        async deletePreset(target?: unknown): Promise<RisunestWriteResult> {
            const { id } = normalizeTarget(target)
            return write(() => store.remove(id))
        },
        async selectPreset(target?: unknown): Promise<RisunestWriteResult> {
            const { id } = normalizeTarget(target)
            return write(() => store.select(id))
        },
    }
}

export function createRisunestPresetAccess(context: RisunestHostContext) {
    return createPresetAccess(context, productionPresetStore)
}
