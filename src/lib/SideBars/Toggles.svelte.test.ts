import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import Toggles from './Toggles.svelte'

vi.mock('src/lang', () => ({ language: languageEnglish }))
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0) }
})
vi.mock('src/ts/storage/database.svelte', () => ({
    getCurrentChat: () => DBState.db.characters[0].chats[0],
}))
vi.mock('src/ts/process/modules', () => ({ getModuleToggles: () => '' }))
vi.mock('src/ts/util', () => ({
    parseToggleSyntax: () => [{ type: 'toggle', key: 'example', value: 'Example toggle' }],
}))
vi.mock('./ModelBind.svelte', () => ({ default: () => {} }))
vi.mock('./PersonaBind.svelte', () => ({ default: () => {} }))
vi.mock('./ToggleBind.svelte', () => ({ default: () => {} }))
vi.mock('./CustomSidebar.svelte', () => ({ default: () => {} }))
vi.mock('../UI/Accordion.svelte', () => ({ default: () => {} }))
vi.mock('../UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount>
const chat = () => DBState.db.characters[0].chats[0]
const customSwitch = () => document.querySelector<HTMLInputElement>('input[role="switch"]')!

beforeEach(async () => {
    DBState.db = {
        characters: [{ chatPage: 0, chats: [{ useLocallySetGlobalVariables: false }] }],
        globalChatVariables: { toggle_example: '0' },
        customPromptTemplateToggle: '',
        supaModelType: 'none',
    } as unknown as typeof DBState.db
    instance = mount(Toggles, {
        target: document.body,
        props: { chara: DBState.db.characters[0], noContainer: true },
    })
    await tick()
})

afterEach(async () => {
    await unmount(instance)
    document.body.replaceChildren()
})

it('switches a global toggle on and off using the existing string values', async () => {
    const input = customSwitch()
    expect(input.checked).toBe(false)
    input.click()
    await tick()
    expect(DBState.db.globalChatVariables.toggle_example).toBe('1')
    expect(input.checked).toBe(true)
    input.click()
    await tick()
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
    expect(input.checked).toBe(false)
})

it('keeps local overrides separate and restores the global value when unpinned', async () => {
    const localSwitch = document.querySelectorAll<HTMLInputElement>('input[role="switch"]')[1]
    localSwitch.click()
    await tick()
    expect(chat().useLocallySetGlobalVariables).toBe(true)
    customSwitch().click()
    await tick()
    expect(chat().GLGlobalVariables?.toggle_example).toBe('1')
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
    const pin = [...document.querySelectorAll('button')].find((button) => button.textContent?.includes('📌'))!
    pin.click()
    await tick()
    expect(chat().GLGlobalVariables?.toggle_example).toBeUndefined()
    expect(customSwitch().checked).toBe(false)
})

it('updates the switch when the active toggle value changes externally', async () => {
    DBState.db.globalChatVariables.toggle_example = '1'
    await tick()
    expect(customSwitch().checked).toBe(true)
    DBState.db.globalChatVariables.toggle_example = '0'
    await tick()
    expect(customSwitch().checked).toBe(false)
})

it('supports initially unset built-in switches and writes their bound values', async () => {
    DBState.db.jailbreak = 'Synthetic prompt'
    DBState.db.hypaV3 = true
    await tick()
    const switches = [...document.querySelectorAll<HTMLInputElement>('input[role="switch"]')]
    expect(switches).toHaveLength(4)
    expect(switches[0].checked).toBe(false)
    switches[0].click()
    switches[2].click()
    await tick()
    expect(DBState.db.jailbreakToggle).toBe(true)
    expect(DBState.db.characters[0].supaMemory).toBe(true)
})

it('tints toggles whose value differs from the chat binding unless binding is disabled', async () => {
    const row = () => customSwitch().closest('div.w-full')!
    expect(row().classList.contains('bg-draculared/15')).toBe(false)
    chat().savedToggleValues = { toggle_example: '1' }
    await tick()
    expect(row().classList.contains('bg-draculared/15')).toBe(true)
    DBState.db.globalChatVariables.toggle_example = '1'
    await tick()
    expect(row().classList.contains('bg-draculared/15')).toBe(false)
    DBState.db.globalChatVariables.toggle_example = '0'
    DBState.db.disableToggleBinding = true
    await tick()
    expect(row().classList.contains('bg-draculared/15')).toBe(false)
})
