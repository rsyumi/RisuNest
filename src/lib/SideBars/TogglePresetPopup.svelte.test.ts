import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import TogglePresetPopup from './TogglePresetPopup.svelte'

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
vi.mock('src/lang', () => ({ language: languageEnglish }))
vi.mock('src/ts/stores.svelte', () => {
    const DBState = $state({ db: {} })
    return { DBState }
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
    document.querySelectorAll<HTMLButtonElement>(`button[title="${languageEnglish.togglePresetActions}"]`)[row].click()
    await tick()
}
const encode = (value: unknown) => ({ name: 'file.json', data: new TextEncoder().encode(JSON.stringify(value)) })

beforeEach(async () => {
    DBState.db = {
        botPresets: [{ name: 'Alpha' }, { name: 'Beta' }],
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
    await unmount(instance)
    document.body.replaceChildren()
    vi.clearAllMocks()
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

it('saves a new preset from the defined toggles only and tags it with the prompt preset', async () => {
    mocks.input.mockResolvedValueOnce('  Three ')
    byText(languageEnglish.saveNewTogglePreset).click()
    await vi.waitFor(() => expect(presets()).toHaveLength(3))
    expect(presets()[2]).toEqual({ name: 'Three', values: { toggle_a: '1', toggle_b: '0' }, promptPresetName: 'Alpha' })
    await tick()
    expect(names()).toHaveLength(2)
    mocks.input.mockResolvedValueOnce('')
    byText(languageEnglish.saveNewTogglePreset).click()
    await tick()
    await tick()
    expect(presets()).toHaveLength(3)
})

it('creates the preset list reactively when the database has none yet', async () => {
    DBState.db.togglePresets = undefined
    await tick()
    expect(document.body.textContent).toContain(languageEnglish.togglePresetsEmpty)
    mocks.input.mockResolvedValueOnce('First')
    byText(languageEnglish.saveNewTogglePreset).click()
    await vi.waitFor(() => expect(names()).toHaveLength(1))
    expect(names()[0].textContent).toContain('First')
})

it('renames, duplicates, overwrites, exports and deletes through the row actions', async () => {
    await openActions(0)
    mocks.input.mockResolvedValueOnce('Uno')
    byText(languageEnglish.renameTogglePreset).click()
    await vi.waitFor(() => expect(presets()[0].name).toBe('Uno'))
    expect(mocks.input).toHaveBeenCalledWith(languageEnglish.renameTogglePreset, [], 'One')
    byText(languageEnglish.duplicateTogglePreset).click()
    await tick()
    expect(presets().map((preset) => preset.name)).toEqual(['Uno', 'Uno (Copy)', 'Two'])
    expect(presets()[1].values).toEqual({ toggle_a: '0', toggle_b: '1' })
    await openActions(1)
    byText(languageEnglish.overwriteTogglePreset).click()
    await vi.waitFor(() => expect(presets()[1].values).toEqual({ toggle_a: '1', toggle_b: '0' }))
    expect(presets()[1].promptPresetName).toBe('Alpha')
    byText(languageEnglish.exportTogglePreset).click()
    expect(mocks.download).toHaveBeenCalledWith('Uno (Copy)_toggle.json', expect.stringContaining('"toggle_a": "1"'))
    byText(languageEnglish.deleteTogglePreset).click()
    await vi.waitFor(() => expect(presets().map((preset) => preset.name)).toEqual(['Uno', 'Two']))
    expect(mocks.confirm).toHaveBeenLastCalledWith(languageEnglish.deleteTogglePresetConfirm('Uno (Copy)'))
})

it('reorders presets only in the unfiltered list', async () => {
    await openActions(0)
    expect(document.querySelector(`button[title="${languageEnglish.moveTogglePresetDown}"]`)).toBeNull()
    switches()[0].click()
    await tick()
    const down = document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.moveTogglePresetDown}"]`)!
    down.click()
    await tick()
    expect(presets().map((preset) => preset.name)).toEqual(['Two', 'One'])
    expect(document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.moveTogglePresetDown}"]`)!.disabled).toBe(true)
    document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.moveTogglePresetUp}"]`)!.click()
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
    byText(languageEnglish.defaultTogglesSaved).click()
    await vi.waitFor(() => expect(DBState.db.defaultToggleValues?.toggle_a).toBe('0'))
    mocks.select.mockResolvedValueOnce('1')
    byText(languageEnglish.defaultTogglesSaved).click()
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
