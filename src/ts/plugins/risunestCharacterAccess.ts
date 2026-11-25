import { get } from 'svelte/store'
import type { character } from '../storage/database.svelte'
import { appendCharacterIdToOrder, removeCharacterIdFromOrder } from '../storage/characterOrderMutation'
import { defineOwnEnumerableProperty } from '../storage/ownEnumerableProperty'
import { validatePluginCompleteCharacter } from './pluginDatabaseAccess'
import {
    isPlainJsonObject,
    normalizeTarget,
    runDatabaseWrite,
    type RisunestHostContext,
    type RisunestWriteResult,
} from './risunestHostContext'

// Characters by ID: create, move to trash, restore.

/** What a trash or restore did. `archived` and `busy` changed nothing. */
export type RisunestCharacterChange = 'applied' | 'not-found' | 'archived' | 'busy'

export interface RisunestCharacterStore {
    /** The blank character the app creates from scratch, with a new ID. */
    createBlankCharacter(): Promise<character>
    /** Throws the message of the complete-character check `setDatabase` applies. */
    validateCharacter(value: unknown): Promise<void>
    /** Stores a new character at the end of the sidebar order without opening it. */
    addCharacter(value: character): Promise<void>
    trashCharacter(id: string): Promise<RisunestCharacterChange>
    restoreCharacter(id: string): Promise<RisunestCharacterChange>
}

export type RisunestCreateCharacterResult =
    | { status: 'applied'; id: string }
    | { status: 'denied' }
    | { status: 'rejected'; reason: string }

// The host assigns the ID, and a plugin creates neither a group nor a trashed character. Low level
// access is granted only through the confirmation card and module imports ask for.
const hostOwnedFields = new Set(['chaId', 'type', 'trashTime', 'lowLevelAccess', '__proto__', 'prototype', 'constructor'])

function changeResult(change: RisunestCharacterChange): RisunestWriteResult {
    if (change === 'applied' || change === 'not-found') return { status: change }
    return { status: 'rejected', reason: change }
}

export function createRisunestCharacterAccessCore(
    context: Pick<RisunestHostContext, 'hasDatabaseWritePermission' | 'lifetimeSignal'>,
    store: RisunestCharacterStore,
) {
    return {
        async createCharacter(input?: unknown): Promise<RisunestCreateCharacterResult> {
            if (!isPlainJsonObject(input)) throw new TypeError('input must be an object')
            for (const key of Object.keys(input)) {
                if (key !== 'fields') throw new TypeError(`Unknown input key: ${key}`)
            }
            if (input.fields !== undefined && !isPlainJsonObject(input.fields)) {
                throw new TypeError('fields must be an object')
            }
            // Stored as JSON, so the character in memory holds what the store holds.
            const fields = JSON.parse(JSON.stringify(input.fields ?? {})) as Record<string, unknown>
            for (const key of Object.keys(fields)) {
                if (hostOwnedFields.has(key)) return { status: 'rejected', reason: `${key} cannot be set` }
            }
            const candidate = await store.createBlankCharacter()
            for (const [key, value] of Object.entries(fields)) defineOwnEnumerableProperty(candidate, key, value)
            try {
                await store.validateCharacter(candidate)
            } catch (error) {
                return { status: 'rejected', reason: error instanceof Error ? error.message : String(error) }
            }
            return runDatabaseWrite(context, async () => {
                await store.addCharacter(candidate)
                return { status: 'applied' as const, id: candidate.chaId }
            })
        },
        async trashCharacter(target?: unknown): Promise<RisunestWriteResult> {
            const { id } = normalizeTarget(target)
            return runDatabaseWrite(context, async () => changeResult(await store.trashCharacter(id)))
        },
        async restoreCharacter(target?: unknown): Promise<RisunestWriteResult> {
            const { id } = normalizeTarget(target)
            return runDatabaseWrite(context, async () => changeResult(await store.restoreCharacter(id)))
        },
    }
}

// The app's modules load when a method is called, so building the host object evaluates none of them.
async function appModules() {
    const stores = await import('../stores.svelte')
    const runtime = await import('../storage/persistentDataRuntime.svelte')
    const catalog = await import('../storage/workingSetCatalog')
    const characters = await import('../characters')
    const resident = (id: string) => stores.DBState.db.characters.find((value) => value.chaId === id)
    const isOpen = (id: string) => stores.DBState.db.characters[get(stores.selectedCharID)]?.chaId === id
    return { runtime, catalog, characters, resident, isOpen }
}

// Trash and restore decide from the working copy before their commit is queued, so they run one
// at a time and take effect in the order they were called.
let characterChanges: Promise<unknown> = Promise.resolve()
function inOrder<T>(change: () => Promise<T>): Promise<T> {
    const next = characterChanges.then(change)
    characterChanges = next.catch(() => undefined)
    return next
}

const productionCharacterStore: RisunestCharacterStore = {
    async createBlankCharacter() {
        return (await appModules()).characters.createBlankChar()
    },
    async validateCharacter(value) {
        validatePluginCompleteCharacter(value)
    },
    async addCharacter(value) {
        const { runtime, characters } = await appModules()
        // An addition fails after it is stored while the open windowed conversation holds
        // unsaved edits, so those are saved first.
        await runtime.flushPendingData('plugin-create-character')
        await characters.commitDetachedCharacter(value, 'create-character')
    },
    trashCharacter: (id) => inOrder(async () => {
        const { runtime, catalog, characters, resident, isOpen } = await appModules()
        const current = resident(id)
        // The app deletes an archived character for good instead of moving it to the trash.
        if (current && catalog.isArchivedCharacter(current)) return 'archived'
        if (current && current.trashTime === undefined && isOpen(id)) {
            const outcome = await characters.removeConfirmedCharacter(id, 'normal')
            return outcome === 'removed' ? 'applied' : outcome === 'missing' ? 'not-found' : 'busy'
        }
        const changed = await runtime.mutatePersistentCharacterDetail(id, 'character-removal', ({ root, character }) => {
            if (character.trashTime !== undefined) return
            removeCharacterIdFromOrder(root, id)
            character.trashTime = Date.now()
        })
        return changed ? 'applied' : 'not-found'
    }),
    restoreCharacter: (id) => inOrder(async () => {
        const { runtime, catalog, resident } = await appModules()
        const current = resident(id)
        // An archived character is never in the trash.
        if (current && catalog.isArchivedCharacter(current)) return 'applied'
        const changed = await runtime.mutatePersistentCharacterDetail(id, 'character-restore', ({ root, character }) => {
            if (character.trashTime === undefined) return
            delete character.trashTime
            appendCharacterIdToOrder(root, id)
        })
        return changed ? 'applied' : 'not-found'
    }),
}

export function createRisunestCharacterAccess(context: RisunestHostContext) {
    return createRisunestCharacterAccessCore(context, productionCharacterStore)
}
