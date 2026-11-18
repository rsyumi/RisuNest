import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, tick, unmount } from 'svelte'
import { language } from 'src/lang'
import type { Database, botPreset } from 'src/ts/storage/database.svelte'
import { DBState, MobileGUI, MobileGUIStack, selectedCharID, settingsOpen } from 'src/ts/stores.svelte'
import {
    observeEffectivePresetDerive, presetMirrorMap, setEffectivePresetBinding, deriveEffectivePresetMirrors,
} from 'src/ts/storage/effectiveIdentityState'
import { flushPresetEditorEdits, presetEditorDBState, syncPresetEditorTarget } from 'src/ts/storage/presetEditor.svelte'
import SeparateParametersSection from './SeparateParametersSection.svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} as Database })
    return {
        DBState, settingsOpen: writable(true), MobileGUI: writable(false), MobileGUIStack: writable(0),
        MobileSideBar: writable(0), selectedCharID: writable(-1),
    } satisfies Partial<typeof import('src/ts/stores.svelte')>
})
vi.mock('src/ts/storage/database.svelte', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    return { getDatabase: () => DBState.db } satisfies Partial<typeof import('src/ts/storage/database.svelte')>
})
vi.mock('src/ts/plugins/plugins.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { customProviderStore: writable([]), pluginV2: { providerOptions: new Map() } } as Partial<typeof import('src/ts/plugins/plugins.svelte')>
})
vi.mock('src/ts/plugins/apiV3/v3.svelte', () => ({ customV3ProviderMetaStore: [] }) satisfies Partial<typeof import('src/ts/plugins/apiV3/v3.svelte')>)
vi.mock('src/ts/globalApi.svelte', () => ({}))
vi.mock('src/ts/util', () => ({}))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    observeEffectivePresetDerive(undefined)
    settingsOpen.set(true)
    MobileGUI.set(false)
    MobileGUIStack.set(0)
    selectedCharID.set(-1)
    document.body.replaceChildren()
})

function apply(db: Database, preset: botPreset): void {
    const fields = db as unknown as Record<string, unknown>
    const record = preset as unknown as Record<string, unknown>
    for (const [key, field] of Object.entries(presetMirrorMap)) {
        if (Object.hasOwn(record, field)) fields[key] = $state.snapshot(record[field])
    }
}

const headers = () => [...document.querySelectorAll('button')]
    .filter(button => button.querySelector(':scope > span'))

it('edits only the auxiliary parameter sets and leaves per-model parameters untouched', async () => {
    const overrides = { 'synthetic-model': { temperature: 70 } }
    DBState.db = {
        seperateParametersEnabled: true,
        seperateParameters: { memory: {}, emotion: {}, translate: {}, otherAx: {}, overrides: structuredClone(overrides) },
        seperateModelsForAxModels: false,
        subModel: 'synthetic-model',
    } as any
    instance = mount(SeparateParametersSection, { target: document.body })
    await tick()
    headers()[0].click()
    await tick()
    const sets = headers().slice(1)
    for (const button of sets) {
        button.click()
        await tick()
        // Turns on the set's first parameter, the temperature.
        button.nextElementSibling!.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click()
        await tick()
    }
    for (const set of ['memory', 'emotion', 'translate', 'otherAx']) {
        expect(DBState.db.seperateParameters[set].temperature).toBe(0)
    }
    expect(DBState.db.seperateParameters.overrides).toEqual(overrides)
    expect(sets.map(button => button.textContent.trim())).toEqual([
        language.longTermMemory, language.emotionImage, language.translator, language.others,
    ])
})

