// Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/ts/characterUpdate.ts), GPL-3.0.

import { language } from 'src/lang'
import type { loreBook } from '../storage/database.svelte'
import type { RisuModule } from '../process/modules'

// Finds an installed character or module that an import is a new version of,
// and merges the two against the import as it was first installed (the base)
// so that edits the user made are kept. Pure functions only, apart from the
// script type names an order shows: the dialog and storage live in
// flow.svelte.ts.

/** Character extension holding the asset key of the version as installed. */
export const UPDATE_BASE_EXTENSION_KEY = 'risunestUpdateBase'

export type UpdateMatchReason = 'lineage' | 'name' | 'namespace'

export interface UpdateCandidate {
    id: string
    name: string
    reason: UpdateMatchReason
    /** Chats of a character, lorebook entries of a module. */
    count: number
}

function compact(value: unknown): string {
    return String(value ?? '').normalize('NFKC').toLowerCase().replace(/[^\p{L}\p{N}]/gu, '')
}

const versionToken = String.raw`(?:버전\s*\d+(?:\.\d+)*|(?:ver(?:sion)?\.?\s*|v\s*)\d+(?:\.\d+)*[a-z]?|\d+\.\d+(?:\.\d+)*[a-z]?)`
const stageWord = String.raw`(?:beta|alpha|rc\d*|preview|demo|test|final|hotfix|fix|patch|베타|알파|체험판|데모|테스트|정식판|정식|최종판|최종|수정판|수정|패치)`
// "Luna v1.2", "Luna - ver 3", "Luna (2)" (the suffix a second install gets), "Luna [v1.0]",
// "Luna Beta", "Luna (베타)". A release word needs a separator so it is never cut out of a word.
const versionTail = new RegExp(String.raw`(?:\s*[-–—:_]?\s*${versionToken}|(?:\s+|\s*[-–—:_]\s*)${stageWord}|\s*[([]\s*(?:${versionToken}|v?\d+(?:\.\d+)*|${stageWord})\s*[)\]])\s*$`, 'iu')

/** Name without trailing version and release marks, case and punctuation, for comparing installs. */
export function updateNameKey(name: unknown): string {
    let text = String(name ?? '').normalize('NFKC').trim()
    while (true) {
        const next = text.replace(versionTail, '').trim()
        if (next === text || !next) break
        text = next
    }
    return compact(text)
}

const reasonRank: Record<UpdateMatchReason, number> = { lineage: 0, name: 1, namespace: 2 }

function rank(candidates: UpdateCandidate[]): UpdateCandidate[] {
    return candidates.sort((a, b) => reasonRank[a.reason] - reasonRank[b.reason]
        || b.count - a.count
        || a.name.localeCompare(b.name))
}

/** Installed characters (id, name and chat count) whose name matches the incoming one, best first. */
export function findCharacterUpdateCandidates(
    installed: Iterable<{ id: string, name: string, count: number }>,
    incomingName: string,
): UpdateCandidate[] {
    const key = updateNameKey(incomingName)
    if (!key) return []
    const candidates: UpdateCandidate[] = []
    for (const entry of installed) {
        if (updateNameKey(entry.name) === key) candidates.push({ ...entry, reason: 'name' })
    }
    return rank(candidates)
}

/** Records the id the module had in its file, before the import gives it a new one. */
export function markModuleImport(module: RisuModule): void {
    if (!module.updateLineage && typeof module.id === 'string' && module.id) module.updateLineage = module.id
    delete module.updateBase
}

/** Modules a user installed, as opposed to persona-embedded modules and MCP connections. */
export function isInstalledModule(module: RisuModule | undefined | null): module is RisuModule {
    return !!module?.id && !module.id.startsWith('$') && !module.mcp
}

