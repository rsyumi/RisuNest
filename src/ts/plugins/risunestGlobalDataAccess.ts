import { get } from 'svelte/store'
import type { Loadout } from '../loadout'
import { defineOwnEnumerableProperty } from '../storage/ownEnumerableProperty'
import {
    isPlainJsonObject,
    jsonKindOf,
    normalizeTarget,
    requireDatabaseRead,
    runDatabaseWrite,
    type RisunestHostContext,
    type RisunestJsonKind,
    type RisunestWriteResult,
} from './risunestHostContext'

// Named global variables and loadouts.

export interface RisunestGlobalDataStore {
    /** The stored global variables, including the app's unsaved edits. */
    readGlobalVariables(): Promise<Record<string, string>>
    /** Sets each named global variable, or removes it for null, in one commit. */
    writeGlobalVariables(entries: [string, string | null][]): Promise<void>
    /** The stored loadouts in the app's order, including the app's unsaved edits. */
    readLoadouts(): Promise<Loadout[]>
    /** The loadout the app saves from the current character, preset, persona, modules and variables. */
    createDefaultLoadout(name: string): Promise<Loadout>
    /**
     * Stores the list `change` returns for the stored loadouts in one commit. Answers false,
     * storing nothing, when `change` returns null.
     */
    changeLoadouts(reason: string, change: (loadouts: readonly Loadout[]) => Loadout[] | null): Promise<boolean>
}

/** A rejection that names the first offending variable name or loadout field. */
export type RisunestKeyedRejection = { status: 'rejected'; reason: string; key: string }

export type RisunestWriteLoadoutResult =
    | { status: 'applied'; id: string }
    | Exclude<RisunestWriteResult, { status: 'applied' }>
    | RisunestKeyedRejection

const loadoutFieldKinds = {
    name: 'string',
    lastUsed: 'number',
    favorite: 'boolean',
    characterIds: 'array',
    modules: 'array',
    globalVariables: 'object',
    presetName: 'string',
    personaId: 'string',
    icons: 'array',
} satisfies Record<Exclude<keyof Loadout, 'id'>, RisunestJsonKind>

/** Checks the loadout fields a plugin gives: every array and object a loadout holds is made of strings. */
function rejectLoadoutFields(fields: Record<string, unknown>): RisunestKeyedRejection | null {
    for (const [key, value] of Object.entries(fields)) {
        if (!Object.hasOwn(loadoutFieldKinds, key)) return { status: 'rejected', reason: 'unavailable', key }
        const kind = jsonKindOf(value)
        if (kind !== loadoutFieldKinds[key as keyof typeof loadoutFieldKinds]) return { status: 'rejected', reason: 'type', key }
        if ((kind === 'array' || kind === 'object') && Object.values(value as object).some((entry) => typeof entry !== 'string')) {
            return { status: 'rejected', reason: 'type', key }
        }
    }
    return null
}

export function createRisunestGlobalDataAccessCore(
    context: Pick<RisunestHostContext, 'hasDatabasePermission' | 'hasDatabaseWritePermission' | 'lifetimeSignal'>,
    store: RisunestGlobalDataStore,
) {
    return {
        async readGlobalVariables(options?: unknown): Promise<{ values: Record<string, string> }> {
            const input = options === undefined ? {} : options
            if (!isPlainJsonObject(input)) throw new TypeError('options must be an object')
            for (const key of Object.keys(input)) {
                if (key !== 'names') throw new TypeError(`Unknown option: ${key}`)
            }
            const { names } = input
            if (names !== undefined && (!Array.isArray(names) || names.some((name) => typeof name !== 'string'))) {
                throw new TypeError('names must be an array of strings')
            }
            await requireDatabaseRead(context, 'risunestReadGlobalVariables')
            const stored = await store.readGlobalVariables()
            const values: Record<string, string> = {}
            for (const name of (names as string[] | undefined) ?? Object.keys(stored)) {
                if (Object.hasOwn(stored, name)) defineOwnEnumerableProperty(values, name, stored[name])
            }
            return { values }
        },
        async writeGlobalVariables(set?: unknown): Promise<RisunestWriteResult | RisunestKeyedRejection> {
            if (!isPlainJsonObject(set)) throw new TypeError('set must be an object')
            const entries = Object.entries(set)
            for (const [key, value] of entries) {
                if (value !== null && typeof value !== 'string') return { status: 'rejected', reason: 'type', key }
            }
            return runDatabaseWrite(context, async () => {
                await store.writeGlobalVariables(entries as [string, string | null][])
                return { status: 'applied' as const }
            })
        },
        async listLoadouts(): Promise<{ items: Loadout[]; total: number }> {
            await requireDatabaseRead(context, 'risunestListLoadouts')
            const items = await store.readLoadouts()
            return { items, total: items.length }
        },
        async writeLoadout(loadout?: unknown): Promise<RisunestWriteLoadoutResult> {
            if (!isPlainJsonObject(loadout)) throw new TypeError('loadout must be an object')
            if (loadout.id !== undefined && (typeof loadout.id !== 'string' || loadout.id.length === 0)) {
                throw new TypeError('id must be a non-empty string')
            }
            const id = loadout.id as string | undefined
            const given = Object.fromEntries(Object.entries(loadout).filter(([key]) => key !== 'id'))
            const rejection = rejectLoadoutFields(given)
            if (rejection) return rejection
            // Stored as JSON, so the record in memory holds what the store holds.
            const fields = JSON.parse(JSON.stringify(given)) as Partial<Loadout>
            return runDatabaseWrite(context, async () => {
                if (id === undefined) {
                    const created: Loadout = JSON.parse(JSON.stringify({
                        ...await store.createDefaultLoadout(fields.name ?? ''),
                        ...fields,
                    }))
                    await store.changeLoadouts('plugin-create-loadout', (current) => [...current, created])
                    return { status: 'applied' as const, id: created.id }
                }
                const changed = await store.changeLoadouts('plugin-update-loadout', (current) => {
                    const index = current.findIndex((value) => value.id === id)
                    if (index < 0) return null
                    const next = [...current]
                    next[index] = { ...current[index], ...fields }
                    return next
                })
                return changed ? { status: 'applied' as const, id } : { status: 'not-found' as const }
            })
        },
        async deleteLoadout(target?: unknown): Promise<RisunestWriteResult> {
            const { id } = normalizeTarget(target)
            return runDatabaseWrite(context, async () => {
                const changed = await store.changeLoadouts('plugin-delete-loadout', (current) => (
                    current.some((value) => value.id === id) ? current.filter((value) => value.id !== id) : null
                ))
                return { status: changed ? 'applied' as const : 'not-found' as const }
            })
        },
    }
}

