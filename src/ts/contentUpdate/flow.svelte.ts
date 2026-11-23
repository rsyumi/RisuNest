// Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/ts/characterUpdateFlow.svelte.ts), GPL-3.0.

import { get } from 'svelte/store'
import { language } from 'src/lang'
import { alertClear, alertConfirm, alertError, type alertData } from '../alert'
import { loadAsset, saveAsset } from '../globalApi.svelte'
import { DBState, alertStore } from '../stores.svelte'
import type { character } from '../storage/database.svelte'
import type { RisuModule } from '../process/modules'
import type { CharacterDetail } from '../storage/persistentDataStore'
import type { UpdateBaseSource } from './baseFile'
import { generatingConversations } from '../storage/generatingConversationRegistry'
import { isTauriDesktop, isTauriMobile } from '../platform'
import {
    getCatalogConversationCount,
    isArchivedCharacter,
    isCatalogCharacterStub,
    patchWorkingSetCharacterDetail,
} from '../storage/workingSetCatalog'
import {
    assetAliasCleanupTarget,
    mutateCharacterDetailFields,
    mutatePersistentCharacterDetail,
    readPersistentCharacterDetail,
} from '../storage/persistentDataRuntime.svelte'
import { collectOwnedAssetKeys, scheduleDeletedOwnerAssetCleanup } from '../storage/deletedOwnerAssetCleanup'
import { appendCharacterIdToOrder, removeCharacterIdFromOrder } from '../storage/characterOrderMutation'
import { editStartText, editedChoice, isEditable, loreFolderNames, updateGroupOf } from './view'
import {
    CHARACTER_UPDATE,
    MODULE_UPDATE,
    UPDATE_BASE_EXTENSION_KEY,
    applyUpdate,
    baseSnapshot,
    findCharacterUpdateCandidates,
    findModuleUpdateCandidates,
    isInstalledModule,
    planUpdate,
    unresolvedItems,
    type UpdateCandidate,
    type UpdateChoice,
    type UpdateItem,
    type UpdatePlan,
    type UpdateResolution,
    type UpdateSide,
    type UpdateSpec,
} from './merge'

export type UpdateKind = 'character' | 'module'

export interface ContentUpdateCandidate extends UpdateCandidate {
    creator?: string
}

export interface ContentUpdateRequest {
    kind: UpdateKind
    incomingName: string
    candidates: ContentUpdateCandidate[]
    /** Every other installed entity, for choosing one the matcher did not find. */
    others: { id: string, name: string }[]
}

type Decision = { kind: 'new' } | { kind: 'cancel' } | { kind: 'update', id: string, plan: UpdatePlan, choices: Record<string, UpdateChoice> }

/** A resolution, or 'edit' for the text the user wrote (kept in edits). */
export type UpdatePick = UpdateResolution | 'edit'

/** What differs between updating a character and updating a module. */
interface Entity<T extends object> {
    kind: UpdateKind
    spec: UpdateSpec
    /** The installed entity as stored, or null when it is gone. */
    read(id: string): Promise<T | null>
    name(value: T): string
    baseKey(value: T): string | undefined
    /**
     * Format upgrades the app applies on its own, so they do not read as the user's edits.
     * Low-level access left out is off, so the access the user turned on is always compared.
     */
    normalize<P extends Partial<T>>(value: P): Promise<P>
    /** Busy right now: updating would race a running request. */
    busy(id: string): boolean
}

function residentCharacter(id: string): character | undefined {
    return DBState.db.characters.find((value) => value?.chaId === id) as character | undefined
}

/** Characters an update can target: no groups, trash, archived or app-owned characters. */
function updatableCharacters(excludeId: string) {
    const out: { id: string, name: string, count: number }[] = []
    for (const value of DBState.db.characters) {
        if (!value?.chaId || value.chaId === excludeId || value.chaId.startsWith('§')) continue
        if ((value.type as string) === 'group' || value.trashTime || isArchivedCharacter(value)) continue
        out.push({ id: value.chaId, name: value.name ?? '', count: getCatalogConversationCount(value) })
    }
    return out
}