/** Installed modules the incoming module may update, best first. */
export function findModuleUpdateCandidates(modules: readonly RisuModule[], incoming: RisuModule): UpdateCandidate[] {
    const name = updateNameKey(incoming.name)
    const lineage = incoming.updateLineage ?? ''
    const candidates: UpdateCandidate[] = []
    for (const module of modules) {
        if (!isInstalledModule(module) || module.id === incoming.id) continue
        let reason: UpdateMatchReason | null = null
        if (lineage && (module.id === lineage || module.updateLineage === lineage)) reason = 'lineage'
        else if (name && updateNameKey(module.name) === name) reason = 'name'
        else if (incoming.namespace && module.namespace === incoming.namespace) reason = 'namespace'
        if (!reason) continue
        candidates.push({ id: module.id, name: module.name ?? '', reason, count: module.lorebook?.length ?? 0 })
    }
    return rank(candidates)
}

/** How one kind of entity merges. */
export interface UpdateSpec {
    /** The user's own data. An update never touches these. */
    preserved: Set<string>
    /** Bookkeeping that follows the import without asking. */
    silent: Set<string>
    /** Lists merged entry by entry, with the key that identifies an entry. */
    lists: Record<string, (item: any) => string>
    /** Objects merged key by key, with keys that follow the import without asking. */
    maps: Record<string, Set<string>>
    /** Lists of lore entries: ids and caches are ignored and the installed id is kept. */
    lore: Set<string>
    /** Lists whose entries both sides changed are merged field by field. */
    fieldMerged: Set<string>
}

const loreKey = (entry: loreBook) => JSON.stringify([entry?.mode ?? '', entry?.comment ?? '', entry?.key ?? '', entry?.secondkey ?? ''])
// A named regex stays the same item when the author changes its pattern.
const regexKey = (script: any) => JSON.stringify(script?.comment ? [script.comment, script.type ?? ''] : ['', script?.type ?? '', script?.in ?? ''])
const triggerKey = (script: any) => JSON.stringify([script?.comment ?? '', script?.type ?? ''])
const assetKey = (asset: any) => JSON.stringify([asset?.[0] ?? ''])

export const CHARACTER_UPDATE: UpdateSpec = {
    preserved: new Set([
        'type', 'chaId', 'chats', 'chatFolders', 'chatPage', 'firstMsgIndex', 'notes',
        'ttsMode', 'ttsSpeech', 'voicevoxConfig', 'naittsConfig', 'gptSoVitsConfig', 'fishSpeechConfig',
        'oaiVoice', 'oaiTTSConfig', 'hfTTS', 'ttsReadOnlyQuoted', 'supaMemory',
        'removedQuotes', 'postHistoryInstructionsApplied', 'reloadKeys', 'scriptstate', 'realmId',
        'imported', 'trashTime', 'lastInteraction', 'translatorNote', 'doNotChangeSeperateModels',
        'escapeOutput', 'modules', 'coldstorage', 'coldStoragedChats', 'virtualscript',
    ]),
    silent: new Set(['creation_date', 'modification_date', 'additionalData']),
    lists: {
        globalLore: loreKey, customscript: regexKey, triggerscript: triggerKey,
        emotionImages: assetKey, additionalAssets: assetKey,
        ccAssets: asset => JSON.stringify([asset?.type ?? '', asset?.name ?? '']),
    },
    maps: { extentions: new Set(['risuRealmImportId', UPDATE_BASE_EXTENSION_KEY]) },
    lore: new Set(['globalLore']),
    fieldMerged: new Set(['globalLore', 'customscript']),
}

export const MODULE_UPDATE: UpdateSpec = {
    // The id is what characters, chats and the enabled list point to.
    preserved: new Set(['id', 'updateBase']),
    silent: new Set(['updateLineage']),
    lists: { lorebook: loreKey, regex: regexKey, trigger: triggerKey, assets: assetKey },
    maps: {},
    lore: new Set(['lorebook']),
    fieldMerged: new Set(['lorebook', 'regex']),
}

