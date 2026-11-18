import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { Database } from 'src/ts/storage/database.svelte'
import { DBState, easyPanelStore, settingsOpen } from 'src/ts/stores.svelte'
import { holdPresetEditor } from 'src/ts/storage/presetEditor.svelte'
import EasyPanel from './EasyPanel.svelte'

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const DBState = $state({ db: {} as Database })
    const easyPanelStore = $state({ open: false })
    return {
        DBState, easyPanelStore, settingsOpen: writable(false), MobileGUI: writable(false), MobileGUIStack: writable(0),
        MobileSideBar: writable(0), selectedCharID: writable(0),
    } satisfies Partial<typeof import('src/ts/stores.svelte')>
})
vi.mock('src/lib/UI/ModelList.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/Setting/Pages/Advanced/CustomModelsSettings.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/Setting/Pages/ClaudeThinkingSeparateParams.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/Others/AllSeperateParameters.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))

let instance: ReturnType<typeof mount> | undefined
beforeEach(() => {
    DBState.db = { botPresetsId: 0, botPresets: [{ id: 'preset', name: 'Preset' }], characters: [] } as unknown as Database
    easyPanelStore.open = true
})
afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    settingsOpen.set(false)
    document.body.replaceChildren()
})

async function openPanel(): Promise<void> {
    instance = mount(EasyPanel, { target: document.body })
    await tick()
    expect(easyPanelStore.open).toBe(true)
}

it('closes when preset settings open under a panel opened from the chat screen', async () => {
    await openPanel()
    settingsOpen.set(true)
    await tick()
    expect(easyPanelStore.open).toBe(false)
})

it('closes when quick settings open under a panel opened from the chat screen', async () => {
    await openPanel()
    const release = holdPresetEditor()
    await tick()
    expect(easyPanelStore.open).toBe(false)
    release()
})

it('closes when the settings window it was opened from closes', async () => {
    settingsOpen.set(true)
    await openPanel()
    settingsOpen.set(false)
    await tick()
    expect(easyPanelStore.open).toBe(false)
})
