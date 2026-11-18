import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

const navigation = vi.hoisted(() => ({ changeChar: vi.fn<(index: number) => Promise<boolean>>() }))
vi.mock('src/ts/stores.svelte', async () => (await import('./openResult.testSupport')).importStoresModule())
vi.mock('../characters', () => ({ changeChar: navigation.changeChar }) satisfies Partial<typeof import('../characters')>)

import {
    botMakerMode,
    CharConfigSubMenu,
    DBState,
    MobileGUI,
    MobileSideBar,
    openPersonaList,
    openPresetList,
    QuickSettings,
    settingsOpen,
    SettingsMenuIndex,
    sideBarStore,
} from 'src/ts/stores.svelte'
import { openImportResult } from './openResult'
import { openedImportResult } from './sheetStore'

/** Lets the selection that `openImportResult` started finish. */
const settled = () => new Promise(resolve => setTimeout(resolve))

function screen() {
    return {
        settingsOpen: get(settingsOpen),
        settingsPage: get(SettingsMenuIndex),
        presetList: get(openPresetList),
        personaList: get(openPersonaList),
        opened: get(openedImportResult),
        tab: get(CharConfigSubMenu),
        characterPanel: get(botMakerMode),
        sidebar: get(sideBarStore),
        quickSettings: QuickSettings.open,
        mobilePanel: get(MobileSideBar),
    }
}

beforeEach(() => {
    navigation.changeChar.mockReset().mockResolvedValue(true)
    DBState.db.characters = [{ chaId: 'first' }, { chaId: 'imported' }] as typeof DBState.db.characters
    settingsOpen.set(true)
    SettingsMenuIndex.set(2)
    // Both lists show over every screen until the Open button closes them.
    openPresetList.set(true)
    openPersonaList.set(true)
    openedImportResult.set(null)
    CharConfigSubMenu.set(0)
    botMakerMode.set(false)
    sideBarStore.set(false)
    QuickSettings.open = true
    MobileSideBar.set(1)
    MobileGUI.set(false)
})

describe('opening an import result', () => {
    it.each(['module', 'module-from-charx', 'module-from-lorebook'] as const)('opens the module settings page for %s', kind => {
        openImportResult({ kind })
        expect(screen()).toMatchObject({ settingsOpen: true, settingsPage: 14, presetList: false, personaList: false, opened: null })
        expect(navigation.changeChar).not.toHaveBeenCalled()
    })

    it('opens the preset list', () => {
        openImportResult({ kind: 'preset' })
        expect(screen()).toMatchObject({ presetList: true, personaList: false, settingsOpen: true, settingsPage: 2, opened: null })
    })

    it.each([false, true])('selects an imported character and leaves settings for its chat, mobile %s', async mobile => {
        MobileGUI.set(mobile)
        openImportResult({ kind: 'character', chaId: 'imported' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(1))
        await settled()
        expect(screen()).toEqual({
            settingsOpen: false, settingsPage: 2, presetList: false, personaList: false, opened: { kind: 'character' }, tab: 0,
            characterPanel: false, sidebar: false, quickSettings: true, mobilePanel: 0,
        })
    })

    it('opens the lorebook tab of the character panel in the desktop sidebar', async () => {
        openImportResult({ kind: 'character-lorebook', chaId: 'imported', characterName: 'Synthetic' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(1))
        await settled()
        expect(screen()).toEqual({
            settingsOpen: false, settingsPage: 2, presetList: false, personaList: false, opened: { kind: 'character-lorebook' }, tab: 3,
            characterPanel: true, sidebar: true, quickSettings: false, mobilePanel: 1,
        })
    })

    it('opens the lorebook tab of the mobile character panel', async () => {
        MobileGUI.set(true)
        openImportResult({ kind: 'character-lorebook', chaId: 'imported', characterName: 'Synthetic' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(1))
        await settled()
        expect(screen()).toEqual({
            settingsOpen: false, settingsPage: 2, presetList: false, personaList: false, opened: { kind: 'character-lorebook' }, tab: 3,
            characterPanel: false, sidebar: false, quickSettings: true, mobilePanel: 2,
        })
    })

    it('leaves the screen as it was when the selection is refused or the character is gone', async () => {
        const before = screen()
        navigation.changeChar.mockResolvedValue(false)
        openImportResult({ kind: 'character-lorebook', chaId: 'imported', characterName: 'Synthetic' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledOnce())
        await settled()
        expect(screen()).toEqual(before)

        openImportResult({ kind: 'character', chaId: 'removed' })
        await settled()
        expect(navigation.changeChar).toHaveBeenCalledOnce()
        expect(screen()).toEqual(before)
    })
})
