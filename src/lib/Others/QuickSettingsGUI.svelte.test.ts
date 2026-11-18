import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { Database, botPreset } from 'src/ts/storage/database.svelte'
import { DBState, sideBarStore } from 'src/ts/stores.svelte'
import { observeEffectivePresetDerive, presetMirrorMap, setEffectivePresetBinding } from 'src/ts/storage/effectiveIdentityState'
import { flushPresetEditorEdits, presetEditorDBState, syncPresetEditorTarget } from 'src/ts/storage/presetEditor.svelte'
import { textEditorPopup } from 'src/ts/gui/textEditorPopup.svelte'
import QuickSettingsGUI from './QuickSettingsGUI.svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} as Database })
    const QuickSettings = $state({ open: true, index: 0 })
    return {
        DBState, QuickSettings, settingsOpen: writable(false), MobileGUI: writable(false), MobileGUIStack: writable(0),
        MobileSideBar: writable(0), selectedCharID: writable(0), disableHighlight: writable(false), sideBarStore: writable(true),
    } satisfies Partial<typeof import('src/ts/stores.svelte')>
})
// The bot page stands in as one of its text fields, which can open the popup editor.
vi.mock('../Setting/Pages/BotSettings.svelte', async () => ({ default: (await import('../UI/GUI/TextAreaInput.svelte')).default }))
vi.mock('../Setting/Pages/OtherBotSettings.svelte', () => ({ default: () => {} }))
vi.mock('../Setting/Pages/Module/ModuleSettings.svelte', () => ({ default: () => {} }))
vi.mock('src/ts/gui/guisize', async () => {
    const { writable } = await import('svelte/store')
    return { textAreaSize: writable(0), textAreaTextSize: writable(1) }
})
vi.mock('src/ts/gui/highlight', () => ({
    highlighter: vi.fn(), getNewHighlightId: () => 1, removeHighlight: vi.fn(), AllCBS: [],
}))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/hotkey', () => ({ hotkeyMatches: () => false }))
vi.mock('src/ts/util', () => ({ sleep: async () => {} }))

function apply(db: Database, preset: botPreset): void {
    const fields = db as unknown as Record<string, unknown>
    const record = preset as unknown as Record<string, unknown>
    for (const [key, field] of Object.entries(presetMirrorMap)) {
        if (Object.hasOwn(record, field)) fields[key] = $state.snapshot(record[field])
    }
}

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    observeEffectivePresetDerive(undefined)
    sideBarStore.set(true)
    textEditorPopup.request = null
    document.body.replaceChildren()
})

it('shows and edits the selected preset while quick settings show in a bound chat', async () => {
    DBState.db = {
        botPresetsId: 0,
        botPresets: [
            { id: 'selected', name: 'Selected', mainPrompt: 'Prompt selected' },
            { id: 'bound', name: 'Bound', mainPrompt: 'Prompt bound' },
        ],
        characters: [],
        hotkeys: [],
    } as unknown as Database
    observeEffectivePresetDerive({
        beforeDerive: flushPresetEditorEdits,
        afterDerive: (db) => syncPresetEditorTarget(db, apply),
    })
    setEffectivePresetBinding(DBState.db, 'bound', apply)
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt bound')

    instance = mount(QuickSettingsGUI, { target: document.body })
    await tick()
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt selected')
    // Its text fields preview the preset they edit.
    document.querySelector<HTMLButtonElement>('button[aria-label="Edit in popup"]')!.click()
    expect(textEditorPopup.request).toMatchObject({ preview: true, presetEditor: true })

    presetEditorDBState.db.mainPrompt = 'Edited selected'
    // A closed sidebar hides quick settings without unmounting them.
    sideBarStore.set(false)
    await tick()
    expect(DBState.db.botPresets.map(preset => preset.mainPrompt)).toEqual(['Edited selected', 'Prompt bound'])
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt bound')
    sideBarStore.set(true)
    await tick()
    expect(presetEditorDBState.db.mainPrompt).toBe('Edited selected')

    presetEditorDBState.db.mainPrompt = 'Edited again'
    await unmount(instance)
    instance = undefined
    expect(DBState.db.botPresets.map(preset => preset.mainPrompt)).toEqual(['Edited again', 'Prompt bound'])
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt bound')
})