// The app's modules load when a method is called, so building the host object evaluates none of them.
async function appModules() {
    const stores = await import('../stores.svelte')
    const runtime = await import('../storage/persistentDataRuntime.svelte')
    const database = await import('../storage/database.svelte')
    const { getPersistentDataStore } = await import('../storage/persistentDataStoreFactory')
    return { stores, runtime, database, getPersistentDataStore }
}

async function readStoredRoot(reason: string) {
    const { runtime, getPersistentDataStore } = await appModules()
    await runtime.flushPendingData(reason)
    return (await getPersistentDataStore().readRoot()).value
}

const productionGlobalDataStore: RisunestGlobalDataStore = {
    async readGlobalVariables() {
        const root = await readStoredRoot('plugin-read-global-variables')
        return root.explicitGlobalChatVariables ?? root.globalChatVariables ?? {}
    },
    async writeGlobalVariables(entries) {
        const { stores, runtime, database } = await appModules()
        const { isConversationSummaryStub } = await import('../storage/conversationResidency')
        const db = stores.DBState.db
        // The app's unsaved variable edits reach the shared set or the bound conversation first.
        database.flushEffectiveToggleEdits(db)
        // The shared set is edited directly: a toggle sent through the unit commit would go to
        // the conversation the open chat binds.
        const shared = database.getExplicitGlobalChatVariables(db)
        for (const [name, value] of entries) {
            if (value === null) delete shared[name]
            else {
                if (!Object.hasOwn(shared, name) && name in shared) defineOwnEnumerableProperty(shared, name, value)
                shared[name] = value
            }
        }
        // The variables the app uses follow the shared set, except the toggles the open chat binds.
        const character = db.characters[get(stores.selectedCharID)]
        const chat = character?.chats?.[character.chatPage]
        database.deriveEffectiveToggleVariables(db, chat && !isConversationSummaryStub(chat) ? chat : undefined)
        await runtime.flushPendingData('plugin-write-global-variables')
    },
    async readLoadouts() {
        return (await readStoredRoot('plugin-read-loadouts')).loadouts ?? []
    },
    async createDefaultLoadout(name) {
        await appModules()
        const { makeLoadout } = await import('../loadout')
        return makeLoadout({ name })
    },
    async changeLoadouts(reason, change) {
        const { runtime } = await appModules()
        const { diffRecordCollection } = await import('../storage/persistentUnitCapture')
        let changed = false
        await runtime.commitPreparedUnitIntent(reason, async (reader) => {
            const before = (await reader.readRoot()).value.loadouts ?? []
            const after = change(before)
            changed = after !== null
            return after && { unitMutations: diffRecordCollection('loadouts', before, after), conversations: [] }
        })
        return changed
    },
}

export function createRisunestGlobalDataAccess(context: RisunestHostContext) {
    return createRisunestGlobalDataAccessCore(context, productionGlobalDataStore)
}
