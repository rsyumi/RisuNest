import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import TogglePresetPopup from './TogglePresetPopup.svelte'
import { observeEffectivePresetDerive, setEffectivePresetBinding } from 'src/ts/storage/effectiveIdentityState'
import { syncPresetEditorTarget } from 'src/ts/storage/presetEditor.svelte'

const mocks = vi.hoisted(() => ({
    confirm: vi.fn(async () => true),
    input: vi.fn(async () => ''),
    select: vi.fn(async () => '2'),
    toast: vi.fn(),
    error: vi.fn(),
    file: vi.fn(async (): Promise<{ name: string; data: Uint8Array } | null> => null),
    download: vi.fn(async () => true),
    close: vi.fn(),
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, alertStore: writable({ type: 'none', msg: '' }) }
})
vi.mock('src/ts/alert', () => ({
    alertConfirm: mocks.confirm,
    alertInput: mocks.input,
    alertSelect: mocks.select,
    alertToast: mocks.toast,
    alertError: mocks.error,
}))
vi.mock('src/ts/util', () => ({ selectSingleFile: mocks.file }))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: mocks.download }))
vi.mock('src/ts/toggleDefinitions', () => ({ currentToggleKeys: () => ['toggle_a', 'toggle_b'] }))

let instance: ReturnType<typeof mount>
const presets = () => DBState.db.togglePresets!
const names = () => [...document.querySelectorAll<HTMLButtonElement>(`button[title="${languageEnglish.apply}"]`)]
const byText = (text: string) =>
    [...document.querySelectorAll('button')].find((button) => button.textContent?.trim() === text)!
const switches = () => [...document.querySelectorAll<HTMLInputElement>('input[role="switch"]')]
const openActions = async (row: number) => {
    document.querySelectorAll<HTMLButtonElement>(`button[title="${languageEnglish.presetActions}"]`)[row].click()
    await tick()
}
const encode = (value: unknown) => ({ name: 'file.json', data: new TextEncoder().encode(JSON.stringify(value)) })

beforeEach(async () => {
    DBState.db = {
        botPresets: [{ id: 'alpha', name: 'Alpha' }, { id: 'beta', name: 'Beta' }],
        botPresetsId: 0,
        globalChatVariables: { toggle_a: '1', toggle_b: '0', toggle_orphan: 'x' },
        togglePresets: [
            { name: 'One', values: { toggle_a: '0', toggle_b: '1' }, promptPresetName: 'Alpha' },
            { name: 'Two', values: { toggle_a: '1' }, promptPresetName: 'Beta' },
        ],
    } as unknown as typeof DBState.db
    instance = mount(TogglePresetPopup, { target: document.body, props: { close: mocks.close } })
    await tick()
})

afterEach(async () => {
    observeEffectivePresetDerive(undefined)
    await unmount(instance)
    document.body.replaceChildren()
    vi.clearAllMocks()
})

it('deletes the confirmed preset after its position changes', async () => {
    let confirm!: (value: boolean) => void
    mocks.confirm.mockReturnValueOnce(new Promise(resolve => { confirm = resolve }))
    await openActions(0)
    byText(languageEnglish.deletePreset).click()
    presets().reverse()
    await tick()
    confirm(true)
    await vi.waitFor(() => expect(presets()).toHaveLength(1))
    expect(presets()[0].name).toBe('Two')
})

it('lists presets of the active prompt preset, shows all on request and applies after confirmation', async () => {
    expect(names().map((button) => button.textContent)).toEqual([expect.stringContaining('One')])
    expect(names()[0].textContent).toContain('Alpha')
    switches()[0].click()
    await tick()
    expect(names()).toHaveLength(2)
    mocks.confirm.mockResolvedValueOnce(false)
    names()[1].click()
    await vi.waitFor(() =>
        expect(mocks.confirm).toHaveBeenCalledWith(languageEnglish.applyTogglePresetMismatchConfirm),
    )
    await tick()
    expect(DBState.db.globalChatVariables).toEqual({ toggle_a: '1', toggle_b: '0', toggle_orphan: 'x' })
    expect(mocks.close).not.toHaveBeenCalled()
    names()[0].click()
    await vi.waitFor(() => expect(mocks.close).toHaveBeenCalledTimes(1))
    expect(mocks.confirm).toHaveBeenLastCalledWith(languageEnglish.applyTogglePresetConfirm)
    expect(DBState.db.globalChatVariables).toEqual({ toggle_a: '0', toggle_b: '1', toggle_orphan: 'x' })
    expect(mocks.toast).toHaveBeenCalledWith(languageEnglish.togglePresetApplied('One'))
})