const characters: Entity<CharacterDetail> = {
    kind: 'character',
    spec: CHARACTER_UPDATE,
    read: async (id) => {
        const stored = await readPersistentCharacterDetail(id, 'content-update-read')
        return stored ? structuredClone(stored) : null
    },
    name: (value) => value.name ?? '',
    baseKey: (value) => {
        const key = value.extentions?.[UPDATE_BASE_EXTENSION_KEY]
        return typeof key === 'string' ? key : undefined
    },
    normalize: async (value) => {
        // The upgrades opening a character applies (lorebook version, inlay screen defaults).
        const [{ updateLorebooks }, { updateInlayScreen }] = await Promise.all([import('../characters'), import('../process/inlayScreen')])
        if (Array.isArray(value.globalLore)) value.globalLore = updateLorebooks(value.globalLore)
        if (!value.newGenData && value.viewScreen) updateInlayScreen(value as unknown as character)
        value.lowLevelAccess = !!value.lowLevelAccess
        // The editor and the export read the creator and version from additionalData, which an import
        // fills beside them; they compare as the creator and version items.
        const data = value.additionalData
        if (data && typeof data === 'object') {
            value.creator = String(data.creator ?? '')
            value.characterVersion = String(data.character_version ?? '')
        }
        return value
    },
    // From the start of a request, before a first token or without streaming.
    busy: (id) => generatingConversations.snapshot().some((target) => target.characterId === id),
}

const modules: Entity<RisuModule> = {
    kind: 'module',
    spec: MODULE_UPDATE,
    read: async (id) => {
        const value = (DBState.db.modules ?? []).find((module) => module?.id === id)
        return value ? structuredClone($state.snapshot(value)) as RisuModule : null
    },
    name: (value) => value.name ?? '',
    baseKey: (value) => value.updateBase,
    normalize: async (value) => {
        const { updateLorebooks } = await import('../characters')
        if (Array.isArray(value.lorebook)) value.lorebook = updateLorebooks(value.lorebook)
        value.lowLevelAccess = !!value.lowLevelAccess
        return value
    },
    busy: () => false,
}

class ContentUpdateDialogState {
    request = $state.raw<ContentUpdateRequest | null>(null)
    step = $state<'choose' | 'review'>('choose')
    target = $state('')
    targetName = $state('')
    plan = $state.raw<UpdatePlan | null>(null)
    choices = $state<Record<string, UpdatePick>>({})
    /** Text the user wrote for items picked as 'edit'. */
    edits = $state<Record<string, string>>({})
    /** Lore folder id to title, for grouping entries. */
    folders = $state.raw<Map<string, string>>(new Map())
    /** Where the base came from: the install record, an earlier file, or nowhere. */
    baseSource = $state<{ kind: 'record' | 'file' | 'none', name?: string }>({ kind: 'none' })
    showAll = $state(false)
    /** The target is generating a response: updating now would race it. */
    targetBusy = $state(false)
    busy = $state(false)
    error = $state('')
}

/** State of the "update an installed character or module?" dialog shown while importing. */
export const contentUpdateDialog = new ContentUpdateDialogState()

let pending: { entity: Entity<any>, incoming: object, resolve: (decision: Decision) => void } | null = null

function finish(decision: Decision) {
    const current = pending
    pending = null
    contentUpdateDialog.request = null
    contentUpdateDialog.plan = null
    contentUpdateDialog.choices = {}
    contentUpdateDialog.edits = {}
    current?.resolve(decision)
}

async function loadUpdateBase<T extends object>(entity: Entity<T>, key: string | undefined): Promise<Partial<T> | null> {
    if (!key) return null
    try {
        const data = await loadAsset(key)
        if (!data) return null
        const base = JSON.parse(new TextDecoder().decode(data))
        return base && typeof base === 'object' && !Array.isArray(base) ? await entity.normalize(base) : null
    } catch (error) {
        // A base that did not reach this device compares as no base.
        console.warn('Update base could not be read', error)
        return null
    }
}

/** The import's own fields as the base the next update compares with, taken before normalization. */
function updateBaseData(spec: UpdateSpec, value: object): Uint8Array | null {
    try {
        return new TextEncoder().encode(JSON.stringify(baseSnapshot(spec, value)))
    } catch (error) {
        console.warn('Update base could not be saved', error)
        return null
    }
}

async function saveUpdateBase(data: Uint8Array | null): Promise<string> {
    if (!data) return ''
    try {
        return await saveAsset(data, '', 'base.json')
    } catch (error) {
        console.warn('Update base could not be saved', error)
        return ''
    }
}

