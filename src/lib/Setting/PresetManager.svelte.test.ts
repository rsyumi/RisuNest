import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import { stubDragEvent, touch } from 'src/ts/gui/htmlTouchDrag.testSupport'
import PresetManager, { type PresetManagerProps } from './PresetManager.svelte'

const text = languageEnglish.risuNest.presetList
const mocks = vi.hoisted(() => ({
    confirm: vi.fn(async (_message: string) => true),
    input: vi.fn(async (): Promise<string | null> => null),
    cardExport: vi.fn(async () => ({ type: '', type2: '' })),
    download: vi.fn(async () => ({ data: {}, buf: null })),
    diff: vi.fn(),
    diffClose: vi.fn(),
    close: vi.fn(),
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, ShowRealmFrameStore: writable(''), alertStore: writable({ type: 'none', msg: '' }) }
})
vi.mock('src/ts/alert', () => ({
    alertCardExport: mocks.cardExport,
    alertConfirm: mocks.confirm,
    alertError: vi.fn(),
    alertInput: mocks.input,
    alertToast: vi.fn(),
}))
vi.mock('src/ts/process/templates/templates', () => ({ prebuiltPresets: { OAI2: {} } }))
vi.mock('../Others/PromptDiffModal.svelte', () => ({
    default: (_anchor: unknown, props: { firstPreset: { name: string }; secondPreset: { name: string }; onClose(): void }) => {
        mocks.diff([props.firstPreset.name, props.secondPreset.name])
        // Its close button saves the view settings, then closes.
        return { handleClose: () => { mocks.diffClose(); props.onClose() } }
    },
}))
vi.mock('src/ts/storage/database.svelte', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    const { createPresetWorkingSetController } = await import('src/ts/storage/presetWorkingSetOperations')
    const db = () => DBState.db as any
    // The persistent store boundary: commit the mutated working set back to the live database.
    const controller = createPresetWorkingSetController({
        getDatabase: db,
        captureCurrentPreset: () => null,
        applyPreset: () => {},
        mutatePersistentPresets: async (_reason, mutate) => {
            const state = { root: { botPresetsId: db().botPresetsId } as any, presets: $state.snapshot(db().botPresets) as any[] }
            await mutate(state)
            db().botPresets = state.presets
            db().botPresetsId = state.root.botPresetsId
        },
    })
    return {
        changeToPreset: controller.changeToPreset, addPreset: controller.addPreset, copyPreset: controller.copyPreset,
        movePreset: controller.movePreset, removePreset: controller.removePreset, renamePreset: controller.renamePreset,
        downloadPreset: mocks.download, importPreset: vi.fn(),
        // Bodies come from the store; the synthetic working set already holds them.
        readPresetBodies: async (ids: number[]) => ids.map(id => $state.snapshot(db().botPresets[id])),
    }
})

const fixture = ['Alpha', 'Beta', 'Gamma', 'Delta']
let instance: ReturnType<typeof mount> | undefined
let hit: Element | null
const order = () => (DBState.db.botPresets as { name: string }[]).map(preset => preset.name)
const rows = () => [...document.querySelectorAll<HTMLElement>('[role="listitem"]')]
const row = (name: string) => rows().find(item => item.textContent?.includes(name))!
const choose = (name: string) => row(name).querySelector('button')!
const byText = (text: string) => [...document.querySelectorAll('button')].find(button => button.textContent?.trim() === text)!
async function openActions(name: string) {
    row(name).querySelector<HTMLButtonElement>(`button[title="${languageEnglish.presetActions}"]`)!.click()
    await tick()
}
async function search(value: string) {
    const input = document.querySelector<HTMLInputElement>(`input[placeholder="${languageEnglish.search}"]`)!
    input.value = value
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await tick()
}
async function open(props: Partial<PresetManagerProps> = {}, names = fixture, extra: Record<string, unknown> = {}) {
    if (instance) await unmount(instance)
    DBState.db = {
        botPresets: names.map(name => ({ id: `id-${name}`, name })),
        botPresetsId: 0,
        ...extra,
    } as any
    instance = mount(PresetManager, { target: document.body, props: { close: mocks.close, ...props } })
    await tick()
}

beforeEach(async () => {
    vi.useFakeTimers()
    stubDragEvent()
    vi.spyOn(HTMLElement.prototype, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
    vi.spyOn(document, 'elementFromPoint').mockImplementation(() => hit)
    await open()
})

afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    vi.runOnlyPendingTimers()
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.clearAllMocks()
    vi.unstubAllGlobals()
    vi.useRealTimers()
})

it('numbers the first nine presets in hotkey order, selects a clicked preset and closes', async () => {
    const names = Array.from({ length: 10 }, (_, index) => `Preset ${index}`)
    await open({}, names)
    expect(rows().map(item => item.querySelector('button span')!.textContent)).toEqual(['1', '2', '3', '4', '5', '6', '7', '8', '9', ''])
    expect(choose('Preset 0').getAttribute('aria-current')).toBe('true')
    choose('Preset 2').click()
    await vi.waitFor(() => expect(mocks.close).toHaveBeenCalled())
    expect(DBState.db.botPresetsId).toBe(2)
})

it('filters by name keeping the original numbers, without reordering, and says when nothing matches', async () => {
    expect(document.querySelectorAll('[draggable="true"]').length).toBeGreaterThan(0)
    await search('  GAM ')
    expect(rows()).toHaveLength(1)
    expect(choose('Gamma').querySelector('span')!.textContent).toBe('3')
    expect(document.querySelectorAll('[draggable="true"]')).toHaveLength(0)
    await openActions('Gamma')
    expect(document.querySelector(`button[title="${languageEnglish.movePresetDown}"]`)).toBeNull()
    await search('zzz')
    expect(rows()).toHaveLength(0)
    expect(document.body.textContent).toContain(text.noMatch)
})

