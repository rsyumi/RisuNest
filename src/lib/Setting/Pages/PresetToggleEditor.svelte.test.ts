import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { DBState } from 'src/ts/stores.svelte'
import { language } from 'src/lang'
import PresetToggleEditor from './PresetToggleEditor.svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, selectedCharID: writable(0), alertStore: writable({ type: 'none', msg: '' }) }
})
vi.mock('src/ts/storage/database.svelte', () => ({}))
vi.mock('src/ts/characters', () => ({}))
vi.mock('src/ts/process/modules', () => ({ getModuleToggles: () => '' }))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    document.body.replaceChildren()
})

it('shows the preset custom toggles and stores an edit made in the editor', async () => {
    DBState.db = { customPromptTemplateToggle: 'mood=Mood' } as typeof DBState.db
    instance = mount(PresetToggleEditor, { target: document.body, props: { hideLabel: true } })
    await tick()
    const name = document.querySelector<HTMLInputElement>(`input[aria-label="${language.name}"]`)!
    expect(name.value).toBe('Mood')
    name.value = 'Weather'
    name.dispatchEvent(new Event('input', { bubbles: true }))
    await tick()
    expect(DBState.db.customPromptTemplateToggle).toBe('mood=Weather')
})