function isEmpty(value: unknown): boolean {
    if (value === undefined || value === null || value === '') return true
    if (Array.isArray(value)) return value.length === 0
    return typeof value === 'object' && Object.keys(value as object).length === 0
}

function stable(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(stable)
    if (!value || typeof value !== 'object') return value
    const out: Record<string, unknown> = {}
    for (const key of Object.keys(value as object).sort()) {
        const item = (value as Record<string, unknown>)[key]
        if (item !== undefined) out[key] = stable(item)
    }
    return out
}

/** Comparable form: drops ids and caches that differ between installs of the same card. */
function canon(spec: UpdateSpec, field: string, value: unknown): string {
    if (isEmpty(value)) return ''
    let clean = value
    if (spec.lore.has(field) && value && typeof value === 'object') {
        const { id: _id, loreCache: _cache, ...rest } = value as loreBook & { id?: string, loreCache?: unknown }
        clean = rest
    } else if (field === 'vits' && value && typeof value === 'object') {
        const { id: _id, ...rest } = value as { id?: string }
        clean = rest
    }
    return JSON.stringify(stable(clean))
}

export type UpdateItemStatus =
    | 'update'    // only the author changed it
    | 'mine'      // only the user changed it
    | 'added'     // new in the update
    | 'removed'   // the author removed it, the user had not changed it
    | 'mineOnly'  // the user added it
    | 'conflict'  // both changed it
    | 'merged'    // both changed it, in different fields
    | 'differs'   // no base: who changed it is unknown

export type UpdateSide = 'mine' | 'theirs'
/** A side, or both sides' changes of an entry merged field by field. */
export type UpdateResolution = UpdateSide | 'both'
/**
 * A resolution, a value the user wrote while comparing the two, or fields the user wrote into an entry,
 * which otherwise keeps what 내 버전 writes.
 */
export type UpdateChoice = UpdateResolution | { value: unknown } | { edited: Record<string, unknown> }

export interface UpdateItem {
    id: string
    field: string
    /** Item key inside a list or the extensions map; empty for a whole field or a list's order. */
    key: string
    /** The item is the order of a list: each side holds its entry names in order. */
    order?: boolean
    status: UpdateItemStatus
    mine: unknown
    theirs: unknown
    /** The value as first installed; meaningful only when baseKnown. */
    base: unknown
    baseKnown: boolean
    /** Preselected side, or null when the user has to decide. */
    choice: UpdateResolution | null
    /**
     * An entry merged field by field: what mine and new write, as shown. On a conflict they settle only the
     * fields both sides changed; on a merged item both hold the merged entry. The update writes the fields
     * only the author changed (`authored`) and, for new, those both changed (`overlap`) onto the entry as
     * it is stored then.
     */
    resolved?: { mine: Record<string, unknown>, theirs: Record<string, unknown>, authored: string[], overlap: string[] }
    /** An entry only the author changed, compared field by field: the fields new writes onto the entry as it is stored then. */
    authored?: string[]
}

export interface UpdatePlan {
    hasBase: boolean
    items: UpdateItem[]
}

function decide(hasBase: boolean, base: string, mine: string, theirs: string, present: { mine: boolean, theirs: boolean, base: boolean }): Pick<UpdateItem, 'status' | 'choice'> | null {
    if (mine === theirs && present.mine === present.theirs) return null
    if (!hasBase) return { status: 'differs', choice: null }
    if (!present.base) {
        if (present.theirs && !present.mine) return { status: 'added', choice: 'theirs' }
        if (present.mine && !present.theirs) return { status: 'mineOnly', choice: 'mine' }
        return { status: 'conflict', choice: null }
    }
    if (mine === base && present.mine) return present.theirs ? { status: 'update', choice: 'theirs' } : { status: 'removed', choice: 'theirs' }
    if (theirs === base && present.theirs) return { status: 'mine', choice: 'mine' }
    return { status: 'conflict', choice: null }
}