/**
 * After an update, deletes the assets among `values` that nothing references any more: the
 * replaced base, and what the merged version did not take from the import or the old module.
 */
function releaseUnusedAssets(startedAtMs: number, ...values: unknown[]) {
    scheduleDeletedOwnerAssetCleanup(collectOwnedAssetKeys(...values), startedAtMs, 'content-update', () => assetAliasCleanupTarget())
}

/**
 * Ends an import the user cancelled: the assets its file saved go once nothing uses them. An import
 * whose operation ended fails with the operation's reason.
 */
function cancelImport(incoming: object, signal: AbortSignal | undefined): null {
    releaseUnusedAssets(Date.now(), incoming)
    signal?.throwIfAborted()
    return null
}

/** Stores the import. When that fails, the base saved for it and the assets its file saved go once nothing uses them. */
async function commitImport<T extends object>(commit: (value: T) => Promise<unknown>, incoming: T): Promise<void> {
    try {
        await commit(incoming)
    } catch (error) {
        releaseUnusedAssets(Date.now(), incoming)
        throw error
    }
}

/**
 * Low-level access the user turned on would also cover the triggers the update
 * takes from the import, so it is confirmed as for an import that asks for it.
 */
async function confirmLowLevelAccess(decision: Decision, triggerField: string): Promise<boolean> {
    if (decision.kind !== 'update') return true
    const taken = (item: UpdateItem) => (decision.choices[item.id] ?? item.choice) !== 'mine'
    const access = decision.plan.items.find((item) => item.field === 'lowLevelAccess')
    // Only a trigger the import brings in runs new code; a removal or a new order does not.
    const brought = (item: UpdateItem) => item.field === triggerField && item.key && item.theirs !== undefined && taken(item)
    if (!access?.mine || taken(access) || !decision.plan.items.some(brought)) return true
    return await alertConfirm(language.lowLevelAccessConfirm)
}

export function selectContentUpdateTarget(id: string) {
    if (contentUpdateDialog.busy) return
    contentUpdateDialog.target = id
}

/** Compares the chosen entity with the import and opens the review step. */
export async function reviewContentUpdate() {
    const dialog = contentUpdateDialog
    const session = pending
    if (!session || dialog.busy) return
    const { entity, incoming } = session
    const target = dialog.target
    dialog.busy = true
    dialog.error = ''
    try {
        const value = await entity.read(target)
        if (!value) throw new Error(language.errors.noData)
        const base = await loadUpdateBase(entity, entity.baseKey(value))
        const mine = await entity.normalize(value)
        // Another import may have opened the dialog meanwhile.
        if (pending !== session || dialog.target !== target) return
        dialog.plan = planUpdate(entity.spec, mine, incoming, base)
        dialog.baseSource = { kind: base ? 'record' : 'none' }
        dialog.folders = loreFolderNames((mine as any).globalLore ?? (mine as any).lorebook, (incoming as any).globalLore ?? (incoming as any).lorebook)
        dialog.choices = {}
        dialog.edits = {}
        dialog.showAll = false
        dialog.targetName = entity.name(mine)
        dialog.targetBusy = entity.busy(target)
        dialog.step = 'review'
    } catch (error) {
        if (pending === session) dialog.error = error instanceof Error ? error.message : String(error)
    } finally {
        if (pending === session) dialog.busy = false
    }
}

export function backToContentUpdateTargets() {
    if (contentUpdateDialog.busy) return
    contentUpdateDialog.step = 'choose'
    contentUpdateDialog.plan = null
    contentUpdateDialog.error = ''
}

/** Both sides together only for an item merged field by field, and an edit only where there is text to rewrite. */
function offersPick(item: UpdateItem, pick: UpdatePick) {
    return pick === 'both' ? item.status === 'merged' : pick === 'edit' ? isEditable(item) : true
}

export function setContentUpdateChoice(id: string, pick: UpdatePick) {
    const dialog = contentUpdateDialog
    const item = dialog.plan?.items.find((entry) => entry.id === id)
    if (!item || !offersPick(item, pick)) return
    // Start from the user's version: usually the one they already rewrote.
    if (pick === 'edit' && dialog.edits[id] === undefined) dialog.edits = { ...dialog.edits, [id]: editStartText(item) }
    dialog.choices = { ...dialog.choices, [id]: pick }
}