it('shows and edits the selected preset in the settings window while a bound preset is applied', async () => {
    const sets = () => ({ memory: {}, emotion: {}, translate: {}, otherAx: {}, overrides: {} })
    DBState.db = {
        botPresetsId: 0,
        botPresets: [
            {
                id: 'selected', name: 'Selected', subModel: 'synthetic-model', seperateParametersEnabled: true,
                seperateParameters: { ...sets(), emotion: { thinking_type: 'adaptive', adaptive_thinking_effort: 'xhigh' } },
                seperateModelsForAxModels: true, seperateModels: { memory: '', emotion: 'claude-opus-5', translate: '', otherAx: '' },
            },
            {
                id: 'bound', name: 'Bound', subModel: 'synthetic-model', seperateParametersEnabled: false, seperateParameters: sets(),
                enableCustomFlags: true, customFlags: [],
            },
        ],
        characters: [],
    } as unknown as Database
    observeEffectivePresetDerive({
        beforeDerive: flushPresetEditorEdits,
        afterDerive: (db) => syncPresetEditorTarget(db, apply),
    })
    setEffectivePresetBinding(DBState.db, 'bound', apply)
    expect(DBState.db.seperateParametersEnabled).toBe(false)

    instance = mount(SeparateParametersSection, { target: document.body })
    await tick()
    headers()[0].click()
    await tick()
    // The selected preset turns the auxiliary sets on; the applied one leaves them off.
    expect(headers()).toHaveLength(5)
    headers()[1].click()
    await tick()
    headers()[1].nextElementSibling!.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click()
    flushSync()
    expect(DBState.db.botPresets[0].seperateParameters.memory.temperature).toBe(0)
    expect(DBState.db.botPresets[1].seperateParameters).toEqual(sets())
    expect(DBState.db.seperateParameters).toEqual(sets())
    expect($state.snapshot(presetEditorDBState.db.seperateParameters).memory).toEqual({ temperature: 0 })
    // The model's options follow the selected preset's flags, not the applied preset's custom flags.
    headers()[2].click()
    await tick()
    flushSync()
    const efforts = [...headers()[2].nextElementSibling!.querySelectorAll('option')].map(option => option.value)
    expect(efforts).toContain('xhigh')
    expect(DBState.db.botPresets[0].seperateParameters.emotion.adaptive_thinking_effort).toBe('xhigh')
    expect($state.snapshot(presetEditorDBState.db).seperateParametersEnabled).toBe(true)

    document.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click()
    flushSync()
    expect(DBState.db.botPresets[0].seperateParametersEnabled).toBe(false)
    expect(DBState.db.botPresets[1].seperateParametersEnabled).toBe(false)
    document.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click()
    flushSync()
    await tick()
    expect(headers()).toHaveLength(5)

    // Selecting the applied preset makes the page edit the mirrors again.
    DBState.db.botPresetsId = 1
    deriveEffectivePresetMirrors(DBState.db, apply)
    await tick()
    expect(headers()).toHaveLength(1)
    expect(DBState.db.botPresets[0].seperateParametersEnabled).toBe(true)
})

it('keeps the values the settings window showed when a protection flag changes there', () => {
    const sets = (temperature: number) => ({ memory: { temperature }, emotion: {}, translate: {}, otherAx: {}, overrides: {} })
    DBState.db = {
        botPresetsId: 0,
        botPresets: [
            { id: 'selected', name: 'Selected', seperateParameters: sets(1) },
            { id: 'bound', name: 'Bound', seperateParameters: sets(2) },
        ],
        characters: [],
    } as unknown as Database
    observeEffectivePresetDerive({
        beforeDerive: flushPresetEditorEdits,
        afterDerive: (db) => syncPresetEditorTarget(db, apply),
    })
    setEffectivePresetBinding(DBState.db, 'bound', apply)
    const editor = presetEditorDBState.db
    expect(editor.seperateParameters.memory).toEqual({ temperature: 1 })

    // Turning protection on shares the edited preset's values, also with the applied preset.
    editor.disableSeperateParameterChangeOnPresetChange = true
    expect(DBState.db.seperateParameters.memory).toEqual({ temperature: 1 })
    editor.seperateParameters.memory.temperature = 3
    expect(DBState.db.seperateParameters.memory).toEqual({ temperature: 3 })
    expect(DBState.db.botPresets.map(preset => preset.seperateParameters.memory)).toEqual([{ temperature: 1 }, { temperature: 2 }])

    // Turning it off gives the shared values to the edited preset; the applied one shows its own again.
    editor.disableSeperateParameterChangeOnPresetChange = false
    expect(DBState.db.botPresets[0].seperateParameters.memory).toEqual({ temperature: 3 })
    expect(DBState.db.seperateParameters.memory).toEqual({ temperature: 2 })
    expect(editor.seperateParameters.memory).toEqual({ temperature: 3 })

    // Outside the settings window the same fields edit the applied preset.
    settingsOpen.set(false)
    expect(presetEditorDBState.db.seperateParameters.memory).toEqual({ temperature: 2 })
    // The mobile settings screen is the settings window too, until a character's chat replaces it.
    MobileGUI.set(true)
    MobileGUIStack.set(2)
    expect(presetEditorDBState.db.seperateParameters.memory).toEqual({ temperature: 3 })
    selectedCharID.set(0)
    expect(presetEditorDBState.db.seperateParameters.memory).toEqual({ temperature: 2 })
})