const isRecord = (value: unknown): value is Record<string, unknown> => !!value && typeof value === 'object' && !Array.isArray(value)
const fieldCanon = (value: unknown) => value === undefined ? undefined : JSON.stringify(stable(value))

/**
 * One entry both sides changed, merged field by field against the base: a field one side alone
 * changed takes that side, and one both changed takes each side in turn. Lore ids and caches are
 * the installed entry's. Null when the entry is not a record on every side.
 */
function mergeFields(lore: boolean, base: unknown, mine: unknown, theirs: unknown): NonNullable<UpdateItem['resolved']> | null {
    if (!isRecord(base) || !isRecord(mine) || !isRecord(theirs)) return null
    const out = { mine: {} as Record<string, unknown>, theirs: {} as Record<string, unknown>, authored: [] as string[], overlap: [] as string[] }
    for (const key of new Set([...Object.keys(mine), ...Object.keys(theirs), ...Object.keys(base)])) {
        let values: [unknown, unknown]
        if (lore && (key === 'id' || key === 'loreCache')) values = [mine[key], mine[key]]
        else {
            const [b, m, t] = [fieldCanon(base[key]), fieldCanon(mine[key]), fieldCanon(theirs[key])]
            if (m === t || t === b) values = [mine[key], mine[key]]
            else if (m === b) {
                out.authored.push(key)
                values = [theirs[key], theirs[key]]
            } else {
                out.overlap.push(key)
                values = [mine[key], theirs[key]]
            }
        }
        if (values[0] !== undefined) out.mine[key] = values[0]
        if (values[1] !== undefined) out.theirs[key] = values[1]
    }
    return out
}

function keyed(keyOf: (item: any) => string, list: unknown): Map<string, unknown> {
    const out = new Map<string, unknown>()
    const seen = new Map<string, number>()
    for (const item of Array.isArray(list) ? list : []) {
        const key = keyOf(item)
        const slot = seen.get(key) ?? 0
        seen.set(key, slot + 1)
        out.set(slot ? `${key}#${slot}` : key, item)
    }
    return out
}

function mapEntries(value: unknown): Map<string, unknown> {
    return new Map(value && typeof value === 'object' && !Array.isArray(value) ? Object.entries(value) : [])
}

const has = (record: object, key: string) => Object.prototype.hasOwnProperty.call(record, key)

/** Whether the entries a list shares with another come in another order there. */
function reordered(list: Map<string, unknown>, other: Map<string, unknown>): boolean {
    const order = [...other.keys()].filter((key) => list.has(key))
    let index = 0
    for (const key of list.keys()) if (other.has(key) && key !== order[index++]) return true
    return false
}

/** Whether an entry both lists have and the base does not comes before a shared entry in one and after it in the other. */
function addedApart(mine: Map<string, unknown>, theirs: Map<string, unknown>, base: Map<string, unknown>): boolean {
    const at = new Map([...theirs.keys()].map((key, index) => [key, index]))
    let last = -1, lastAdded = -1
    for (const key of mine.keys()) {
        const index = at.get(key)
        if (index === undefined) continue
        if (index < (base.has(key) ? lastAdded : last)) return true
        last = Math.max(last, index)
        if (!base.has(key)) lastAdded = Math.max(lastAdded, index)
    }
    return false
}

// Script types, by the label the script editors show for them.
const scriptTypes = {
    editinput: 'editInput', editoutput: 'editOutput', editprocess: 'editProcess', editdisplay: 'editDisplay', edittrans: 'editTranslationDisplay', disabled: 'disabled',
    start: 'triggerStart', output: 'triggerOutput', input: 'triggerInput', manual: 'triggerManual', display: 'editDisplay', request: 'editProcess',
} satisfies Record<string, keyof typeof language>
const entryName = (entry: any) => String(entry?.comment || entry?.name || entry?.key || entry?.in || entry?.[0] || '')