export function setContentUpdateEdit(id: string, text: string) {
    contentUpdateDialog.edits = { ...contentUpdateDialog.edits, [id]: text }
}

/**
 * Gives the listed items the same side, for a section or a search result.
 * Items the user rewrote keep their text.
 */
export function chooseContentUpdateMany(ids: string[], side: UpdateSide) {
    const next = { ...contentUpdateDialog.choices }
    for (const id of ids) if (next[id] !== 'edit') next[id] = side
    contentUpdateDialog.choices = next
}

const baseFileExtensions = ['charx', 'png', 'json', 'lorebook', 'risum', 'jpg', 'jpeg']

/**
 * Lets the user pick an earlier file and reads it with `read` while the pick holds it. The native
 * apps pass its path, so its card and module are read without its assets reaching the WebView.
 */
async function pickUpdateBaseFile(read: (file: UpdateBaseSource) => Promise<void>): Promise<void> {
    if (isTauriMobile) {
        const [{ fileAccept }, { withNativeSelectedPath }] = await Promise.all([import('../util'), import('../storage/nativeFileSelection')])
        await withNativeSelectedPath({ accept: fileAccept(baseFileExtensions) }, (file) => read({ name: file.name, path: file.path }))
        return
    }
    if (isTauriDesktop) {
        const [{ fileAccept }, { open }] = await Promise.all([import('../util'), import('../storage/fileSelection')])
        const filters = fileAccept(baseFileExtensions) === '*' ? undefined : [{ name: baseFileExtensions.join(', '), extensions: baseFileExtensions }]
        const path = await open({ multiple: false, directory: false, filters })
        if (typeof path === 'string') await read({ name: path.split(/[\\/]/).at(-1) || path, path })
        return
    }
    const { selectSingleFile } = await import('../util')
    const file = await selectSingleFile(baseFileExtensions)
    if (file) await read(file)
}

/**
 * Uses an earlier file of this character or module as the base, for an install
 * that has no record of the version it came from. The file's assets are not
 * read, so an asset the two sides differ on, a removed one too, is asked about.
 */
export async function loadContentUpdateBaseFile() {
    const dialog = contentUpdateDialog
    const session = pending
    if (!session || dialog.busy) return
    const { entity, incoming } = session
    let reading = false
    try {
        await pickUpdateBaseFile(async (file) => {
            // Another import may have opened the dialog meanwhile.
            if (pending !== session || dialog.busy) return
            const target = dialog.target
            reading = true
            dialog.busy = true
            dialog.error = ''
            const value = await entity.read(target)
            if (!value) throw new Error(language.errors.noData)
            const mine = await entity.normalize(value)
            // The card readers load only when a file is chosen.
            const { readUpdateBaseFile } = await import('./baseFile')
            const base = await entity.normalize(await readUpdateBaseFile(entity.kind, file) as Partial<typeof mine>)
            if (pending !== session || dialog.target !== target) return
            const plan = planUpdate(entity.spec, mine, incoming, base, (field) => updateGroupOf({ field }) === 'asset')
            const items = new Map(plan.items.map((item) => [item.id, item]))
            dialog.plan = plan
            // A pick the item compared with this file no longer offers, such as both on what is now a conflict, is asked again.
            dialog.choices = Object.fromEntries(Object.entries(dialog.choices).filter(([id, pick]) => {
                const item = items.get(id)
                return !!item && offersPick(item, pick)
            }))
            dialog.baseSource = { kind: 'file', name: file.name }
        })
    } catch (error) {
        const unsupported = await import('./baseFile').then(
            ({ UnsupportedUpdateBaseFileError }) => error instanceof UnsupportedUpdateBaseFileError,
            () => false,
        )
        const text = language.risuNest.contentUpdate
        if (pending === session) dialog.error = unsupported
            ? text.baseFileUnsupported
            : `${text.baseFileFailed} ${error instanceof Error ? error.message : String(error)}`
    } finally {
        if (reading && pending === session) dialog.busy = false
    }
}

