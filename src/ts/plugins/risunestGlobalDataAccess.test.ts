import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Loadout } from '../loadout'
import type { PersistentDataStore } from '../storage/persistentDataStore'
import { openProductionLibrary } from '../storage/tests/productionLibraryFixture'
import type { PluginDatabaseAccess } from './pluginDatabaseAccess'

const state = vi.hoisted(() => ({
    store: null as PersistentDataStore | null,
    readGranted: true,
    writeGranted: true,
    writeAsked: 0,
}))

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('../gui/receivedDisplaySettings', () => ({ applyReceivedDisplaySettings: vi.fn(async () => undefined) }))
vi.mock('../storage/persistentDataStoreFactory', async (importOriginal) => ({
    ...await importOriginal<typeof import('../storage/persistentDataStoreFactory')>(),
    getPersistentDataStore: () => state.store,
    getRawPersistentDataStore: () => state.store,
}))

afterEach(() => {
    state.readGranted = true
    state.writeGranted = true
    state.writeAsked = 0
})

function loadout(id: string, name: string): Loadout {
    return {
        name,
        id,
        lastUsed: 100,
        favorite: false,
        characterIds: ['char-a'],
        modules: ['module-synthetic'],
        globalVariables: { shared: 'loadout' },
        presetName: 'Preset Alpha',
        personaId: 'persona-synthetic',
        icons: ['char-a.png'],
    }
}

/// The fixture library with two shared variables, one toggle Beta's chat keeps for itself, and two loadouts.
async function openLibrary(name: string) {
    const library = await openProductionLibrary(name, (store) => {
        state.store = store
    }, (initial) => {
        initial.explicitGlobalChatVariables = { shared: 'stored', kept: 'stored', toggle_mode: 'global' }
        initial.globalChatVariables = { ...initial.explicitGlobalChatVariables }
        initial.characters.find((value) => value.chaId === 'char-b')!.chats[0].savedToggleValues = { toggle_mode: 'bound' }
        initial.loadouts = [loadout('loadout-a', 'Synthetic A'), loadout('loadout-b', 'Synthetic B')]
    })
    const { get } = await import('svelte/store')
    const characters = await import('../characters')
    const { getGlobalChatVar } = await import('../parser/chatVar.svelte')
    const { flushPendingData } = await import('../storage/persistentDataRuntime.svelte')
    const { createRisunestGlobalDataAccess } = await import('./risunestGlobalDataAccess')
    const live = () => library.database.getDatabase()
    const access = createRisunestGlobalDataAccess({
        pluginName: 'synthetic-plugin',
        databaseAccess: {} as PluginDatabaseAccess,
        hasDatabasePermission: async () => state.readGranted,
        hasDatabaseWritePermission: async () => {
            state.writeAsked += 1
            return state.writeGranted
        },
        lifetimeSignal: new AbortController().signal,
    })
    return {
        ...library,
        access,
        getGlobalChatVar,
        live,
        /** Opens a character the way the sidebar does, with the chat screen's toggle binding. */
        async open(id: string) {
            expect(await characters.changeChar(live().characters.findIndex((value) => value.chaId === id))).toBe(true)
            const db = live()
            const character = db.characters[get(library.stores.selectedCharID)]
            library.database.flushEffectiveToggleEdits(db)
            library.database.deriveEffectiveToggleVariables(db, character.chats[character.chatPage])
        },
        settle: () => flushPendingData('test-settle'),
        revision: async () => (await library.store.readRoot()).revision,
        storedVariables: async () => (await library.store.readRoot()).value.explicitGlobalChatVariables,
        storedLoadouts: async () => (await library.store.readRoot()).value.loadouts,
    }
}

