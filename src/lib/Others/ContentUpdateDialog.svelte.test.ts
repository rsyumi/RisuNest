import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, tick, unmount } from 'svelte'

const mocks = vi.hoisted(() => ({
    details: new Map<string, any>(),
    db: { characters: [] as any[], modules: [] as any[] },
    assets: new Map<string, Uint8Array>(),
}))

vi.mock('src/ts/globalApi.svelte', () => ({
    loadAsset: async (key: string) => mocks.assets.get(key) ?? null,
    saveAsset: async () => 'assets/base.json',
}))
vi.mock('src/ts/alert', () => ({ alertClear: vi.fn(), alertError: vi.fn(), alertConfirm: vi.fn() }))
vi.mock('src/ts/stores.svelte', async () => ({ DBState: { db: mocks.db }, alertStore: (await import('svelte/store')).writable({ type: 'none', msg: '' }) }))
vi.mock('src/ts/characters', () => ({ updateLorebooks: (books: unknown[]) => books }))
vi.mock('src/ts/process/inlayScreen', () => ({ updateInlayScreen: (value: unknown) => value }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    readPersistentCharacterDetail: async (id: string) => structuredClone(mocks.details.get(id) ?? null),
    mutatePersistentCharacterDetail: async (id: string, _reason: string, mutate: (state: any) => void) => {
        mutate({ root: { characterOrder: [] }, character: mocks.details.get(id) })
        return true
    },
    mutateCharacterDetailFields: async (id: string, _reason: string, mutate: (state: any) => void | Promise<void>) => {
        await mutate({ character: mocks.details.get(id) })
        return true
    },
}))

import { language } from 'src/lang'
import ContentUpdateDialog from './ContentUpdateDialog.svelte'
import { commitImportedCharacter, contentUpdateDialog } from 'src/ts/contentUpdate/flow.svelte'
import { registerGeneratingConversation } from 'src/ts/storage/generatingConversationRegistry'
import { modalNavigation } from 'src/ts/ui/modalNavigation'

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

const button = (label: string) => [...document.querySelectorAll('button')].find((element) => element.textContent?.trim() === label) as HTMLButtonElement

it('names fields as the character editor does and updates once every difference has a side', async () => {
    const regex = (comment: string) => ({ comment, type: 'editoutput', in: comment, out: '' })
    const installed = { type: 'character', chaId: 'installed', name: 'Luna', creatorNotes: 'old note', utilityBot: false, customscript: [regex('A'), regex('B')], chats: [{ id: 'chat' }], extentions: {} }
    mocks.details.set('installed', structuredClone(installed))
    mocks.db.characters = [installed]
    instance = mount(ContentUpdateDialog, { target: document.body })
    const commit = vi.fn(async (value: any) => { mocks.details.set(value.chaId, structuredClone(value)) })
    const result = commitImportedCharacter({ type: 'character', chaId: 'incoming', name: 'Luna v2', creatorNotes: 'new note', utilityBot: true, customscript: [regex('B'), regex('A')], chats: [], extentions: {} } as any, commit)
    const text = language.risuNest.contentUpdate

    await vi.waitFor(() => expect(button(text.next)).toBeTruthy())
    button(text.next).click()
    await vi.waitFor(() => expect(contentUpdateDialog.step).toBe('review'))
    await tick()
    expect(document.body.textContent).toContain(language.creatorNotes)
    expect(document.querySelector('[data-update-item="customscript"]')?.textContent).toContain(`${language.regexScript} ${text.listOrder}`)
    expect(document.querySelector('[data-update-item="utilityBot"]')?.textContent).toContain(language.utilityBot)
    expect(button(text.update).disabled).toBe(true)

    button(text.scopeTheirs).click()
    flushSync()
    // Each side shows as a button that reads as pressed once chosen.
    expect(document.querySelector('[data-update-item="utilityBot"] button[aria-pressed="true"]')?.textContent?.trim()).toBe(text.takeTheirs)
    expect(button(text.update).disabled).toBe(false)
    // A request for the character holds the update back only until it ends.
    const release = registerGeneratingConversation({ characterId: 'installed', conversationId: 'chat' })
    button(text.update).click()
    flushSync()
    expect(document.body.textContent).toContain(text.busy)
    release()
    button(text.update).click()
    expect(await result).toBe('installed')
    expect(mocks.details.get('installed')).toMatchObject({ name: 'Luna v2', creatorNotes: 'new note', customscript: [regex('B'), regex('A')], chats: [{ id: 'chat' }] })
})