it('lists presets of the prompt preset a conversation binding applies instead of the selection', async () => {
    observeEffectivePresetDerive({ beforeDerive: () => {}, afterDerive: (db) => syncPresetEditorTarget(db, () => {}) })
    setEffectivePresetBinding(DBState.db, 'beta', () => {})
    await tick()
    expect(names().map((button) => button.textContent)).toEqual([expect.stringContaining('Two')])
    mocks.input.mockResolvedValueOnce('Bound')
    byText(languageEnglish.saveNewPreset).click()
    await vi.waitFor(() => expect(presets().at(-1)).toMatchObject({ name: 'Bound', promptPresetName: 'Beta' }))
})

it('keeps a refused preset open without showing an applied toast', async () => {
    await unmount(instance)
    instance = mount(TogglePresetPopup, { target: document.body, props: {
        close: mocks.close, onApply: async () => false,
    } })
    await tick()
    names()[0].click()
    await vi.waitFor(() => expect(mocks.confirm).toHaveBeenCalled())
    await tick()
    expect(mocks.toast).not.toHaveBeenCalled()
    expect(mocks.close).not.toHaveBeenCalled()
    expect(DBState.db.globalChatVariables).toEqual({ toggle_a: '1', toggle_b: '0', toggle_orphan: 'x' })
})

it('uses staged values for a new preset without changing applied toggles', async () => {
    await unmount(instance)
    instance = mount(TogglePresetPopup, { target: document.body, props: {
        close: mocks.close, values: { toggle_a: 'edited', toggle_b: '1' },
    } })
    mocks.input.mockResolvedValueOnce('Draft preset')
    await tick()
    byText(languageEnglish.saveNewPreset).click()
    await vi.waitFor(() => expect(presets().at(-1)?.name).toBe('Draft preset'))
    expect(presets().at(-1)?.values).toEqual({ toggle_a: 'edited', toggle_b: '1' })
    expect(DBState.db.globalChatVariables).toEqual({ toggle_a: '1', toggle_b: '0', toggle_orphan: 'x' })
})

it('saves a new preset from the defined toggles only and tags it with the prompt preset', async () => {
    mocks.input.mockResolvedValueOnce('  Three ')
    byText(languageEnglish.saveNewPreset).click()
    await vi.waitFor(() => expect(presets()).toHaveLength(3))
    expect(presets()[2]).toEqual({ name: 'Three', values: { toggle_a: '1', toggle_b: '0' }, promptPresetName: 'Alpha' })
    await tick()
    expect(names()).toHaveLength(2)
    mocks.input.mockResolvedValueOnce('')
    byText(languageEnglish.saveNewPreset).click()
    await tick()
    await tick()
    expect(presets()).toHaveLength(3)
})

it('creates the preset list reactively when the database has none yet', async () => {
    DBState.db.togglePresets = undefined
    await tick()
    expect(document.body.textContent).toContain(languageEnglish.togglePresetsEmpty)
    mocks.input.mockResolvedValueOnce('First')
    byText(languageEnglish.saveNewPreset).click()
    await vi.waitFor(() => expect(names()).toHaveLength(1))
    expect(names()[0].textContent).toContain('First')
})

it('renames, duplicates, overwrites, exports and deletes through the row actions', async () => {
    await openActions(0)
    mocks.input.mockResolvedValueOnce('Uno')
    byText(languageEnglish.renamePreset).click()
    await vi.waitFor(() => expect(presets()[0].name).toBe('Uno'))
    expect(mocks.input).toHaveBeenCalledWith(languageEnglish.renamePreset, [], 'One')
    byText(languageEnglish.duplicatePreset).click()
    await tick()
    expect(presets().map((preset) => preset.name)).toEqual(['Uno', 'Uno (Copy)', 'Two'])
    expect(presets()[1].values).toEqual({ toggle_a: '0', toggle_b: '1' })
    await openActions(1)
    byText(languageEnglish.overwriteTogglePreset).click()
    await vi.waitFor(() => expect(presets()[1].values).toEqual({ toggle_a: '1', toggle_b: '0' }))
    expect(presets()[1].promptPresetName).toBe('Alpha')
    byText(languageEnglish.exportTogglePreset).click()
    expect(mocks.download).toHaveBeenCalledWith('Uno (Copy)_toggle.json', expect.stringContaining('"toggle_a": "1"'))
    byText(languageEnglish.deletePreset).click()
    await vi.waitFor(() => expect(presets().map((preset) => preset.name)).toEqual(['Uno', 'Two']))
    expect(mocks.confirm).toHaveBeenLastCalledWith(languageEnglish.deleteTogglePresetConfirm('Uno (Copy)'))
})

