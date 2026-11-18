import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { flushSync, mount, unmount } from 'svelte'
import { get } from 'svelte/store'
import { languageEnglish } from '../../lang/en'
import { DBState, openPresetList } from 'src/ts/stores.svelte'
import { presetListBinding } from 'src/ts/chatBindings.svelte'
import PresetBind from './PresetBind.svelte'

const text = languageEnglish.risuNest
const mocks = vi.hoisted(() => ({ blocked: vi.fn(() => false) }))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0), openPresetList: writable(false) }
})
vi.mock('src/ts/chatBindings.svelte', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    const presetListBinding = $state<{ target: unknown }>({ target: null })
    return {
        presetListBinding,
        captureChatBindingTarget: () => ({ conversation: DBState.db.characters[0].chats[0], isCurrent: () => true }),
        chatBindingBlockedByGeneration: mocks.blocked,
    }
})

let instance: ReturnType<typeof mount>
const chat = () => DBState.db.characters[0].chats[0]
const button = () => document.querySelector<HTMLButtonElement>(`button[title="${text.presetBinding.label}"]`)!

beforeEach(() => {
    mocks.blocked.mockReturnValue(false)
    DBState.db = {
        botPresets: [{ id: 'preset-a', name: 'Alpha' }, { id: 'preset-b', name: 'Beta' }],
        botPresetsId: 0,
        characters: [{ chatPage: 0, chats: [{ id: 'chat' }] }],
    } as unknown as typeof DBState.db
    instance = mount(PresetBind, { target: document.body })
    flushSync()
})

afterEach(() => {
    unmount(instance)
    presetListBinding.target = null
    openPresetList.set(false)
    document.body.replaceChildren()
})

it('names the bound preset, a missing binding, or the selected preset it inherits', () => {
    expect(button().textContent).toContain(`${text.presetList.inherit} (Alpha)`)
    chat().promptPresetId = 'preset-b'
    flushSync()
    expect(button().textContent).toContain('Beta')
    expect(button().textContent).not.toContain('Alpha')
    chat().promptPresetId = 'removed'
    flushSync()
    expect(button().textContent).toContain(text.presetBinding.missing)
})

it('opens the preset list in bind mode for the selected conversation unless a response is generating', () => {
    mocks.blocked.mockReturnValue(true)
    button().click()
    expect(get(openPresetList)).toBe(false)
    expect(presetListBinding.target).toBeNull()

    mocks.blocked.mockReturnValue(false)
    button().click()
    expect(get(openPresetList)).toBe(true)
    expect((presetListBinding.target as { conversation: unknown }).conversation).toBe(chat())
})
