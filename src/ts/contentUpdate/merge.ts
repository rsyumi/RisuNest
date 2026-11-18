// Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/ts/characterUpdate.ts), GPL-3.0.

import type { loreBook } from '../storage/database.svelte'
import type { RisuModule } from '../process/modules'

// Finds an installed character or module that an import is a new version of,
// and merges the two against the import as it was first installed (the base)
// so that edits the user made are kept. Pure functions only: the dialog and
// storage live in flow.svelte.ts.

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
}

const loreKey = (entry: loreBook) => JSON.stringify([entry?.mode ?? '', entry?.comment ?? '', entry?.key ?? '', entry?.secondkey ?? ''])
// A named regex stays the same item when the author changes its pattern.
const regexKey = (script: any) => JSON.stringify(script?.comment ? [script.comment, script.type ?? ''] : ['', script?.type ?? '', script?.in ?? ''])
const triggerKey = (script: any) => JSON.stringify([script?.comment ?? '', script?.type ?? ''])
const assetKey = (asset: any) => String(asset?.[0] ?? '')

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
}

export const MODULE_UPDATE: UpdateSpec = {
    // The id is what characters, chats and the enabled list point to.
    preserved: new Set(['id', 'updateBase']),
    silent: new Set(['updateLineage']),
    lists: { lorebook: loreKey, regex: regexKey, trigger: triggerKey, assets: assetKey },
    maps: {},
    lore: new Set(['lorebook']),
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
    | 'differs'   // no base: who changed it is unknown

export type UpdateSide = 'mine' | 'theirs'
/** A side, or a value the user wrote while comparing the two. */
export type UpdateChoice = UpdateSide | { value: unknown }

export interface UpdateItem {
    id: string
    field: string
    /** Item key inside a list or the extensions map; empty for a whole field. */
    key: string
    status: UpdateItemStatus
    mine: unknown
    theirs: unknown
    /** The value as first installed; meaningful only when baseKnown. */
    base: unknown
    baseKnown: boolean
    /** Preselected side, or null when the user has to decide. */
    choice: UpdateSide | null
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

/**
 * Compares the installed entity (mine), the import (theirs) and the import as
 * it was installed (base, null when unknown). Fields the import does not set
 * and the user's own data are left out.
 */
export function planUpdate<T extends object>(spec: UpdateSpec, mine: T, theirs: T, base: Partial<T> | null): UpdatePlan {
    const hasBase = !!base
    const items: UpdateItem[] = []
    const push = (field: string, key: string, values: { b?: unknown, m?: unknown, t?: unknown }, present: { mine: boolean, theirs: boolean, base: boolean }) => {
        const verdict = decide(hasBase, canon(spec, field, values.b), canon(spec, field, values.m), canon(spec, field, values.t), present)
        if (!verdict || spec.silent.has(field)) return
        items.push({ id: key ? `${field}\u0000${key}` : field, field, key, mine: values.m, theirs: values.t, base: values.b, baseKnown: hasBase && present.base, ...verdict })
    }
    for (const field of Object.keys(theirs)) {
        if (spec.preserved.has(field)) continue
        const m = (mine as any)[field]
        const t = (theirs as any)[field]
        const b = base ? (base as any)[field] : undefined
        const keyOf = has(spec.lists, field) ? spec.lists[field] : null
        const silentKeys = has(spec.maps, field) ? spec.maps[field] : null
        if (keyOf || silentKeys) {
            const read = keyOf ? (value: unknown) => keyed(keyOf, value) : mapEntries
            const mineMap = read(m), theirsMap = read(t), baseMap = read(b)
            for (const key of new Set([...theirsMap.keys(), ...mineMap.keys(), ...baseMap.keys()])) {
                if (!mineMap.has(key) && !theirsMap.has(key)) continue
                if (silentKeys?.has(key)) continue
                push(field, key, { b: baseMap.get(key), m: mineMap.get(key), t: theirsMap.get(key) },
                    { mine: mineMap.has(key), theirs: theirsMap.has(key), base: baseMap.has(key) })
            }
            continue
        }
        push(field, '', { b, m, t }, { mine: true, theirs: true, base: true })
    }
    return { hasBase, items }
}

export function unresolvedItems(plan: UpdatePlan): UpdateItem[] {
    return plan.items.filter(item => item.choice === null)
}

/** Puts mine-only items back after the item they followed in the user's list. */
function mergeList(keyOf: (item: any) => string, lore: boolean, mineList: unknown, theirsList: unknown, choices: Map<string, UpdateChoice>): unknown[] {
    const mineMap = keyed(keyOf, mineList)
    const theirsMap = keyed(keyOf, theirsList)
    const keepId = (chosen: unknown, own: unknown) => lore && own && chosen && typeof chosen === 'object'
        && (own as { id?: string }).id ? { ...(chosen as object), id: (own as { id?: string }).id } : chosen
    const pick = (key: string): unknown => {
        const own = mineMap.get(key)
        const other = theirsMap.get(key)
        const choice = choices.get(key)
        if (!choice) return own ?? other
        if (typeof choice === 'object') return keepId(choice.value, own)
        return choice === 'mine' ? own : keepId(other, own)
    }
    const result: { key: string, value: unknown }[] = []
    for (const key of theirsMap.keys()) {
        const value = pick(key)
        if (value !== undefined) result.push({ key, value })
    }
    let previous: string | null = null
    for (const key of mineMap.keys()) {
        if (!theirsMap.has(key)) {
            const value = pick(key)
            if (value !== undefined) {
                const at = previous === null ? -1 : result.findIndex(entry => entry.key === previous)
                result.splice(at + 1, 0, { key, value })
            }
        }
        if (result.some(entry => entry.key === key)) previous = key
    }
    return result.map(entry => entry.value)
}

/**
 * The installed entity with the update applied by the plan's choices. Every
 * unresolved item must have a choice. Preserved fields stay as they are.
 */
export function applyUpdate<T extends object>(spec: UpdateSpec, mine: T, theirs: T, plan: UpdatePlan, choices: Record<string, UpdateChoice> = {}): T {
    const chosen = new Map<string, Map<string, UpdateChoice>>()
    for (const item of plan.items) {
        const side = choices[item.id] ?? item.choice
        if (!side) throw new Error(`Update choice missing for ${item.id}`)
        if (!chosen.has(item.field)) chosen.set(item.field, new Map())
        chosen.get(item.field)!.set(item.key, side)
    }
    const result: Record<string, any> = { ...mine }
    for (const field of Object.keys(theirs)) {
        if (spec.preserved.has(field)) continue
        const fieldChoices = chosen.get(field) ?? new Map<string, UpdateChoice>()
        const m = (mine as any)[field]
        const t = (theirs as any)[field]
        if (has(spec.lists, field)) {
            result[field] = mergeList(spec.lists[field], spec.lore.has(field), m, t, fieldChoices)
        } else if (has(spec.maps, field)) {
            const silentKeys = spec.maps[field]
            const out: Record<string, unknown> = {}
            const mineMap = mapEntries(m), theirsMap = mapEntries(t)
            for (const key of new Set([...mineMap.keys(), ...theirsMap.keys()])) {
                const side = silentKeys.has(key) && theirsMap.has(key) ? 'theirs' : fieldChoices.get(key)
                const value = typeof side === 'object' ? side.value : side === 'theirs' ? theirsMap.get(key) : side === 'mine' ? mineMap.get(key) : mineMap.get(key) ?? theirsMap.get(key)
                if (value !== undefined) out[key] = value
            }
            result[field] = out
        } else if (spec.silent.has(field)) {
            result[field] = t
        } else {
            const side = fieldChoices.get('')
            if (typeof side === 'object') result[field] = side.value
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
    const counts = { update: 0, mine: 0, added: 0, removed: 0, mineOnly: 0, conflict: 0, differs: 0, unresolved: 0 }
    for (const item of plan.items) {
        counts[item.status]++
        if (!(choices[item.id] ?? item.choice)) counts.unresolved++
    }
    return counts
}