export function chooseContentUpdate(kind: 'new' | 'cancel' | 'update') {
    const dialog = contentUpdateDialog
    if (!pending || dialog.busy) return
    if (kind !== 'update') return finish({ kind })
    const plan = dialog.plan
    if (!plan) return
    dialog.targetBusy = pending.entity.busy(dialog.target)
    if (dialog.targetBusy) return
    const picks = $state.snapshot(dialog.choices)
    if (unresolvedItems(plan).some((item) => !picks[item.id])) return
    const choices: Record<string, UpdateChoice> = {}
    for (const item of plan.items) {
        const pick = picks[item.id]
        if (pick === 'edit') choices[item.id] = editedChoice(item, dialog.edits[item.id] ?? editStartText(item))
        else if (pick) choices[item.id] = pick
    }
    finish({ kind: 'update', id: dialog.target, plan, choices })
}

async function withCreators(candidates: UpdateCandidate[]): Promise<ContentUpdateCandidate[]> {
    return Promise.all(candidates.slice(0, 20).map(async (candidate) => {
        try {
            const detail = await readPersistentCharacterDetail(candidate.id, 'content-update-candidates')
            const creator = detail?.additionalData?.creator || detail?.creator
            return creator ? { ...candidate, creator } : candidate
        } catch {
            return candidate
        }
    }))
}

function ask<T extends object>(
    entity: Entity<T>,
    incoming: T,
    candidates: ContentUpdateCandidate[],
    others: { id: string, name: string }[],
    forcedId: string | null,
    signal: AbortSignal | undefined,
): Promise<Decision> {
    if (pending) finish({ kind: 'cancel' })
    alertClear()
    const listed = new Set(candidates.map((candidate) => candidate.id))
    const dialog = contentUpdateDialog
    dialog.step = 'choose'
    dialog.target = forcedId ?? candidates[0]?.id ?? ''
    dialog.plan = null
    dialog.choices = {}
    dialog.edits = {}
    dialog.showAll = false
    dialog.targetBusy = false
    dialog.busy = false
    dialog.error = ''
    dialog.request = {
        kind: entity.kind,
        incomingName: entity.name(incoming),
        candidates,
        others: others.filter((other) => !listed.has(other.id)).sort((a, b) => a.name.localeCompare(b.name)),
    }
    const decision = new Promise<Decision>((resolve) => { pending = { entity, incoming, resolve } })
    // An operation that ended, such as one the system stopped in the background, takes its dialog down.
    const session = pending
    const abort = () => { if (pending === session) finish({ kind: 'cancel' }) }
    signal?.addEventListener('abort', abort, { once: true })
    void decision.then(() => signal?.removeEventListener('abort', abort))
    if (signal?.aborted) abort()
    // The update action names its target, so the review opens right away.
    if (forcedId) void reviewContentUpdate()
    return decision
}

/** The loading notice an import shows, which the dialog takes down while it asks. */
function shownLoading(): alertData | null {
    const shown = get(alertStore)
    return shown.type === 'wait' || shown.type === 'progress' ? shown : null
}

/** Runs the rest of an import under the loading notice the dialog took down, so the screen stays covered until it is written. */
async function underLoading<T>(notice: alertData | null, run: () => Promise<T>): Promise<T> {
    if (!notice) return run()
    alertStore.set(notice)
    try {
        return await run()
    } finally {
        alertClear()
    }
}

let forcedTarget: { kind: UpdateKind, id: string } | null = null

/**
 * Runs an import whose first character or module updates the named one
 * instead of being matched by name. Imports after the first match as usual.
 */
export async function importAsContentUpdate<R>(kind: UpdateKind, id: string, run: () => Promise<R>): Promise<R> {
    forcedTarget = { kind, id }
    try {
        return await run()
    } finally {
        forcedTarget = null
    }
}

function takeForcedTarget(kind: UpdateKind): string | null {
    if (forcedTarget?.kind !== kind) return null
    const id = forcedTarget.id
    forcedTarget = null
    return id
}

function replaceDetailFields(target: Record<string, unknown>, source: Record<string, unknown>, fixed: ReadonlySet<string>) {
    for (const key of Object.keys(target)) {
        if (!fixed.has(key) && source[key] === undefined) delete target[key]
    }
    for (const [key, value] of Object.entries(source)) {
        if (!fixed.has(key) && value !== undefined) target[key] = structuredClone(value)
    }
}

// The store never edits a character's type in place; a stored import may not carry one yet.
const characterIdentity = new Set(['chaId', 'type', 'chats', 'chatFolders'])

/**
 * Turns the just-imported character into the trashed copy of the installed
 * one as it was, then writes the merged version to the installed one. The copy
 * goes first so that a failure in between never loses the user's version.
 */