describe('plugin global variables', { timeout: 30_000 }, () => {
    it('stores what a plugin writes, removes what it sets to null, and the app uses the new values', async () => {
        const library = await openLibrary('risunest-variables-write')
        await library.open('char-a')
        // The app's own edit that is not saved yet.
        library.live().globalChatVariables.edited = 'app'

        expect(await library.access.writeGlobalVariables({ shared: 'plugin', added: 'new', kept: null }))
            .toEqual({ status: 'applied' })

        const written = { shared: 'plugin', added: 'new', toggle_mode: 'global', edited: 'app' }
        expect(await library.storedVariables()).toStrictEqual(written)
        expect(library.getGlobalChatVar('shared')).toBe('plugin')
        expect(library.getGlobalChatVar('added')).toBe('new')
        expect(library.getGlobalChatVar('kept')).toBe('null')
        expect(library.getGlobalChatVar('edited')).toBe('app')
        expect(await library.access.readGlobalVariables()).toStrictEqual({ values: written })
        expect(await library.access.readGlobalVariables({ names: ['added', 'kept'] })).toStrictEqual({ values: { added: 'new' } })
    })

    it("keeps nothing of writes whose commits fail, also when they overlap, and keeps the app's edits made meanwhile", async () => {
        const library = await openLibrary('risunest-variables-failed')
        await library.open('char-b')
        // The first failure arrives a task later, as storage answers, so the second write is called while it is pending.
        // The app changes a variable that write set, and a toggle the open chat keeps, while that commit waits.
        vi.spyOn(library.store, 'commit')
            .mockImplementationOnce(() => new Promise((_, reject) => setTimeout(() => {
                library.live().globalChatVariables.edited = 'app'
                library.live().globalChatVariables.toggle_app = 'app'
                reject(new Error('synthetic storage failure'))
            })))
            .mockRejectedValueOnce(new Error('synthetic storage failure'))

        const first = library.access.writeGlobalVariables({ shared: 'first', added: 'new', kept: null, toggle_mode: 'first', edited: 'first' })
        const second = library.access.writeGlobalVariables({ shared: 'second', toggle_mode: 'second' })
        await expect(first).rejects.toThrow('synthetic storage failure')
        await expect(second).rejects.toThrow('synthetic storage failure')

        expect(library.getGlobalChatVar('shared')).toBe('stored')
        expect(library.getGlobalChatVar('added')).toBe('null')
        expect(library.getGlobalChatVar('kept')).toBe('stored')
        expect(library.getGlobalChatVar('toggle_mode')).toBe('bound')
        expect(library.getGlobalChatVar('edited')).toBe('app')
        expect(library.getGlobalChatVar('toggle_app')).toBe('app')
        await library.settle()
        expect(await library.storedVariables()).toStrictEqual({ shared: 'stored', kept: 'stored', toggle_mode: 'global', edited: 'app' })
        expect((await library.store.readConversation('char-b', 'conv-beta'))!.value.savedToggleValues).toEqual({ toggle_mode: 'bound', toggle_app: 'app' })
    })

    it('changes the shared toggle value and leaves the one the open chat keeps for itself', async () => {
        const library = await openLibrary('risunest-variables-bound-toggle')
        await library.open('char-b')
        expect(library.getGlobalChatVar('toggle_mode')).toBe('bound')

        expect(await library.access.writeGlobalVariables({ toggle_mode: 'plugin', toggle_new: 'on' })).toEqual({ status: 'applied' })

        expect(await library.storedVariables()).toMatchObject({ toggle_mode: 'plugin', toggle_new: 'on' })
        expect((await library.store.readConversation('char-b', 'conv-beta'))!.value.savedToggleValues).toEqual({ toggle_mode: 'bound' })
        expect(library.getGlobalChatVar('toggle_mode')).toBe('bound')
        expect(await library.access.readGlobalVariables({ names: ['toggle_mode'] })).toStrictEqual({ values: { toggle_mode: 'plugin' } })
    })

    it('reads the app\'s unsaved edits', async () => {
        const library = await openLibrary('risunest-variables-read-pending')
        library.live().globalChatVariables.shared = 'app'

        expect(await library.access.readGlobalVariables({ names: ['shared', 'missing'] })).toStrictEqual({ values: { shared: 'app' } })
    })

    it('rejects a bad argument or a value that is not a string before asking, and needs db to read', async () => {
        const library = await openLibrary('risunest-variables-rejected')
        const revision = await library.revision()

        for (const options of [null, ['shared'], { names: 'shared' }, { names: [1] }, { name: ['shared'] }]) {
            await expect(library.access.readGlobalVariables(options)).rejects.toThrow(TypeError)
        }
        for (const set of [undefined, null, ['shared'], 'shared']) {
            await expect(library.access.writeGlobalVariables(set)).rejects.toThrow(TypeError)
        }
        expect(await library.access.writeGlobalVariables({ shared: 'plugin', count: 1 }))
            .toEqual({ status: 'rejected', reason: 'type', key: 'count' })
        // Names the parsed JSON of a plugin call can hold.
        for (const name of ['__proto__', 'constructor']) {
            expect(await library.access.writeGlobalVariables({ shared: 'plugin', ...JSON.parse(`{"${name}":"plugin"}`) }))
                .toEqual({ status: 'rejected', reason: 'unavailable', key: name })
        }
        expect(state.writeAsked).toBe(0)

        state.writeGranted = false
        expect(await library.access.writeGlobalVariables({ shared: 'plugin' })).toEqual({ status: 'denied' })
        state.readGranted = false
        await expect(library.access.readGlobalVariables()).rejects.toThrow('requires the db permission')

        expect(await library.revision()).toBe(revision)
        expect(library.getGlobalChatVar('shared')).toBe('stored')
    })
})