/** Entry names in list order, one list per side, for the order of a list itself. A script whose name another entry has shows its type. */
function entryNames(...lists: Map<string, unknown>[]): string[][] {
    const keys = new Map<string, Set<string>>()
    for (const list of lists) for (const [key, entry] of list) keys.set(entryName(entry), (keys.get(entryName(entry)) ?? new Set()).add(key))
    return lists.map((list) => [...list.values()].map((entry: any) => {
        const name = entryName(entry)
        const type = has(scriptTypes, String(entry?.type)) ? scriptTypes[entry.type as keyof typeof scriptTypes] : null
        return type && keys.get(name)!.size > 1 ? `${name} (${language[type]})` : name
    }))
}

/**
 * Compares the installed entity (mine), the import (theirs) and the import as
 * it was installed (base, null when unknown). Fields the import does not set
 * and the user's own data are left out. Fields `unread` names compare as
 * without a base, for a base that does not carry them.
 */
export function planUpdate<T extends object>(spec: UpdateSpec, mine: T, theirs: T, base: Partial<T> | null, unread?: (field: string) => boolean): UpdatePlan {
    const items: UpdateItem[] = []
    for (const field of Object.keys(theirs)) {
        if (spec.preserved.has(field)) continue
        const known = !!base && !unread?.(field)
        const push = (key: string, values: { b?: unknown, m?: unknown, t?: unknown }, present: { mine: boolean, theirs: boolean, base: boolean }) => {
            let verdict = decide(known, canon(spec, field, values.b), canon(spec, field, values.m), canon(spec, field, values.t), present)
            if (!verdict || spec.silent.has(field)) return
            let resolved: UpdateItem['resolved']
            let authored: string[] | undefined
            if (key && spec.fieldMerged.has(field) && present.base && present.mine && present.theirs) {
                if (verdict.status === 'conflict') {
                    resolved = mergeFields(spec.lore.has(field), values.b, values.m, values.t) ?? undefined
                    if (resolved && !resolved.overlap.length) verdict = { status: 'merged', choice: 'both' }
                } else if (verdict.status === 'update') {
                    authored = mergeFields(spec.lore.has(field), values.b, values.m, values.t)?.authored
                }
            }
            items.push({
                id: key ? `${field}\u0000${key}` : field, field, key, mine: values.m, theirs: values.t, base: values.b,
                baseKnown: known && present.base, ...verdict, ...(resolved ? { resolved } : {}), ...(authored ? { authored } : {}),
            })
        }
        const m = (mine as any)[field]
        const t = (theirs as any)[field]
        const b = known ? (base as any)[field] : undefined
        const keyOf = has(spec.lists, field) ? spec.lists[field] : null
        const silentKeys = has(spec.maps, field) ? spec.maps[field] : null
        if (keyOf || silentKeys) {
            const read = keyOf ? (value: unknown) => keyed(keyOf, value) : mapEntries
            const mineMap = read(m), theirsMap = read(t), baseMap = read(b)
            // The entries both have come in another order. The base settles it only when one side alone moved
            // them, and not when an entry both added, which it has no place for, sits apart.
            if (keyOf && reordered(mineMap, theirsMap)) {
                const settled = known && !addedApart(mineMap, theirsMap, baseMap)
                const [mineOrder, theirsOrder, baseOrder] = entryNames(mineMap, theirsMap, baseMap)
                const verdict: Pick<UpdateItem, 'status' | 'choice'> = !settled ? { status: known ? 'conflict' : 'differs', choice: null }
                    : !reordered(theirsMap, baseMap) ? { status: 'mine', choice: 'mine' }
                    : !reordered(mineMap, baseMap) ? { status: 'update', choice: 'theirs' }
                    : { status: 'conflict', choice: null }
                items.push({ id: field, field, key: '', order: true, mine: mineOrder, theirs: theirsOrder, base: baseOrder, baseKnown: known, ...verdict })
            }
            for (const key of new Set([...theirsMap.keys(), ...mineMap.keys(), ...baseMap.keys()])) {
                if (!mineMap.has(key) && !theirsMap.has(key)) continue
                if (silentKeys?.has(key)) continue
                push(key, { b: baseMap.get(key), m: mineMap.get(key), t: theirsMap.get(key) },
                    { mine: mineMap.has(key), theirs: theirsMap.has(key), base: baseMap.has(key) })
            }
            continue
        }
        push('', { b, m, t }, { mine: true, theirs: true, base: true })
    }
    return { hasBase: !!base, items }
}

