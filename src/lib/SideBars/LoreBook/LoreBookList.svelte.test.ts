import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { languageEnglish } from 'src/lang/en'
import { DBState } from 'src/ts/stores.svelte'
import type { loreBook } from 'src/ts/storage/database.svelte'
import LoreBookList from './LoreBookList.svelte'
import { IndexedDbPersistentDataStore } from 'src/ts/storage/indexedDbPersistentDataStore'
import { SaveCoordinator } from 'src/ts/storage/saveCoordinator'
import { capturePersistentRoot } from 'src/ts/storage/persistentDataRuntime'

const mocks = vi.hoisted(() => ({ create: vi.fn(), instances: [] as { destroy: ReturnType<typeof vi.fn> }[] }))
vi.mock('sortablejs', () => ({ default: { create: mocks.create } }))
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0) }
})
vi.mock('src/ts/util', () => ({ sortableOptions: {}, sleep: vi.fn() }))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(), alertCheckboxConfirm: vi.fn(), alertError: vi.fn() }))
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/storage/database.svelte', () => ({
    getCurrentCharacter: () => DBState.db.characters[0],
    getCurrentChat: () => DBState.db.characters[0].chats[0],
}))
vi.mock('src/ts/tokenizer', () => ({ tokenizeAccurate: vi.fn(async () => 1) }))
vi.mock('../../Others/Help.svelte', () => ({ default: () => {} }))
vi.mock('../../UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount> | undefined
function book(comment: string, extra: Partial<loreBook> = {}): loreBook {
    return { comment, key: comment, content: '', mode: 'normal', insertorder: 100,
        alwaysActive: false, secondkey: '', selective: false, ...extra }
}
function row(name: string) {
    const label = [...document.querySelectorAll<HTMLButtonElement>('button')].find(button => button.textContent?.trim() === name)!
    return label.parentElement!
}
async function drag(name: string, target: HTMLElement, before: Element | null = null) {
    const item = row(name).parentElement!
    const from = item.parentElement!
    const options = mocks.create.mock.calls.findLast(([element]) => element === from)![1]
    const oldIndex = [...from.children].indexOf(item)
    options.onStart?.({ item, from })
    target.insertBefore(item, before)
    await options.onEnd({ item, from, to: target, oldIndex, newIndex: [...target.children].indexOf(item) })
    await tick()
}
async function remove(name: string) {
    row(name).querySelectorAll<HTMLButtonElement>(':scope > button')[2].click()
    await vi.waitFor(() => expect(document.body.textContent).not.toContain(name))
    await tick()
}

beforeEach(() => {
    alerts.alertConfirm.mockReset().mockResolvedValue(true)
    alerts.alertCheckboxConfirm.mockReset().mockResolvedValue({ confirmed: true, checked: true })
    mocks.create.mockReset()
    mocks.instances = []
    mocks.create.mockImplementation(() => {
        const sortable = { destroy: vi.fn() }
        mocks.instances.push(sortable)
        return sortable
    })
    DBState.db = { characters: [{ globalLore: [], chats: [{ localLore: [] }], chatPage: 0 }], modules: [] } as any
})
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

describe('lorebook editing', () => {
    it('restores dragging and paging after replacing a list with an open entry', async () => {
        DBState.db.characters[0].globalLore = [book('Old')]
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        row('Old').querySelector('button')!.click()
        await tick()
        const creations = mocks.create.mock.calls.length
        DBState.db.characters[0].globalLore = Array.from({ length: 65 }, (_, i) => book(`New ${i}`))
        await tick()
        expect(mocks.create).toHaveBeenCalledTimes(creations + 1)
        const next = document.querySelector<HTMLButtonElement>(`button[aria-label="${languageEnglish.risuNest.pager.next}"]`)!
        expect(next.disabled).toBe(false)
        next.click()
        await tick()
        expect(document.querySelectorAll('[data-risu-idx]')).toHaveLength(5)
    })

    it('reorders a later page using backing indices and preserves hidden entries', async () => {
        const lore = Array.from({ length: 65 }, (_, i) => book(`Entry ${i}`))
        DBState.db.characters[0].globalLore = lore
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        document.querySelector<HTMLButtonElement>(`button[aria-label="${languageEnglish.risuNest.pager.next}"]`)!.click()
        await tick()
        await drag('Entry 64', row('Entry 60').parentElement!.parentElement!, row('Entry 60').parentElement)
        expect(DBState.db.characters[0].globalLore.map(item => item.comment)).toEqual([
            ...lore.slice(0, 60).map(item => item.comment), 'Entry 64', 'Entry 60', 'Entry 61', 'Entry 62', 'Entry 63',
        ])
        expect(document.querySelectorAll('[data-risu-idx]')).toHaveLength(5)
    })

    it('persists module reordering through the owning array and store reopen', async () => {
        DBState.db.modules = [{ id: 'module', name: 'Module', description: '', lorebook: [book('First'), book('Last')] }]
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore('module-lore-drag', indexedDB, IDBKeyRange)
        await store.open()
        const { revision } = await store.replaceFromDatabase({ ...$state.snapshot(DBState.db), characters: [], botPresets: [] })
        const coordinator = new SaveCoordinator({
            store, captureRoot: () => capturePersistentRoot(DBState.db), captureCharacter: () => null,
            captureSelectedCharacter: () => null, replaceDatabase: () => {},
        })
        coordinator.initialize(revision)
        const lore = DBState.db.modules[0].lorebook!
        instance = mount(LoreBookList, { target: document.body, props: { externalLoreBooks: lore } })
        await tick()
        await drag('Last', row('First').parentElement!.parentElement!, row('First').parentElement)
        expect(DBState.db.modules[0].lorebook).toBe(lore)
        coordinator.markPersistentDataDirty(0)
        await coordinator.flushPendingDataLocally('module-lore-drag-test')
        const reopened = new IndexedDbPersistentDataStore('module-lore-drag', indexedDB, IDBKeyRange)
        await reopened.open()
        expect((await reopened.readRoot()).value.modules[0].lorebook?.map(item => item.comment)).toEqual(['Last', 'First'])
    })

    it('keeps expanded folders and child editors mounted when reordering root entries', async () => {
        DBState.db.characters[0].globalLore = [book('Folder', { mode: 'folder', key: 'folder' }),
            book('Child', { folder: 'folder' }), book('First'), book('Last')]
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        row('Folder').querySelector('button')!.click()
        await tick()
        row('Child').querySelector('button')!.click()
        await tick()
        const folderInput = row('Folder').parentElement!.querySelector('input')
        const childInput = row('Child').parentElement!.querySelector('input')
        await drag('Last', row('First').parentElement!.parentElement!, row('First').parentElement)
        expect(DBState.db.characters[0].globalLore.map(item => item.comment)).toEqual(['Folder', 'Child', 'Last', 'First'])
        expect(row('Folder').parentElement!.querySelector('input')).toBe(folderInput)
        expect(row('Child').parentElement!.querySelector('input')).toBe(childInput)
    })

    it('moves entries into and out of an empty folder without closing it', async () => {
        DBState.db.characters[0].globalLore = [book('Folder', { mode: 'folder', key: 'folder' }), book('Move'), book('Keep')]
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        row('Folder').querySelector('button')!.click()
        await tick()
        const root = document.querySelector<HTMLElement>('[data-show-folder=""]')!
        const folder = document.querySelector<HTMLElement>('[data-show-folder="folder"]')!
        await drag('Move', folder)
        expect(row('Move').parentElement!.parentElement).toBe(folder)
        expect(DBState.db.characters[0].globalLore.find(item => item.comment === 'Move')!.folder).toBe('folder')
        await drag('Move', root, row('Keep').parentElement)
        expect(row('Move').parentElement!.parentElement).toBe(root)
        expect(DBState.db.characters[0].globalLore.find(item => item.comment === 'Move')!.folder).toBeUndefined()
        expect(document.querySelector('[data-show-folder="folder"]')).toBe(folder)
    })

    it('keeps the same list and rows after a cancelled drag', async () => {
        DBState.db.characters[0].globalLore = [book('First'), book('Last')]
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        const first = row('First').parentElement!
        await drag('First', first.parentElement!, first.nextElementSibling)
        expect(row('First').parentElement).toBe(first)
        expect(mocks.create).toHaveBeenCalledTimes(1)
    })

    it('removes a module folder and its children from the owning array', async () => {
        DBState.db.modules = [{ id: 'module', name: 'Module', description: '', lorebook: [
            book('Folder', { mode: 'folder', key: 'folder' }), book('Child', { folder: 'folder' }), book('Keep'),
        ] }]
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore('module-lore-editor', indexedDB, IDBKeyRange)
        await store.open()
        const { revision } = await store.replaceFromDatabase({ ...$state.snapshot(DBState.db), characters: [], botPresets: [] })
        const coordinator = new SaveCoordinator({
            store, captureRoot: () => capturePersistentRoot(DBState.db),
            captureCharacter: () => null,
            captureSelectedCharacter: () => null, replaceDatabase: () => {},
        })
        coordinator.initialize(revision)
        const lore = DBState.db.modules[0].lorebook!
        instance = mount(LoreBookList, { target: document.body, props: { externalLoreBooks: lore } })
        await tick()
        await remove('Folder')
        expect(alerts.alertCheckboxConfirm).toHaveBeenCalledExactlyOnceWith({
            title: languageEnglish.removeConfirm + 'Folder',
            description: languageEnglish.checkboxConfirmation.lorebookDeletionDescription,
            checkboxLabel: languageEnglish.checkboxConfirmation.lorebookDeletion,
            actionLabel: languageEnglish.confirm,
            cancelLabel: languageEnglish.cancel,
            requireChecked: true,
        })
        expect(alerts.alertConfirm).not.toHaveBeenCalled()
        expect(lore.map(item => item.comment)).toEqual(['Keep'])
        coordinator.markPersistentDataDirty(0)
        await coordinator.flushPendingDataLocally('module-lore-editor-test')
        const reopened = new IndexedDbPersistentDataStore('module-lore-editor', indexedDB, IDBKeyRange)
        await reopened.open()
        expect((await reopened.readRoot()).value.modules[0].lorebook?.map(item => item.comment)).toEqual(['Keep'])
        await unmount(instance)
        instance = mount(LoreBookList, { target: document.body, props: { externalLoreBooks: lore } })
        await tick()
        expect(document.body.textContent).not.toContain('Folder')
        expect(document.body.textContent).toContain('Keep')
    })

    it('keeps a folder and its children while acknowledgement is pending and after cancellation', async () => {
        const lore = [book('Folder', { mode: 'folder', key: 'folder' }), book('Child', { folder: 'folder' }), book('Keep')]
        DBState.db.characters[0].globalLore = lore
        let confirm!: (answer: { confirmed: boolean; checked: boolean }) => void
        alerts.alertCheckboxConfirm.mockImplementationOnce(() => new Promise(resolve => { confirm = resolve }))
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        row('Folder').querySelectorAll<HTMLButtonElement>(':scope > button')[2].click()
        await vi.waitFor(() => expect(alerts.alertCheckboxConfirm).toHaveBeenCalledOnce())
        expect(DBState.db.characters[0].globalLore.map(item => item.comment)).toEqual(['Folder', 'Child', 'Keep'])
        confirm({ confirmed: false, checked: false })
        await tick()
        expect(DBState.db.characters[0].globalLore.map(item => item.comment)).toEqual(['Folder', 'Child', 'Keep'])
        expect(document.body.textContent).toContain('Folder')
        expect(alerts.alertConfirm).not.toHaveBeenCalled()
    })

    it.each(['character', 'chat', 'module'])('restores %s dragging after deleting a closed entry and opening then closing another', async (scope) => {
        const owner = DBState.db.characters[0]
        owner.globalLore = [book('Delete'), book('Keep')]
        owner.chats[0].localLore = [book('Delete'), book('Keep')]
        instance = mount(LoreBookList, { target: document.body, props: {
            submenu: scope === 'chat' ? 1 : 0,
            externalLoreBooks: scope === 'module' ? owner.globalLore : undefined,
        } })
        await tick()
        await remove('Delete')
        row('Keep').querySelector<HTMLButtonElement>('button')!.click()
        await tick()
        expect(mocks.instances.at(-1)!.destroy).toHaveBeenCalled()
        const creations = mocks.create.mock.calls.length
        row('Keep').querySelector<HTMLButtonElement>('button')!.click()
        await tick()
        expect(mocks.create).toHaveBeenCalledTimes(creations + 1)
    })

    it('keeps dragging disabled for an open detail when an expanded folder is removed', async () => {
        DBState.db.characters[0].globalLore = [book('Folder', { mode: 'folder' }), book('Keep')]
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        row('Folder').querySelector<HTMLButtonElement>('button')!.click()
        row('Keep').querySelector<HTMLButtonElement>('button')!.click()
        await tick()
        const creations = mocks.create.mock.calls.length
        await remove('Folder')
        expect(mocks.create).toHaveBeenCalledTimes(creations)
        row('Keep').querySelector<HTMLButtonElement>('button')!.click()
        await tick()
        expect(mocks.create).toHaveBeenCalledTimes(creations + 1)
    })

    it('toggles a folder and its children without closing their editor', async () => {
        DBState.db.characters[0].globalLore = [book('Folder', { mode: 'folder', key: 'folder' }), book('Child', { folder: 'folder' })]
        instance = mount(LoreBookList, { target: document.body })
        await tick()
        row('Folder').querySelector<HTMLButtonElement>('button')!.click()
        await tick()
        const input = document.querySelector('input')!
        row('Folder').querySelectorAll<HTMLButtonElement>(':scope > button')[1].click()
        await tick()
        expect(DBState.db.characters[0].globalLore.every(item => item.alwaysActive)).toBe(true)
        expect(document.querySelector('input')).toBe(input)
        row('Folder').querySelectorAll<HTMLButtonElement>(':scope > button')[1].click()
        await tick()
        expect(DBState.db.characters[0].globalLore.every(item => !item.alwaysActive)).toBe(true)
    })
})