async function foldCharacterUpdate(decision: Extract<Decision, { kind: 'update' }>, incoming: character, baseKey: string): Promise<string> {
    const startedAtMs = Date.now()
    const targetId = decision.id
    const previous = await characters.read(targetId)
    if (!previous) throw new Error(language.errors.noData)

    const text = language.risuNest.contentUpdate
    const backedUp = await mutatePersistentCharacterDetail(incoming.chaId, 'content-update-backup', ({ root, character }) => {
        replaceDetailFields(character as unknown as Record<string, unknown>, previous as unknown as Record<string, unknown>, characterIdentity)
        character.name = `${previous.name ?? ''} ${text.backupSuffix}`.trim()
        // The copy holds one empty chat, so it opens on that one.
        character.chatPage = 0
        if (character.extentions) delete character.extentions[UPDATE_BASE_EXTENSION_KEY]
        removeCharacterIdFromOrder(root, incoming.chaId)
        character.trashTime = Date.now()
    })
    if (!backedUp) throw new Error(language.errors.noData)
    let merged: CharacterDetail | undefined
    const updated = await mutateCharacterDetailFields(targetId, 'content-update', async ({ character }) => {
        // Merged with the character as it is stored now, so what changed while the copy was written stays.
        merged = applyUpdate(CHARACTER_UPDATE, await characters.normalize(character), incoming as CharacterDetail, decision.plan, decision.choices)
        merged.additionalData = { ...(merged.additionalData ?? {}), creator: merged.creator, character_version: merged.characterVersion }
        merged.extentions = { ...(merged.extentions ?? {}) }
        if (baseKey) merged.extentions[UPDATE_BASE_EXTENSION_KEY] = baseKey
        else delete merged.extentions[UPDATE_BASE_EXTENSION_KEY]
        replaceDetailFields(character as unknown as Record<string, unknown>, merged as unknown as Record<string, unknown>, characterIdentity)
    }).catch((error: unknown) => error)
    if (updated !== true) {
        // The write failed or the installed character went away: the import stays a new character.
        await mutatePersistentCharacterDetail(incoming.chaId, 'content-update-restore', ({ root, character }) => {
            replaceDetailFields(character as unknown as Record<string, unknown>, incoming as unknown as Record<string, unknown>, characterIdentity)
            appendCharacterIdToOrder(root, incoming.chaId)
        })
        throw updated instanceof Error ? updated : new Error(language.errors.noData)
    }
    const live = residentCharacter(targetId)
    if (live && merged && isCatalogCharacterStub(live)) patchWorkingSetCharacterDetail(live, merged)
    // The trashed copy keeps the previous card's assets, all but its base.
    releaseUnusedAssets(startedAtMs, incoming, previous.extentions?.[UPDATE_BASE_EXTENSION_KEY])
    return targetId
}

/**
 * Before an imported character commits: offers to update an installed
 * character it looks like a new version of. `commit` stores the import as a
 * new character. Returns the id the import ended as (the updated character
 * when the user chose to update), or null when the user cancelled the import.
 */
export async function commitImportedCharacter(incoming: character, commit: (value: character) => Promise<unknown>, signal?: AbortSignal): Promise<string | null> {
    const forcedId = takeForcedTarget('character')
    const installed = updatableCharacters(incoming.chaId)
    const target = forcedId && installed.some((entry) => entry.id === forcedId) ? forcedId : null
    const candidates = target ? [] : findCharacterUpdateCandidates(installed, incoming.name ?? '')
    const baseData = updateBaseData(CHARACTER_UPDATE, incoming)
    let decision: Decision = { kind: 'new' }
    let loading: alertData | null = null
    if (target || candidates.length) {
        await characters.normalize(incoming)
        const listed = await withCreators(target ? installed.filter((entry) => entry.id === target).map((entry) => ({ ...entry, reason: 'name' as const })) : candidates)
        loading = shownLoading()
        decision = await ask(characters, incoming as CharacterDetail, listed, installed, target, signal)
        if (decision.kind === 'cancel' || !await confirmLowLevelAccess(decision, 'triggerscript')) return cancelImport(incoming, signal)
    }
    const update = decision.kind === 'update' ? decision : null
    return underLoading(loading, async () => {
        if (signal?.aborted) return cancelImport(incoming, signal)
        const baseKey = await saveUpdateBase(baseData)
        if (baseKey) incoming.extentions = { ...(incoming.extentions ?? {}), [UPDATE_BASE_EXTENSION_KEY]: baseKey }
        // An operation that ended while the base was saved stores nothing, and the base goes with the import's assets.
        if (signal?.aborted) return cancelImport(incoming, signal)
        await commitImport(commit, incoming)
        if (!update) return incoming.chaId
        try {
            return await foldCharacterUpdate(update, incoming, baseKey)
        } catch (error) {
            console.error(error)
            // The import is a new character unless it stayed the trashed copy of the installed one.
            const kept = await characters.read(incoming.chaId).catch(() => null)
            const installed = !!kept && !kept.trashTime
            const text = language.risuNest.contentUpdate
            alertError(`${installed ? text.failed : text.notUpdated}\n${error instanceof Error ? error.message : String(error)}`)
            return installed ? incoming.chaId : null
        }
    })
}

