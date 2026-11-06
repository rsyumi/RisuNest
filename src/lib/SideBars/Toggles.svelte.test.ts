import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { DBState } from 'src/ts/stores.svelte'
import Toggles from './Toggles.svelte'
import { doingChat } from 'src/ts/process/generationState'
import { replaceConversationMetadata } from 'src/ts/storage/activeConversationSession'
import type { ToggleValues } from 'src/ts/toggleBindings'

const syntax = vi.hoisted(() => ({ type: 'toggle' }))
const toggleBind = vi.hoisted(() => ({
    props: {} as { unpinKeys?: string[], onApplyPreset?: (values: ToggleValues, keys: readonly string[]) => Promise<boolean> },
}))

vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0) }
})
vi.mock('src/ts/storage/database.svelte', () => ({
    getCurrentChat: () => DBState.db.characters[0].chats[DBState.db.characters[0].chatPage],
}))
vi.mock('src/ts/process/modules', () => ({ getModuleToggles: () => '' }))
// The persistent writes are covered by Chat.windowedInteractions.test.ts; here they apply in memory.
vi.mock('src/ts/chatBindings.svelte', async () => {
    const { get } = await import('svelte/store')
    const { doingChat } = await import('src/ts/process/generationState')
    const { applyConversationBindingPatch } = await import('src/ts/storage/conversationBinding')
    const { DBState } = await import('src/ts/stores.svelte')
    return {
        captureChatBindingTarget: () => ({ conversation: DBState.db.characters[0].chats[0], isCurrent: () => true }),
        conversationMutationBlockedByGeneration: () => get(doingChat),
        updateChatBinding: async (conversation: object, patch: object) => {
            applyConversationBindingPatch(conversation, patch)
        },
    }
})
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({ mutateCharacterDetailFields: async () => false }))
vi.mock('src/ts/util', () => ({
    parseToggleSyntax: () => [{ type: syntax.type, key: 'example', value: 'Example toggle', options: ['A', 'B'] }],
}))
vi.mock('./ModelBind.svelte', () => ({ default: () => {} }))
vi.mock('./PersonaBind.svelte', () => ({ default: () => {} }))
// The binding controls are covered by Chat.windowedInteractions.test.ts; here they expose what they receive.
vi.mock('./ToggleBind.svelte', () => ({ default: (_anchor: unknown, props: typeof toggleBind.props) => { toggleBind.props = props } }))
vi.mock('./CustomSidebar.svelte', () => ({ default: () => {} }))
vi.mock('../UI/Accordion.svelte', () => ({ default: () => {} }))
vi.mock('../UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount>
const chat = () => DBState.db.characters[0].chats[0]
const customSwitch = () => document.querySelector<HTMLInputElement>('input[role="switch"]')!

beforeEach(async () => {
    doingChat.set(false)
    syntax.type = 'toggle'
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
    doingChat.set(false)
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

it.each(['toggle', 'select', 'text'])('stages a bound %s input without changing applied or saved values', async type => {
    syntax.type = type
    DBState.db.customPromptTemplateToggle = 'refresh'
    chat().savedToggleValues = { toggle_example: '0' }
    await tick()
    if (type === 'toggle') customSwitch().click()
    else {
        const input = document.querySelector<HTMLInputElement | HTMLSelectElement>(type === 'select' ? 'select' : 'input[type="text"]')!
        input.value = '1'
        input.dispatchEvent(new Event('change', { bubbles: true }))
    }
    await tick()
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
    expect(chat().savedToggleValues).toEqual({ toggle_example: '0' })
    if (type === 'toggle') expect(customSwitch().checked).toBe(true)
    else expect(document.querySelector<HTMLInputElement | HTMLSelectElement>(type === 'select' ? 'select' : 'input[type="text"]')!.value).toBe('1')

    DBState.db.disableToggleBinding = true
    await tick()
    if (type === 'toggle') expect(customSwitch().checked).toBe(false)
    else expect(document.querySelector<HTMLInputElement | HTMLSelectElement>(type === 'select' ? 'select' : 'input[type="text"]')!.value).toBe('0')
})

it('discards unapplied input when leaving and returning to the bound conversation', async () => {
    chat().savedToggleValues = { toggle_example: '0' }
    DBState.db.characters[0].chats.push({ savedToggleValues: { toggle_example: '0' } } as any)
    await tick()
    customSwitch().click()
    await tick()
    expect(customSwitch().checked).toBe(true)
    DBState.db.characters[0].chatPage = 1
    await tick()
    expect(customSwitch().checked).toBe(false)
    DBState.db.characters[0].chatPage = 0
    await tick()
    expect(customSwitch().checked).toBe(false)
    expect(chat().savedToggleValues).toEqual({ toggle_example: '0' })
})

it('keeps unapplied input when the conversation metadata and global variables are replaced', async () => {
    chat().savedToggleValues = { toggle_example: '0' }
    await tick()
    customSwitch().click()
    await tick()
    expect(customSwitch().checked).toBe(true)
    // A metadata-changing session commit replaces every metadata object of the live chat.
    replaceConversationMetadata(chat(), { ...$state.snapshot(chat()), name: 'Renamed' })
    DBState.db.globalChatVariables = { ...DBState.db.globalChatVariables, synthetic_plugin_value: '1' }
    await tick()
    expect(customSwitch().checked).toBe(true)
    expect(chat().savedToggleValues).toEqual({ toggle_example: '0' })
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
})

it('follows the applied values once they hold the saved input', async () => {
    const row = () => customSwitch().closest('div.w-full')!
    chat().savedToggleValues = { toggle_example: '0' }
    chat().GLGlobalVariables = { toggle_example: '0' }
    await tick()
    customSwitch().click()
    await tick()
    expect(row().classList.contains('bg-draculared/15')).toBe(true)
    // Saving writes the binding, removes the replaced override and applies the bound values.
    chat().savedToggleValues = { toggle_example: '1' }
    chat().GLGlobalVariables = {}
    DBState.db.globalChatVariables.toggle_example = '1'
    await tick()
    expect(row().classList.contains('bg-draculared/15')).toBe(false)
    DBState.db.globalChatVariables.toggle_example = '0'
    await tick()
    expect(customSwitch().checked).toBe(false)
    chat().GLGlobalVariables = { toggle_example: '1' }
    await tick()
    expect(customSwitch().checked).toBe(true)
})

it('replaces local overrides with the values of an applied preset', async () => {
    const row = () => customSwitch().closest('div.w-full')!
    chat().savedToggleValues = { toggle_example: '0' }
    chat().GLGlobalVariables = { toggle_example: '1' }
    await tick()
    expect(customSwitch().checked).toBe(true)
    toggleBind.props.onApplyPreset!({ toggle_example: '0' }, ['toggle_example'])
    await tick()
    expect(customSwitch().checked).toBe(false)
    expect(row().classList.contains('bg-draculared/15')).toBe(true)
    expect(toggleBind.props.unpinKeys).toEqual(['toggle_example'])
    expect(chat().GLGlobalVariables).toEqual({ toggle_example: '1' })
    expect(chat().savedToggleValues).toEqual({ toggle_example: '0' })
})

it('replaces the local overrides a preset sets at once in an unbound chat', async () => {
    chat().GLGlobalVariables = { toggle_example: '1', toggle_other: '1' }
    await tick()
    expect(customSwitch().checked).toBe(true)
    toggleBind.props.onApplyPreset!({ toggle_example: '0' }, ['toggle_example'])
    await vi.waitFor(() => expect(chat().GLGlobalVariables).toEqual({ toggle_other: '1' }))
    await tick()
    expect(customSwitch().checked).toBe(false)
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
})

it('previews replacing a local override without removing it before the binding is saved', async () => {
    chat().savedToggleValues = { toggle_example: '0' }
    chat().GLGlobalVariables = { toggle_example: '1' }
    await tick()
    expect(customSwitch().checked).toBe(true)
    customSwitch().click()
    await tick()
    expect(customSwitch().checked).toBe(false)
    expect(chat().GLGlobalVariables).toEqual({ toggle_example: '1' })
    expect(chat().savedToggleValues).toEqual({ toggle_example: '0' })
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
})

it.each([false, true])('applies and clears presets in local overrides (bound: %s)', async (bound) => {
    chat().useLocallySetGlobalVariables = true
    chat().GLGlobalVariables = { toggle_example: '0', unrelated: 'keep' }
    if (bound) chat().savedToggleValues = { toggle_example: '0' }
    await tick()
    await expect(toggleBind.props.onApplyPreset!({ toggle_example: '1' }, ['toggle_example'])).resolves.toBe(true)
    await tick()
    expect(customSwitch().checked).toBe(true)
    expect(chat().GLGlobalVariables).toEqual({ toggle_example: '1', unrelated: 'keep' })
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
    await expect(toggleBind.props.onApplyPreset!({}, ['toggle_example'])).resolves.toBe(true)
    await tick()
    expect(chat().GLGlobalVariables).toEqual({ unrelated: 'keep' })
    expect(customSwitch().checked).toBe(false)
    expect(chat().savedToggleValues).toEqual(bound ? { toggle_example: '0' } : undefined)
})

it('reports a refused global preset without changing either value during generation', async () => {
    chat().GLGlobalVariables = { toggle_example: '1' }
    doingChat.set(true)
    await tick()
    await expect(toggleBind.props.onApplyPreset!({ toggle_example: '0' }, ['toggle_example'])).resolves.toBe(false)
    expect(chat().GLGlobalVariables.toggle_example).toBe('1')
    expect(DBState.db.globalChatVariables.toggle_example).toBe('0')
})

it('holds local toggle edits and unpinning while a response is generating', async () => {
    chat().useLocallySetGlobalVariables = true
    chat().GLGlobalVariables = { toggle_example: '1' }
    doingChat.set(true)
    await tick()
    const localSwitch = document.querySelectorAll<HTMLInputElement>('input[role="switch"]')[1]
    const pin = [...document.querySelectorAll('button')].find(button => button.textContent?.includes('📌'))!
    expect(customSwitch().disabled).toBe(true)
    expect(localSwitch.disabled).toBe(true)
    expect(pin.disabled).toBe(true)
    customSwitch().click()
    localSwitch.click()
    pin.click()
    expect(chat().useLocallySetGlobalVariables).toBe(true)
    expect(chat().GLGlobalVariables.toggle_example).toBe('1')
    doingChat.set(false)
    await tick()
    expect(customSwitch().disabled).toBe(false)
    expect(localSwitch.disabled).toBe(false)
    pin.click()
    await tick()
    expect(chat().GLGlobalVariables.toggle_example).toBeUndefined()
})

it('allows global toggle changes during generation unless they would delete a local override', async () => {
    doingChat.set(true)
    await tick()
    expect(customSwitch().disabled).toBe(false)
    customSwitch().click()
    await tick()
    expect(DBState.db.globalChatVariables.toggle_example).toBe('1')
    chat().GLGlobalVariables = { toggle_example: '' }
    await tick()
    expect(customSwitch().disabled).toBe(true)
})

it.each(['select', 'text', 'textarea'])('holds a local %s toggle until generation finishes', async type => {
    syntax.type = type
    DBState.db.customPromptTemplateToggle = 'refresh'
    chat().useLocallySetGlobalVariables = true
    doingChat.set(true)
    await tick()
    if (type === 'text') expect(document.querySelector<HTMLInputElement>('input[type="text"]')!.disabled).toBe(true)
    else expect(document.querySelector('[inert]')?.textContent).toContain('Example toggle')
    doingChat.set(false)
    await tick()
    expect(document.querySelector('[inert]')).toBeNull()
    if (type === 'text') expect(document.querySelector<HTMLInputElement>('input[type="text"]')!.disabled).toBe(false)
})