it('applies both changes to an entry merged field by field and shows what the author removed', async () => {
    const lore = (comment: string, content: string, more = {}) => ({ key: comment.toLowerCase(), secondkey: '', comment, mode: 'normal', insertorder: 100, alwaysActive: false, selective: false, content, ...more })
    mocks.assets.set('assets/old.json', new TextEncoder().encode(JSON.stringify({ name: 'Luna', globalLore: [lore('Town', 'town'), lore('Inn', 'inn')] })))
    const installed = { type: 'character', chaId: 'installed', name: 'Luna', globalLore: [lore('Town', '마을'), lore('Inn', '여관')], chats: [], extentions: { risunestUpdateBase: 'assets/old.json' } }
    mocks.details.set('installed', structuredClone(installed))
    mocks.db.characters = [installed]
    instance = mount(ContentUpdateDialog, { target: document.body })
    const commit = vi.fn(async (value: any) => { mocks.details.set(value.chaId, structuredClone(value)) })
    // The author raised the Town entry's order and removed the Inn entry the user had rewritten.
    const result = commitImportedCharacter({ type: 'character', chaId: 'incoming', name: 'Luna', globalLore: [lore('Town', 'town', { insertorder: 300 })], chats: [], extentions: {} } as any, commit)
    const text = language.risuNest.contentUpdate
    await vi.waitFor(() => expect(button(text.next)).toBeTruthy())
    button(text.next).click()
    await vi.waitFor(() => expect(contentUpdateDialog.step).toBe('review'))
    await tick()
    button(text.viewAll(2)).click()
    flushSync()
    const row = (name: string) => [...document.querySelectorAll('[data-update-item]')].find((element) => element.textContent?.includes(name)) as HTMLElement
    expect(row('Town').querySelector('button[aria-pressed="true"]')?.textContent?.trim()).toBe(text.takeBoth)
    // The first item to decide starts open.
    expect(row('Inn').querySelector('button[aria-expanded]')?.getAttribute('aria-expanded')).toBe('true')
    expect(row('Inn').textContent).toContain(text.authorChange)
    ;[...row('Inn').querySelectorAll('button')].find((element) => element.textContent?.trim() === text.keepMine)!.click()
    flushSync()
    button(text.update).click()
    expect(await result).toBe('installed')
    expect(mocks.details.get('installed').globalLore.map((entry: any) => [entry.comment, entry.content, entry.insertorder])).toEqual([['Town', '마을', 300], ['Inn', '여관', 100]])
})

it('takes Escape from the screen it opened over and cancels the import', async () => {
    // A screen with its own navigation layer, such as the module settings the update action opens from.
    const behind = document.body.appendChild(document.createElement('div'))
    const closeBehind = vi.fn()
    const layer = modalNavigation(behind, { close: closeBehind })
    mocks.db.characters = [{ type: 'character', chaId: 'installed', name: 'Luna', chats: [] }]
    instance = mount(ContentUpdateDialog, { target: document.body })
    const commit = vi.fn()
    const result = commitImportedCharacter({ type: 'character', chaId: 'incoming', name: 'Luna v2', chats: [], extentions: {} } as any, commit)
    await vi.waitFor(() => expect(button(language.risuNest.contentUpdate.next)).toBeTruthy())

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    expect(closeBehind).not.toHaveBeenCalled()
    expect(await result).toBeNull()
    expect(commit).not.toHaveBeenCalled()
    layer.destroy()
})
