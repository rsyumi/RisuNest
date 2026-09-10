import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import ToggleBind from './ToggleBind.svelte'

const mocks = vi.hoisted(() => ({
    confirm: vi.fn(async () => true),
    toast: vi.fn(),
    error: vi.fn(),
    save: vi.fn(async () => {}),
    popup: vi.fn(),
}))
vi.mock('src/lang', () => ({ language: languageEnglish }))
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0) }
})
vi.mock('src/ts/alert', () => ({
    alertConfirm: mocks.confirm,
    alertToast: mocks.toast,
    alertError: mocks.error,
}))
vi.mock('src/ts/chatBindings.svelte', () => ({
    captureChatBindingTarget: () => ({
        conversation: DBState.db.characters[0].chats[0],
        isCurrent: () => true,
    }),
    updateChatBinding: (conversation: object, patch: object) => Object.assign(conversation, patch),
    saveChatBinding: mocks.save,
}))
vi.mock('./TogglePresetPopup.svelte', () => ({
    default: (anchor: unknown, props: { close: () => void }) => mocks.popup(props),
}))

let instance: ReturnType<typeof mount>
const chat = () => DBState.db.characters[0].chats[0]
const button = (title: string) =>
    document.querySelector<HTMLButtonElement>(`button[title="${title}"]`)

beforeEach(async () => {
    DBState.db = {
        characters: [{ chatPage: 0, chats: [{ id: 'chat' }] }],
        globalChatVariables: { toggle_a: '1', toggle_b: '0' },
    } as unknown as typeof DBState.db
    instance = mount(ToggleBind, { target: document.body })
    await tick()
})

afterEach(async () => {
    await unmount(instance)
    document.body.replaceChildren()
    vi.clearAllMocks()
})

it('binds the current toggle values, counts later changes and saves them', async () => {
    const bind = button(languageEnglish.bindToggles)!
    expect(bind.textContent).toContain(languageEnglish.bindTogglesLabel)
    bind.click()
    await tick()
    await vi.waitFor(() => expect(mocks.toast).toHaveBeenCalledWith(languageEnglish.togglesBound))
    expect(chat().savedToggleValues).toEqual({ toggle_a: '1', toggle_b: '0' })
    expect(mocks.save).toHaveBeenCalledTimes(1)
    const save = button(languageEnglish.saveToggleChanges)!
    expect(save.disabled).toBe(true)
    expect(save.textContent).toContain(languageEnglish.saveTogglesLabel)
    DBState.db.globalChatVariables.toggle_b = '1'
    DBState.db.globalChatVariables.toggle_c = 'x'
    await tick()
    expect(save.disabled).toBe(false)
    expect(save.textContent?.trim()).toBe('2')
    save.click()
    await vi.waitFor(() => expect(mocks.save).toHaveBeenCalledTimes(2))
    expect(chat().savedToggleValues).toEqual({ toggle_a: '1', toggle_b: '1', toggle_c: 'x' })
    await tick()
    expect(button(languageEnglish.saveToggleChanges)!.disabled).toBe(true)
})

it('unbinds only after confirmation and keeps the binding when declined', async () => {
    chat().savedToggleValues = { toggle_a: '1' }
    await tick()
    mocks.confirm.mockResolvedValueOnce(false)
    button(languageEnglish.unbindToggles)!.click()
    await vi.waitFor(() => expect(mocks.confirm).toHaveBeenCalledWith(languageEnglish.unbindTogglesConfirm))
    await tick()
    expect(chat().savedToggleValues).toEqual({ toggle_a: '1' })
    expect(mocks.save).not.toHaveBeenCalled()
    button(languageEnglish.unbindToggles)!.click()
    await vi.waitFor(() => expect(mocks.toast).toHaveBeenCalledWith(languageEnglish.togglesUnbound))
    expect(chat().savedToggleValues).toBeUndefined()
    await tick()
    expect(button(languageEnglish.bindToggles)).not.toBeNull()
})

it('disables binding controls but keeps the preset popup reachable while binding is off', async () => {
    chat().savedToggleValues = { toggle_a: '0' }
    DBState.db.disableToggleBinding = true
    await tick()
    expect(button(languageEnglish.unbindToggles)!.disabled).toBe(true)
    const save = button(languageEnglish.saveToggleChanges)!
    expect(save.disabled).toBe(true)
    expect(save.textContent).toContain(languageEnglish.saveTogglesLabel)
    expect(document.body.textContent).toContain(languageEnglish.toggleBindingDisabled)
    button(languageEnglish.togglePresets)!.click()
    await tick()
    expect(mocks.popup).toHaveBeenCalledTimes(1)
    mocks.popup.mock.calls[0][0].close()
    await tick()
    button(languageEnglish.togglePresets)!.click()
    await tick()
    expect(mocks.popup).toHaveBeenCalledTimes(2)
})

it('mentions chat local overrides only when the chat pins toggle variables locally', async () => {
    expect(document.body.textContent).not.toContain(languageEnglish.localTogglePriority)
    chat().GLGlobalVariables = { toggle_a: '1' }
    await tick()
    expect(document.body.textContent).toContain(languageEnglish.localTogglePriority)
})