it('reorders presets only in the unfiltered list', async () => {
    await openActions(0)
    expect(document.querySelector(`button[title="${languageEnglish.movePresetDown}"]`)).toBeNull()
    switches()[0].click()
    await tick()
    const down = document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.movePresetDown}"]`)!
    down.click()
    await tick()
    expect(presets().map((preset) => preset.name)).toEqual(['Two', 'One'])
    expect(document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.movePresetDown}"]`)!.disabled).toBe(true)
    document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.movePresetUp}"]`)!.click()
    await tick()
    expect(presets().map((preset) => preset.name)).toEqual(['One', 'Two'])
})

it('imports only toggle values from a preset file and rejects malformed files', async () => {
    mocks.file.mockResolvedValueOnce(
        encode({ name: 'Imported', values: { toggle_a: '1', other: '1', toggle_n: 2 }, promptPresetName: 'Beta' }),
    )
    byText(languageEnglish.importTogglePreset).click()
    await vi.waitFor(() => expect(presets()).toHaveLength(3))
    expect(presets()[2]).toEqual({ name: 'Imported', values: { toggle_a: '1' }, promptPresetName: 'Beta' })
    mocks.file.mockResolvedValueOnce(encode({ name: 'Bad', values: ['toggle_a'] }))
    byText(languageEnglish.importTogglePreset).click()
    await vi.waitFor(() => expect(mocks.error).toHaveBeenCalledWith(languageEnglish.togglePresetImportError))
    mocks.file.mockResolvedValueOnce({ name: 'x.json', data: new TextEncoder().encode('{') })
    byText(languageEnglish.importTogglePreset).click()
    await vi.waitFor(() => expect(mocks.error).toHaveBeenCalledTimes(2))
    expect(presets()).toHaveLength(3)
})

it('saves, overwrites and clears the new chat default and toggles binding off', async () => {
    byText(languageEnglish.saveDefaultToggles).click()
    await vi.waitFor(() =>
        expect(DBState.db.defaultToggleValues).toEqual({ toggle_a: '1', toggle_b: '0', toggle_orphan: 'x' }),
    )
    expect(mocks.confirm).toHaveBeenCalledWith(languageEnglish.saveDefaultTogglesConfirm)
    await tick()
    DBState.db.globalChatVariables.toggle_a = '0'
    mocks.select.mockResolvedValueOnce('0')
    byText(languageEnglish.defaultTogglesManage).click()
    await vi.waitFor(() => expect(DBState.db.defaultToggleValues?.toggle_a).toBe('0'))
    mocks.select.mockResolvedValueOnce('1')
    byText(languageEnglish.defaultTogglesManage).click()
    await vi.waitFor(() => expect(DBState.db.defaultToggleValues).toBeUndefined())
    expect(mocks.toast).toHaveBeenLastCalledWith(languageEnglish.defaultTogglesCleared)
    await tick()
    expect(byText(languageEnglish.saveDefaultToggles)).toBeDefined()
    switches()[1].click()
    await tick()
    expect(DBState.db.disableToggleBinding).toBe(true)
})

it('ignores backdrop and Escape while a nested alert is open', async () => {
    let resolve!: (value: boolean) => void
    mocks.confirm.mockReturnValueOnce(new Promise<boolean>((done) => (resolve = done)))
    names()[0].click()
    await tick()
    const backdrop = document.querySelector<HTMLElement>(`button[aria-label="${languageEnglish.cancel}"]`)!
    backdrop.click()
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    expect(mocks.close).not.toHaveBeenCalled()
    resolve(false)
    await tick()
    await tick()
    backdrop.click()
    expect(mocks.close).toHaveBeenCalledTimes(1)
})

it.each(['success', 'cancel', 'failure'] as const)('reports a delayed preset export only after %s', async outcome => {
    let finish!: (value: boolean) => void
    let fail!: (error: Error) => void
    mocks.download.mockReturnValueOnce(new Promise((resolve, reject) => { finish = resolve; fail = reject }))
    await openActions(0)
    byText(languageEnglish.exportTogglePreset).click()
    await tick()
    expect(mocks.toast).not.toHaveBeenCalled()
    if (outcome === 'failure') fail(new Error('synthetic write'))
    else finish(outcome === 'success')
    await tick(); await Promise.resolve()
    if (outcome === 'success') expect(mocks.toast).toHaveBeenCalledWith(languageEnglish.togglePresetExported('One'))
    else expect(mocks.toast).not.toHaveBeenCalled()
    if (outcome === 'failure') expect(mocks.error).toHaveBeenCalledWith(languageEnglish.togglePresetExportError)
    else expect(mocks.error).not.toHaveBeenCalled()
})