export function unresolvedItems(plan: UpdatePlan): UpdateItem[] {
    return plan.items.filter(item => item.choice === null)
}

/**
 * Merges a list in the order of one side, the author's unless `mineOrder`.
 * Items only the other side has follow the item they came after there.
 */
function mergeList(keyOf: (item: any) => string, lore: boolean, mineList: unknown, theirsList: unknown, choices: Map<string, AppliedChoice>, mineOrder = false): unknown[] {
    const mineMap = keyed(keyOf, mineList)
    const theirsMap = keyed(keyOf, theirsList)
    const keepId = (chosen: unknown, own: unknown) => lore && own && chosen && typeof chosen === 'object'
        && (own as { id?: string }).id ? { ...(chosen as object), id: (own as { id?: string }).id } : chosen
    const pick = (key: string): unknown => {
        const own = mineMap.get(key)
        const other = theirsMap.get(key)
        const choice = choices.get(key)
        // Without a choice both sides had the same entry when they were compared: one removed since stays removed.
        if (!choice) return own
        if (typeof choice === 'object') return keepId(chosenValue(choice, own, other), own)
        return choice === 'mine' ? own : keepId(other, own)
    }
    const [lead, other] = mineOrder ? [mineMap, theirsMap] : [theirsMap, mineMap]
    const following = new Map<string | null, string[]>()
    let previous: string | null = null
    for (const key of other.keys()) {
        if (lead.has(key)) previous = key
        else if (following.has(previous)) following.get(previous)!.push(key)
        else following.set(previous, [key])
    }
    const result: unknown[] = []
    const place = (key: string) => {
        const value = pick(key)
        if (value !== undefined) result.push(value)
    }
    for (const key of following.get(null) ?? []) place(key)
    for (const key of lead.keys()) {
        place(key)
        for (const next of following.get(key) ?? []) place(next)
    }
    return result
}

/**
 * A choice on one entry, written onto the entry as it is stored when the update is written: the fields
 * `taken` get the author's value, a field the author removed included, and `set` holds the fields the user
 * wrote. An entry the user removed since the comparison stays removed unless `restore`.
 */
interface EntryPatch {
    taken: string[]
    set?: Record<string, unknown>
    restore: boolean
}

type AppliedChoice = UpdateResolution | { value: unknown } | EntryPatch

function chosenValue(choice: Exclude<AppliedChoice, string>, own: unknown, other: unknown): unknown {
    return 'value' in choice ? choice.value : patchEntry(choice, own, other)
}

function patchEntry(patch: EntryPatch, own: unknown, other: unknown): unknown {
    const start = own ?? (patch.restore ? other : undefined)
    if (!isRecord(start)) return start
    const out: Record<string, unknown> = { ...start }
    for (const key of patch.taken) {
        const value = isRecord(other) ? other[key] : undefined
        if (value === undefined) delete out[key]
        else out[key] = value
    }
    return { ...out, ...patch.set }
}

/**
 * What a choice writes. On an entry merged field by field 둘 다 반영 and, on a conflict, a side take the
 * author's fields onto the stored entry, while a side of a merged item is that whole entry. New on an entry
 * only the author changed takes the fields the author changed. A written field goes into what 내 버전 writes.
 */
