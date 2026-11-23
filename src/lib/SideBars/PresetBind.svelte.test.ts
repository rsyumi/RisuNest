import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { flushSync, mount, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import type { PresetManagerProps } from '../Setting/PresetManager.svelte'
import PresetBind from './PresetBind.svelte'

const text = languageEnglish.risuNest
const mocks = vi.hoisted(() => ({
    blocked: vi.fn(() => false),
    bind: vi.fn(async (_target: unknown, _id: string | null) => true),
    list: vi.fn((_props: unknown) => {}),
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0) }
})
vi.mock('src/ts/chatBindings.svelte', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    return {
        captureChatBindingTarget: () => ({ conversation: DBState.db.characters[0].chats[0], isCurrent: () => true }),
        chatBindingBlockedByGeneration: mocks.blocked,
        bindPresetFromList: mocks.bind,
    }
})
vi.mock('src/ts/storage/presetEditor.svelte', () => ({ getAppliedPresetId: () => 'preset-a' }))
vi.mock('../Setting/PresetManager.svelte', () => ({
    default: (_anchor: unknown, props: PresetManagerProps) => mocks.list(props),
}))

let instance: ReturnType<typeof mount>
const chat = () => DBState.db.characters[0].chats[0]
const button = () => document.querySelector<HTMLButtonElement>(`button[title="${text.presetBinding.label}"]`)
const listProps = () => mocks.list.mock.lastCall![0] as PresetManagerProps

beforeEach(() => {
    mocks.blocked.mockReturnValue(false)
    mocks.list.mockClear()
    mocks.bind.mockClear()
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
    document.body.replaceChildren()
})

it('names the bound preset, a missing binding, or the selected preset it inherits', () => {
    expect(button()!.textContent).toContain(`${text.presetList.inherit} (Alpha)`)
    chat().promptPresetId = 'preset-b'
    flushSync()
    expect(button()!.textContent).toContain('Beta')
    expect(button()!.textContent).not.toContain('Alpha')
    chat().promptPresetId = 'removed'
    flushSync()
    expect(button()!.textContent).toContain(text.presetBinding.missing)
})

it('shows no binding control without a selected chat', () => {
    DBState.db.characters = []
    flushSync()
    expect(button()).toBeNull()
})

it('opens the preset list in bind mode for the selected chat unless a response is generating', async () => {
    mocks.blocked.mockReturnValue(true)
    button()!.click()
    flushSync()
    await vi.waitFor(() => expect(mocks.blocked).toHaveBeenCalled())
    expect(mocks.list).not.toHaveBeenCalled()

    mocks.blocked.mockReturnValue(false)
    chat().promptPresetId = 'preset-b'
    button()!.click()
    flushSync()
    // The layer that will hold the list shows that it is loading.
    expect(document.querySelector('.z-modal [role="status"]')?.textContent).toContain(languageEnglish.loading)
    await vi.waitFor(() => expect(mocks.list).toHaveBeenCalled())
    expect(listProps()).toMatchObject({ mode: 'bind', boundId: 'preset-b', chatPresetId: 'preset-a' })
    // The modal layer keeps the list above the editor popups that also show this row.
    expect(document.querySelector('.fixed.inset-0.z-modal')).not.toBeNull()

    await listProps().onBind!(null)
    expect(mocks.bind).toHaveBeenCalledWith(expect.objectContaining({ conversation: chat() }), null)
    listProps().close()
    flushSync()
    expect(document.querySelector('.fixed.inset-0.z-modal')).toBeNull()
})