it('renames, duplicates, moves and deletes presets in the stored list', async () => {
    mocks.input.mockResolvedValueOnce('Bravo')
    await openActions('Beta')
    byText(languageEnglish.renamePreset).click()
    await vi.waitFor(() => expect(order()).toEqual(['Alpha', 'Bravo', 'Gamma', 'Delta']))

    await openActions('Alpha')
    byText(languageEnglish.duplicatePreset).click()
    await vi.waitFor(() => expect(order()).toEqual(['Alpha', 'Bravo', 'Gamma', 'Delta', 'Alpha Copy']))

    await openActions('Alpha')
    document.querySelector<HTMLButtonElement>(`button[title="${languageEnglish.movePresetDown}"]`)!.click()
    await vi.waitFor(() => expect(order()).toEqual(['Bravo', 'Alpha', 'Gamma', 'Delta', 'Alpha Copy']))

    mocks.confirm.mockResolvedValueOnce(false)
    await openActions('Gamma')
    byText(languageEnglish.deletePreset).click()
    await vi.waitFor(() => expect(mocks.confirm).toHaveBeenLastCalledWith(`${languageEnglish.removeConfirm}Gamma`))
    await tick()
    expect(order()).toContain('Gamma')
    byText(languageEnglish.deletePreset).click()
    await vi.waitFor(() => expect(order()).toEqual(['Bravo', 'Alpha', 'Delta', 'Alpha Copy']))
})

it('exports the preset whose menu was used', async () => {
    await openActions('Gamma')
    byText(text.export).click()
    await vi.waitFor(() => expect(mocks.download).toHaveBeenCalledWith(2, 'risupreset', expect.anything()))
})

it('compares two picked presets without changing the selection when prompt comparison is on', async () => {
    expect(byText(text.compare)).toBeUndefined()
    await open({}, fixture, { showPromptComparison: true })
    byText(text.compare).click()
    await tick()
    choose('Alpha').click()
    await tick()
    choose('Gamma').click()
    await vi.waitFor(() => expect(mocks.diff).toHaveBeenCalledWith(['Alpha', 'Gamma']))
    expect(DBState.db.botPresetsId).toBe(0)
    // Escape closes the comparison as its close button does, leaving the preset list open.
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    expect(mocks.diffClose).toHaveBeenCalled()
    expect(mocks.close).not.toHaveBeenCalled()
})

it('marks the preset the chat uses when the selection differs', async () => {
    await open({ chatPresetId: 'id-Gamma' })
    expect(row('Gamma').textContent).toContain(text.chatPreset)
    expect(row('Alpha').textContent).not.toContain(text.chatPreset)
    await open({ chatPresetId: 'id-Alpha' })
    expect(document.body.textContent).not.toContain(text.chatPreset)
})

it('binds in bind mode without changing the selection, and unbinds through the inherit row', async () => {
    const onBind = vi.fn(async (_id: string | null) => {})
    await open({ mode: 'bind', boundId: 'id-Gamma', onBind })
    // The Ctrl+number shortcuts change the selection, which a click here does not.
    expect(document.body.textContent).not.toContain(languageEnglish.quickPreset)
    expect(row('Alpha').textContent).not.toMatch(/\d/)
    const inherit = () => byText(`${text.inherit} (Alpha)`)
    expect(choose('Gamma').getAttribute('aria-current')).toBe('true')
    expect(choose('Alpha').getAttribute('aria-current')).toBeNull()
    expect(inherit().getAttribute('aria-current')).toBeNull()
    choose('Beta').click()
    await vi.waitFor(() => expect(mocks.close).toHaveBeenCalledTimes(1))
    expect(onBind).toHaveBeenLastCalledWith('id-Beta')
    expect(DBState.db.botPresetsId).toBe(0)

    onBind.mockResolvedValueOnce(false as never)
    inherit().click()
    await vi.waitFor(() => expect(onBind).toHaveBeenLastCalledWith(null))
    await tick()
    expect(mocks.close).toHaveBeenCalledTimes(1)
})

it('moves a held preset by touch to the gap it is dropped on without selecting it', async () => {
    await openActions('Gamma')
    const alpha = [...document.querySelectorAll('button[draggable="true"] span')].find(span => span.textContent === 'Alpha')!
    const end = rows().at(-1)!
    let entered = undefined as Event | undefined
    end.addEventListener('dragenter', event => { entered = event })
    touch(alpha, 'touchstart', 30, 60)
    vi.advanceTimersByTime(450)
    hit = end
    touch(alpha, 'touchmove', 30, 160)
    touch(alpha, 'touchend', 30, 160)
    alpha.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 }))
    await vi.waitFor(() => expect(order()).toEqual(['Beta', 'Gamma', 'Delta', 'Alpha']))
    // The menu that was open stays with its preset.
    await tick()
    const menu = (name: string) => row(name).querySelector(`button[title="${languageEnglish.movePresetDown}"]`)
    expect(menu('Gamma')).not.toBeNull()
    expect(menu('Delta')).toBeNull()
    // Drag implementations that follow the HTML specification drop only where dragenter was cancelled.
    expect(entered?.defaultPrevented).toBe(true)
    expect(mocks.close).not.toHaveBeenCalled()
})
