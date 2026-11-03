import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Database } from './storage/database.svelte'
import type { PersistentDataStore } from './storage/persistentDataStore'
import { IndexedDbPersistentDataStore } from './storage/indexedDbPersistentDataStore'
import { fixtureDatabase } from './storage/tests/persistentDataFixtures'

const state = vi.hoisted(() => ({
    store: null as PersistentDataStore | null,
    cleanup: [] as (() => void | Promise<void>)[],
}))

vi.mock('./parser/parser.svelte', async () => (await import('./parser/parser.testSupport')).parserStubModule())
vi.mock('./gui/receivedDisplaySettings', () => ({ applyReceivedDisplaySettings: vi.fn(async () => undefined) }))
vi.mock('./storage/persistentDataStoreFactory', async (importOriginal) => ({
    ...await importOriginal<typeof import('./storage/persistentDataStoreFactory')>(),
    getPersistentDataStore: () => state.store,
    getRawPersistentDataStore: () => state.store,
}))

afterEach(async () => {
    for (const cleanup of state.cleanup.splice(0).reverse()) await cleanup()
    document.body.replaceChildren()
})

/// Opens the first fixture character in the production runtime with its
/// character tab mounted. Each call loads a fresh module graph because the
/// production runtime is a module singleton bound to the first store it sees.
async function openSelectedCharacterTab(name: string) {
    vi.resetModules()
    const { mount, tick, unmount } = await import('svelte')
    const { get } = await import('svelte/store')
    // stores.svelte must load before characters: the reverse order lets the
    // stores module effect read database.svelte before it is initialized.
    const stores = await import('./stores.svelte')
    const database = await import('./storage/database.svelte')
    const characters = await import('./characters')
    const navigation = await import('src/lib/workingSetNavigation')
    const { getPersistentDataRuntime } = await import('./storage/persistentDataRuntime.svelte')
    const { projectCompleteScalableWorkingSet } = await import('./storage/workingSetCatalog')
    const { default: Harness } = await import('src/lib/SideBars/SelectedCharacterNoteHarness.test.svelte')
    const { noteReads } = await import('src/lib/SideBars/SelectedCharacterNoteReader.test.svelte')

    const initial = database.normalizeDatabaseDefaults(structuredClone(fixtureDatabase) as Database)
    initial.characters = initial.characters.slice(0, 2)
    initial.characters[0].chats[0].note = 'Synthetic note'
    initial.characterOrder = initial.characters.map((character) => character.chaId)
    const [selected, other] = initial.characters
    const store = new IndexedDbPersistentDataStore(name, new IDBFactory(), IDBKeyRange)
    await store.open()
    state.store = store
    const { revision } = await store.replaceFromDatabase(initial)
    database.setDatabaseLite(projectCompleteScalableWorkingSet(initial, null, revision, new Set()))
    stores.selectedCharID.set(-1)
    await getPersistentDataRuntime().initializeActiveWorkingSet(database.getDatabase())
    expect(await characters.changeChar(0)).toBe(true)
    expect(get(stores.selectedCharID)).toBe(0)

    // Answers the removal dialog the way its checkbox and confirm button do.
    state.cleanup.push(stores.alertStore.subscribe((data) => {
        if (data.type !== 'checkboxConfirm') return
        data.onCheckboxConfirm?.({ confirmed: true, checked: true })
        queueMicrotask(() => stores.alertStore.set({ type: 'none', msg: '' }))
    }))
    const target = document.createElement('div')
    document.body.append(target)
    const harness = mount(Harness, { target })
    state.cleanup.push(() => unmount(harness))
    await vi.waitFor(() => expect(target.textContent).toContain(selected.name))
    expect(noteReads.at(-1)).toEqual({ id: selected.chaId, stub: false })
    noteReads.length = 0

    return {
        store,
        target,
        selected,
        other,
        noteReads,
        tick,
        removeChar: characters.removeChar,
        clearCharacterSelection: navigation.clearCharacterSelection,
        selectedIndex: () => get(stores.selectedCharID),
        live: () => database.getDatabase(),
    }
}

// The first case transforms the whole runtime graph after resetting modules.
describe('leaving a selected character with its character tab open', { timeout: 30_000 }, () => {
    it('moves it to the trash without the tab reading a released stub', async () => {
        const opened = await openSelectedCharacterTab('selected-removal-trash')

        await opened.removeChar(0, opened.selected.name)
        await opened.tick()

        expect(opened.noteReads).not.toContainEqual(expect.objectContaining({ stub: true }))
        expect(opened.selectedIndex()).toBe(-1)
        expect(opened.target.textContent).toBe('')
        const stored = await opened.store.readCharacter(opened.selected.chaId)
        expect(stored?.value.trashTime).toEqual(expect.any(Number))
        expect((await opened.store.readRoot()).value.characterOrder).toEqual([opened.other.chaId])
        expect(opened.live().characters.find((character) => character.chaId === opened.selected.chaId)?.trashTime)
            .toBe(stored?.value.trashTime)
    })

    it.each(['permanent', 'permanentForce'] as const)('deletes it (%s) without the tab reading a released stub', async (type) => {
        const opened = await openSelectedCharacterTab(`selected-removal-${type}`)

        await opened.removeChar(opened.selected.chaId, opened.selected.name, type)
        await opened.tick()

        expect(opened.noteReads).not.toContainEqual(expect.objectContaining({ stub: true }))
        expect(opened.selectedIndex()).toBe(-1)
        expect(opened.target.textContent).toBe('')
        expect(await opened.store.readCharacter(opened.selected.chaId)).toBeNull()
        expect(opened.live().characters.map((character) => character.chaId)).toEqual([opened.other.chaId])
    })

    it('permanently deletes a character after moving it to the trash', async () => {
        const opened = await openSelectedCharacterTab('selected-removal-from-trash')
        await opened.removeChar(opened.selected.chaId, opened.selected.name)
        await opened.tick()

        await opened.removeChar(opened.selected.chaId, opened.selected.name, 'permanent')

        expect(opened.selectedIndex()).toBe(-1)
        expect(await opened.store.readCharacter(opened.selected.chaId)).toBeNull()
        expect(opened.live().characters.map((character) => character.chaId)).toEqual([opened.other.chaId])
    })

    it('goes home without the tab reading a released stub', async () => {
        const opened = await openSelectedCharacterTab('selected-removal-home')

        await expect(opened.clearCharacterSelection()).resolves.toBe(true)
        await opened.tick()

        expect(opened.noteReads).not.toContainEqual(expect.objectContaining({ stub: true }))
        expect(opened.selectedIndex()).toBe(-1)
        expect(opened.target.textContent).toBe('')
        expect((await opened.store.readCharacter(opened.selected.chaId))?.value.trashTime).toBeUndefined()
    })
})