/**
 * Replaces the installed module with the merged one and removes the just-imported module.
 * A module the user removed meanwhile leaves the import installed as new.
 */
async function foldModuleUpdate(decision: Extract<Decision, { kind: 'update' }>, incoming: RisuModule, baseKey: string): Promise<string> {
    const startedAtMs = Date.now()
    // Read once the import is stored, so the merge starts from the module as it is now.
    const stored = await modules.read(decision.id)
    if (!stored) return incoming.id
    const merged = applyUpdate(MODULE_UPDATE, await modules.normalize(stored), incoming, decision.plan, decision.choices)
    if (baseKey) merged.updateBase = baseKey
    else delete merged.updateBase
    // The list as it is after the waits above: a received module order puts a new list in its place.
    const list = DBState.db.modules
    const index = list.findIndex((module) => module?.id === decision.id)
    if (index < 0) throw new Error(language.errors.noData)
    list[index] = merged
    const imported = list.findIndex((module) => module?.id === incoming.id)
    if (imported >= 0) list.splice(imported, 1)
    releaseUnusedAssets(startedAtMs, stored, incoming)
    return decision.id
}

/**
 * Before an imported module is added: offers to update an installed module
 * instead. The installed module keeps its id, so the characters, chats and
 * enabled list that point to it keep working. `commit` adds the import as a
 * new module. Returns the id the import ended as, or null when cancelled.
 */
export async function commitImportedModule(incoming: RisuModule, commit: (value: RisuModule) => Promise<unknown>, signal?: AbortSignal): Promise<string | null> {
    const forcedId = takeForcedTarget('module')
    const installed = (DBState.db.modules ?? []).filter((module) => isInstalledModule(module) && module.id !== incoming.id)
    const forced = forcedId ? installed.find((module) => module.id === forcedId) : undefined
    const candidates = forced ? [] : findModuleUpdateCandidates(installed, incoming)
    delete incoming.updateBase
    const baseData = updateBaseData(MODULE_UPDATE, incoming)
    let decision: Decision = { kind: 'new' }
    let loading: alertData | null = null
    if (forced || candidates.length) {
        await modules.normalize(incoming)
        const listed: ContentUpdateCandidate[] = forced
            ? [{ id: forced.id, name: forced.name ?? '', reason: 'name', count: forced.lorebook?.length ?? 0 }]
            : candidates
        loading = shownLoading()
        decision = await ask(modules, incoming, listed, installed.map((module) => ({ id: module.id, name: module.name ?? '' })), forced?.id ?? null, signal)
        if (decision.kind === 'cancel' || !await confirmLowLevelAccess(decision, 'trigger')) return cancelImport(incoming, signal)
    }
    const update = decision.kind === 'update' ? decision : null
    return underLoading(loading, async () => {
        if (signal?.aborted) return cancelImport(incoming, signal)
        const baseKey = await saveUpdateBase(baseData)
        if (baseKey) incoming.updateBase = baseKey
        // An operation that ended while the base was saved stores nothing, and the base goes with the import's assets.
        if (signal?.aborted) return cancelImport(incoming, signal)
        await commitImport(commit, incoming)
        if (!update) return incoming.id
        try {
            return await foldModuleUpdate(update, incoming, baseKey)
        } catch (error) {
            console.error(error)
            alertError(`${language.risuNest.contentUpdate.failed}\n${error instanceof Error ? error.message : String(error)}`)
            return incoming.id
        }
    })
}