function resolvedChoice(item: UpdateItem, side: UpdateChoice): AppliedChoice {
    const authored = item.resolved?.authored ?? []
    if (typeof side === 'object') return 'edited' in side ? { taken: authored, set: side.edited, restore: true } : side
    if (side === 'theirs' && item.authored) return { taken: item.authored, restore: true }
    if (side === 'both') {
        if (!item.resolved || item.status !== 'merged') throw new Error(`Update item ${item.id} has no merged entry`)
        return { taken: authored, restore: false }
    }
    if (!item.resolved || item.status !== 'conflict') return side
    return side === 'mine' ? { taken: authored, restore: false } : { taken: [...authored, ...item.resolved.overlap], restore: true }
}

/**
 * The installed entity with the update applied by the plan's choices. Every
 * unresolved item must have a choice. Preserved fields stay as they are.
 */
export function applyUpdate<T extends object>(spec: UpdateSpec, mine: T, theirs: T, plan: UpdatePlan, choices: Record<string, UpdateChoice> = {}): T {
    const chosen = new Map<string, Map<string, AppliedChoice>>()
    for (const item of plan.items) {
        const side = choices[item.id] ?? item.choice
        if (!side) throw new Error(`Update choice missing for ${item.id}`)
        if (!chosen.has(item.field)) chosen.set(item.field, new Map())
        chosen.get(item.field)!.set(item.key, resolvedChoice(item, side))
    }
    const result: Record<string, any> = { ...mine }
    for (const field of Object.keys(theirs)) {
        if (spec.preserved.has(field)) continue
        const fieldChoices = chosen.get(field) ?? new Map<string, AppliedChoice>()
        const m = (mine as any)[field]
        const t = (theirs as any)[field]
        if (has(spec.lists, field)) {
            result[field] = mergeList(spec.lists[field], spec.lore.has(field), m, t, fieldChoices, fieldChoices.get('') === 'mine')
        } else if (has(spec.maps, field)) {
            const silentKeys = spec.maps[field]
            const out: Record<string, unknown> = {}
            const mineMap = mapEntries(m), theirsMap = mapEntries(t)
            for (const key of new Set([...mineMap.keys(), ...theirsMap.keys()])) {
                const side = silentKeys.has(key) && theirsMap.has(key) ? 'theirs' : fieldChoices.get(key)
                const value = typeof side === 'object' ? chosenValue(side, mineMap.get(key), theirsMap.get(key))
                    : side === 'theirs' ? theirsMap.get(key) : mineMap.get(key)
                if (value !== undefined) out[key] = value
            }
            result[field] = out
        } else if (spec.silent.has(field)) {
            result[field] = t
        } else {
            const side = fieldChoices.get('')
            if (typeof side === 'object') result[field] = chosenValue(side, m, t)
            else if (side === 'theirs') result[field] = t
            else if (side !== 'mine' && m === undefined) result[field] = t
        }
    }
    return result as T
}

/** What gets stored as the base for the next update: the import's own fields. */
export function baseSnapshot<T extends object>(spec: UpdateSpec, value: T): Partial<T> {
    const snapshot: Record<string, unknown> = {}
    for (const [field, item] of Object.entries(value)) {
        if (!spec.preserved.has(field) && item !== undefined) snapshot[field] = item
    }
    if (spec === CHARACTER_UPDATE && snapshot.extentions && typeof snapshot.extentions === 'object') {
        const { [UPDATE_BASE_EXTENSION_KEY]: _key, ...extentions } = snapshot.extentions as Record<string, unknown>
        snapshot.extentions = extentions
    }
    return snapshot as Partial<T>
}

export function summarizePlan(plan: UpdatePlan, choices: Record<string, UpdateChoice | 'edit'> = {}) {
    const counts = { update: 0, mine: 0, added: 0, removed: 0, mineOnly: 0, conflict: 0, merged: 0, differs: 0, unresolved: 0 }
    for (const item of plan.items) {
        counts[item.status]++
        if (!(choices[item.id] ?? item.choice)) counts.unresolved++
    }
    return counts
}
