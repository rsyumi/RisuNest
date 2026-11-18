import { afterEach, expect, it, vi } from 'vitest'
import { flushSync, mount, unmount } from 'svelte'
import { get } from 'svelte/store'
import { languageEnglish } from '../../../lang/en'
import { DBState, openPresetList } from 'src/ts/stores.svelte'
import { observeEffectivePresetDerive, setEffectivePresetBinding } from 'src/ts/storage/effectiveIdentityState'
import { syncPresetEditorTarget } from 'src/ts/storage/presetEditor.svelte'
import PresetHeader from './PresetHeader.svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} })
    return { DBState, openPresetList: writable(false) }
})

let instance: ReturnType<typeof mount> | undefined

afterEach(() => {
    if (instance) unmount(instance)
    instance = undefined
    openPresetList.set(false)
    document.body.replaceChildren()
})

it('names the selected preset and opens the preset list', () => {
    DBState.db = {
        botPresets: [{ id: 'a', name: 'Alpha' }, { id: 'b', name: 'Beta', image: 'data:image/png;base64,AA==' }],
        botPresetsId: 0,
    } as unknown as typeof DBState.db
    instance = mount(PresetHeader, { target: document.body })
    flushSync()
    expect(document.body.textContent).toContain('Alpha')
    expect(document.querySelector('img')).toBeNull()

    DBState.db.botPresetsId = 1
    flushSync()
    expect(document.body.textContent).toContain('Beta')
    expect(document.body.textContent).not.toContain('Alpha')
    expect(document.querySelector('img')?.getAttribute('src')).toBe('data:image/png;base64,AA==')

    const change = [...document.querySelectorAll('button')]
        .find((button) => button.textContent?.includes(languageEnglish.risuNest.presetList.change))!
    change.click()
    expect(get(openPresetList)).toBe(true)
})

it('names the selection, not the preset a chat binding applies', () => {
    DBState.db = {
        botPresets: [{ id: 'a', name: 'Alpha' }, { id: 'b', name: 'Beta' }],
        botPresetsId: 0,
    } as unknown as typeof DBState.db
    observeEffectivePresetDerive({ beforeDerive: () => {}, afterDerive: (db) => syncPresetEditorTarget(db, () => {}) })
    setEffectivePresetBinding(DBState.db, 'b', () => {})
    instance = mount(PresetHeader, { target: document.body })
    flushSync()
    expect(document.body.textContent).toContain('Alpha')
    expect(document.body.textContent).not.toContain('Beta')
})