describe('plugin loadouts', { timeout: 30_000 }, () => {
    it('creates a loadout from the current state plus the given fields, last in the app\'s loadout list', async () => {
        const library = await openLibrary('risunest-loadout-create')
        await library.open('char-a')
        const { makeLoadout } = await import('../loadout')
        const defaults: Partial<Loadout> = makeLoadout({ name: '' })
        delete defaults.id
        delete defaults.lastUsed

        const result = await library.access.writeLoadout({ name: 'Synthetic C', favorite: true })

        expect(result).toEqual({ status: 'applied', id: expect.any(String) })
        const id = (result as { id: string }).id
        expect(id).not.toMatch(/^loadout-[ab]$/)
        const created = { ...defaults, id, lastUsed: expect.any(Number), name: 'Synthetic C', favorite: true }
        expect(created.characterIds).toEqual(['char-a'])
        expect(library.live().loadouts.map((value) => value.id)).toEqual(['loadout-a', 'loadout-b', id])
        expect(library.live().loadouts[2]).toEqual(created)
        expect(await library.storedLoadouts()).toEqual([loadout('loadout-a', 'Synthetic A'), loadout('loadout-b', 'Synthetic B'), created])
        expect(await library.access.listLoadouts()).toEqual({ items: await library.storedLoadouts(), total: 3 })
    })

    it('replaces only the named fields of a loadout and deletes one by ID', async () => {
        const library = await openLibrary('risunest-loadout-update')

        expect(await library.access.writeLoadout({ id: 'loadout-a', name: 'Renamed', modules: [] }))
            .toEqual({ status: 'applied', id: 'loadout-a' })

        const updated = { ...loadout('loadout-a', 'Renamed'), modules: [] }
        expect(await library.storedLoadouts()).toEqual([updated, loadout('loadout-b', 'Synthetic B')])
        expect(library.live().loadouts).toEqual([updated, loadout('loadout-b', 'Synthetic B')])

        expect(await library.access.deleteLoadout({ id: 'loadout-b' })).toEqual({ status: 'applied' })

        expect(await library.storedLoadouts()).toEqual([updated])
        expect(library.live().loadouts).toEqual([updated])
        expect(await library.access.listLoadouts()).toEqual({ items: [updated], total: 1 })
    })

    it('finds no unknown ID, rejects a bad argument or field before asking, and needs db to list', async () => {
        const library = await openLibrary('risunest-loadout-rejected')
        const revision = await library.revision()

        expect(await library.access.writeLoadout({ id: 'missing', name: 'Synthetic' })).toEqual({ status: 'not-found' })
        expect(await library.access.deleteLoadout({ id: 'missing' })).toEqual({ status: 'not-found' })
        const asked = state.writeAsked

        for (const input of [undefined, ['loadout-a'], { id: '' }, { id: 1 }]) {
            await expect(library.access.writeLoadout(input)).rejects.toThrow(TypeError)
        }
        await expect(library.access.deleteLoadout('loadout-a')).rejects.toThrow(TypeError)
        expect(await library.access.writeLoadout({ id: 'loadout-a', bogus: 1 }))
            .toEqual({ status: 'rejected', reason: 'unavailable', key: 'bogus' })
        expect(await library.access.writeLoadout({ favorite: 'yes' })).toEqual({ status: 'rejected', reason: 'type', key: 'favorite' })
        expect(await library.access.writeLoadout({ characterIds: ['char-a', 1] }))
            .toEqual({ status: 'rejected', reason: 'type', key: 'characterIds' })
        expect(await library.access.writeLoadout({ globalVariables: { shared: null } }))
            .toEqual({ status: 'rejected', reason: 'type', key: 'globalVariables' })
        expect(await library.access.writeLoadout({ globalVariables: JSON.parse('{"shared":"x","__proto__":"x"}') }))
            .toEqual({ status: 'rejected', reason: 'unavailable', key: 'globalVariables' })
        expect(state.writeAsked).toBe(asked)

        state.writeGranted = false
        expect(await library.access.writeLoadout({ name: 'Synthetic' })).toEqual({ status: 'denied' })
        expect(await library.access.deleteLoadout({ id: 'loadout-a' })).toEqual({ status: 'denied' })
        state.readGranted = false
        await expect(library.access.listLoadouts()).rejects.toThrow('requires the db permission')

        expect(await library.revision()).toBe(revision)
        expect(library.live().loadouts.map((value) => value.id)).toEqual(['loadout-a', 'loadout-b'])
    })
})
