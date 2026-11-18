import { afterEach, expect, it, vi } from 'vitest'
import type { Database, botPreset } from './database.svelte'
import { DBState, settingsOpen } from '../stores.svelte'
import {
    deriveEffectivePresetMirrors, observeEffectivePresetDerive, presetMirrorMap, setEffectivePresetBinding,
} from './effectiveIdentityState'
import { flushPresetEditorEdits, holdPresetEditor, presetEditorDBState, syncPresetEditorTarget } from './presetEditor.svelte'

vi.mock('../stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} as Database })
    return {
        DBState, settingsOpen: writable(false), MobileGUI: writable(false), MobileGUIStack: writable(0),
        MobileSideBar: writable(0), selectedCharID: writable(0),
    } satisfies Partial<typeof import('../stores.svelte')>
})

// Presets the editor builds a view of; the applied preset is applied to the live database instead.
const viewed: string[] = []
function apply(db: Database, preset: botPreset): void {
    if (db !== DBState.db) viewed.push(preset.id!)
    const fields = db as unknown as Record<string, unknown>
    const record = preset as unknown as Record<string, unknown>
    for (const [key, field] of Object.entries(presetMirrorMap)) {
        if (Object.hasOwn(record, field)) fields[key] = $state.snapshot(record[field])
    }
}

function open(names: string[], bound: string): Database['botPresets'] {
    DBState.db = {
        botPresetsId: 0,
        botPresets: names.map(name => ({ id: name, name, mainPrompt: `Prompt ${name}` })),
        characters: [],
    } as unknown as Database
    observeEffectivePresetDerive({
        beforeDerive: flushPresetEditorEdits,
        afterDerive: (db) => syncPresetEditorTarget(db, apply),
    })
    setEffectivePresetBinding(DBState.db, bound, apply)
    return DBState.db.botPresets
}

afterEach(() => {
    settingsOpen.set(false)
    observeEffectivePresetDerive(undefined)
    viewed.length = 0
})

it('builds the selected preset view only while preset settings show and keeps an edit made just before closing', () => {
    const presets = open(['selected', 'bound'], 'bound')
    // Chat switches and saves in a bound chat leave the selected preset alone while settings are closed.
    deriveEffectivePresetMirrors(DBState.db, apply)
    flushPresetEditorEdits(DBState.db)
    expect(viewed).toEqual([])
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt bound')

    settingsOpen.set(true)
    expect(viewed).toEqual(['selected'])
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt selected')
    presetEditorDBState.db.mainPrompt = 'Edited selected'
    settingsOpen.set(false)
    expect(presets[0].mainPrompt).toBe('Edited selected')
    expect(presets[1].mainPrompt).toBe('Prompt bound')
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt bound')

    // A change received while settings are closed shows once they open again.
    presets[0].mainPrompt = 'Received'
    deriveEffectivePresetMirrors(DBState.db, apply)
    expect(viewed).toEqual(['selected'])
    const release = holdPresetEditor()
    expect(presetEditorDBState.db.mainPrompt).toBe('Received')
    release()
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt bound')
})

it('keeps edits across a selection change and a binding change while preset settings show', () => {
    const presets = open(['selected', 'other', 'bound'], 'bound')
    settingsOpen.set(true)
    presetEditorDBState.db.mainPrompt = 'Edited selected'
    DBState.db.botPresetsId = 1
    deriveEffectivePresetMirrors(DBState.db, apply)
    expect(presets[0].mainPrompt).toBe('Edited selected')
    expect(presetEditorDBState.db.mainPrompt).toBe('Prompt other')

    presetEditorDBState.db.mainPrompt = 'Edited other'
    // Without the binding the selection is applied and edited in place.
    setEffectivePresetBinding(DBState.db, null, apply)
    expect(presets[1].mainPrompt).toBe('Edited other')
    expect(DBState.db.mainPrompt).toBe('Edited other')
    expect(presetEditorDBState.db.mainPrompt).toBe('Edited other')
    expect(presets.map(preset => preset.mainPrompt)).toEqual(['Edited selected', 'Edited other', 'Prompt bound'])
})
