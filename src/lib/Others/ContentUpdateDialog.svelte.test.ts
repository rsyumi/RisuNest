import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, tick, unmount } from 'svelte'

const mocks = vi.hoisted(() => ({
    details: new Map<string, any>(),
    db: { characters: [] as any[], modules: [] as any[] },
}))

vi.mock('src/ts/globalApi.svelte', () => ({
    loadAsset: async () => null,
    saveAsset: async () => 'assets/base.json',
}))
vi.mock('src/ts/alert', () => ({ alertClear: vi.fn(), alertError: vi.fn(), alertConfirm: vi.fn() }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: mocks.db } }))
vi.mock('src/ts/characters', () => ({ updateLorebooks: (books: unknown[]) => books }))
vi.mock('src/ts/process/inlayScreen', () => ({ updateInlayScreen: (value: unknown) => value }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    readPersistentCharacterDetail: async (id: string) => structuredClone(mocks.details.get(id) ?? null),
    mutatePersistentCharacterDetail: async (id: string, _reason: string, mutate: (state: any) => void) => {
        mutate({ root: { characterOrder: [] }, character: mocks.details.get(id) })
        return true
    },
    mutateCharacterDetailFields: async (id: string, _reason: string, mutate: (state: any) => void) => {
        mutate({ character: mocks.details.get(id) })
        return true
    },
}))

import { language } from 'src/lang'
import ContentUpdateDialog from './ContentUpdateDialog.svelte'
import { commitImportedCharacter, contentUpdateDialog } from 'src/ts/contentUpdate/flow.svelte'
import { registerGeneratingConversation } from 'src/ts/storage/generatingConversationRegistry'

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

const button = (label: string) => [...document.querySelectorAll('button')].find((element) => element.textContent?.trim() === label) as HTMLButtonElement

it('names fields as the character editor does and updates once every difference has a side', async () => {
    const regex = (comment: string) => ({ comment, type: 'editoutput', in: comment, out: '' })
    const installed = { type: 'character', chaId: 'installed', name: 'Luna', creatorNotes: 'old note', customscript: [regex('A'), regex('B')], chats: [{ id: 'chat' }], extentions: {} }
    mocks.details.set('installed', structuredClone(installed))
    mocks.db.characters = [installed]
    instance = mount(ContentUpdateDialog, { target: document.body })
    const commit = vi.fn(async (value: any) => { mocks.details.set(value.chaId, structuredClone(value)) })
    const result = commitImportedCharacter({ type: 'character', chaId: 'incoming', name: 'Luna v2', creatorNotes: 'new note', customscript: [regex('B'), regex('A')], chats: [], extentions: {} } as any, commit)
    const text = language.risuNest.contentUpdate

    await vi.waitFor(() => expect(button(text.next)).toBeTruthy())
    button(text.next).click()
    await vi.waitFor(() => expect(contentUpdateDialog.step).toBe('review'))
    await tick()
    expect(document.body.textContent).toContain(language.creatorNotes)
    expect(document.querySelector('[data-update-item="customscript"]')?.textContent).toContain(`${language.regexScript} ${text.listOrder}`)
    expect(button(text.update).disabled).toBe(true)

    button(text.scopeTheirs).click()
    flushSync()
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
