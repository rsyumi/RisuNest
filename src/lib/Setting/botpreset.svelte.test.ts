import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { DBState } from 'src/ts/stores.svelte'
import { stubDragEvent, touch } from 'src/ts/gui/htmlTouchDrag.testSupport'
import BotPreset from './botpreset.svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, ShowRealmFrameStore: writable('') }
})
vi.mock('src/ts/alert', () => ({ alertCardExport: vi.fn(), alertConfirm: vi.fn(), alertError: vi.fn() }))
vi.mock('src/ts/process/templates/templates', () => ({ prebuiltPresets: { OAI2: {} } }))
vi.mock('../Others/PromptDiffModal.svelte', () => ({ default: () => {} }))
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
            const state = { root: { botPresetsId: db().botPresetsId } as any, presets: [...db().botPresets] }
            await mutate(state)
            db().botPresets = state.presets
            db().botPresetsId = state.root.botPresetsId
        },
    })
    return {
        movePreset: controller.movePreset, changeToPreset: vi.fn(), addPreset: vi.fn(), copyPreset: vi.fn(),
        downloadPreset: vi.fn(), importPreset: vi.fn(), readPresetBodies: vi.fn(), removePreset: vi.fn(), renamePreset: vi.fn(),
    }
})

let instance: ReturnType<typeof mount>
let hit: Element | null
const close = vi.fn()
const order = () => (DBState.db.botPresets as { name: string }[]).map(preset => preset.name)
function label(name: string) {
    return [...document.querySelectorAll('button[draggable="true"] span')].find(span => span.textContent === name)!
}

beforeEach(async () => {
    vi.useFakeTimers()
    stubDragEvent()
    vi.spyOn(HTMLElement.prototype, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
    vi.spyOn(document, 'elementFromPoint').mockImplementation(() => hit)
    DBState.db = { botPresets: ['Alpha', 'Beta', 'Gamma', 'Delta'].map(name => ({ name })), botPresetsId: 0 } as any
    instance = mount(BotPreset, { target: document.body, props: { close } })
    await tick()
})

afterEach(async () => {
    await unmount(instance)
    vi.runOnlyPendingTimers()
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
    vi.useRealTimers()
})

it('moves a held preset by touch to the gap it is dropped on without selecting it', async () => {
    const alpha = label('Alpha')
    const end = [...document.querySelectorAll('[role="listitem"]')].at(-1)!
    let entered = undefined as Event | undefined
    end.addEventListener('dragenter', event => { entered = event })
    touch(alpha, 'touchstart', 30, 60)
    vi.advanceTimersByTime(450)
    hit = end
    touch(alpha, 'touchmove', 30, 160)
    touch(alpha, 'touchend', 30, 160)
    alpha.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 }))
    await vi.waitFor(() => expect(order()).toEqual(['Beta', 'Gamma', 'Delta', 'Alpha']))
    // Drag implementations that follow the HTML specification drop only where dragenter was cancelled.
    expect(entered?.defaultPrevented).toBe(true)
    expect(close).not.toHaveBeenCalled()
})
