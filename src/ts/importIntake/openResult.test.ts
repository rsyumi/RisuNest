import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get, toStore } from 'svelte/store'

const navigation = vi.hoisted(() => ({ changeChar: vi.fn<(index: number) => Promise<boolean>>() }))
vi.mock('src/ts/stores.svelte', async () => (await import('./openResult.testSupport')).importStoresModule())
vi.mock('../characters', () => ({ changeChar: navigation.changeChar }) satisfies Partial<typeof import('../characters')>)
vi.mock('../characterCards', async () => ({
    showRealmInfoStore: (await import('svelte/store')).writable(null),
}) satisfies Partial<typeof import('../characterCards')>)

import {
    bookmarkListOpen,
    botMakerMode,
    CharConfigSubMenu,
    customSideBarConfigDialogStore,
    DBState,
    easyPanelStore,
    hypaV3ModalOpen,
    irisStore,
    loadoutModalStore,
    MobileGUI,
    MobileSideBar,
    moduleScopeMenuOpen,
    openPersonaList,
    openPresetList,
    popUpEditorStore,
    QuickSettings,
    selectedCharID,
    settingsOpen,
    SettingsMenuIndex,
    ShowRealmFrameStore,
    sideBarStore,
} from 'src/ts/stores.svelte'
import { showRealmInfoStore, type hubType } from '../characterCards'
import { canOpenImportResult, openImportResult } from './openResult'
import { openedImportResult } from './sheetStore'

const overlayStores = [bookmarkListOpen, moduleScopeMenuOpen]
const overlayPanels = [popUpEditorStore, customSideBarConfigDialogStore]
// These work on the selected character and hold unsaved work only while open.
const workScreens = {
    'the HypaV3 summaries': (open: boolean) => hypaV3ModalOpen.set(open),
    'the Realm upload': (open: boolean) => ShowRealmFrameStore.set(open ? 'character' : ''),
    EasyPanel: (open: boolean) => { easyPanelStore.open = open },
    loadouts: (open: boolean) => { loadoutModalStore.open = open },
    Iris: (open: boolean) => { irisStore.open = open },
}
const showWorkScreens = (open: boolean) => Object.values(workScreens).forEach(show => show(open))

/** Lets the selection that `openImportResult` started finish. */
const settled = () => new Promise(resolve => setTimeout(resolve))

function screen() {
    return {
        settingsOpen: get(settingsOpen),
        settingsPage: get(SettingsMenuIndex),
        presetList: get(openPresetList),
        personaList: get(openPersonaList),
        overlays: [...overlayStores.map(open => get(open)), ...overlayPanels.map(panel => panel.open), get(showRealmInfoStore)].some(Boolean),
        workScreens: [get(hypaV3ModalOpen), get(ShowRealmFrameStore), easyPanelStore.open, loadoutModalStore.open, irisStore.open].every(Boolean),
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
    selectedCharID.set(0)
    settingsOpen.set(true)
    SettingsMenuIndex.set(2)
    // The lists and panels show over every screen until the Open button closes them.
    openPresetList.set(true)
    openPersonaList.set(true)
    for (const open of overlayStores) open.set(true)
    for (const panel of overlayPanels) panel.open = true
    showWorkScreens(true)
    showRealmInfoStore.set({ name: 'Synthetic' } as hubType)
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
        expect(canOpenImportResult({ kind })).toBe(true)
        openImportResult({ kind })
        expect(screen()).toMatchObject({ settingsOpen: true, settingsPage: 14, presetList: false, personaList: false, overlays: false, workScreens: true, opened: null })
        expect(navigation.changeChar).not.toHaveBeenCalled()
    })

    it('opens the preset list', () => {
        openImportResult({ kind: 'preset' })
        expect(screen()).toMatchObject({ presetList: true, personaList: false, overlays: false, workScreens: true, settingsOpen: true, settingsPage: 2, opened: null })
    })

    it.each([false, true])('selects an imported character and leaves settings for its chat, mobile %s', async mobile => {
        showWorkScreens(false)
        MobileGUI.set(mobile)
        openImportResult({ kind: 'character', chaId: 'imported' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(1))
        await settled()
        expect(screen()).toEqual({
            settingsOpen: false, settingsPage: 2, presetList: false, personaList: false, overlays: false, workScreens: false, opened: { kind: 'character' }, tab: 0,
            characterPanel: false, sidebar: false, quickSettings: true, mobilePanel: 0,
        })
    })

    it('opens the lorebook tab of the character panel in the desktop sidebar', async () => {
        showWorkScreens(false)
        openImportResult({ kind: 'character-lorebook', chaId: 'imported', characterName: 'Synthetic' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(1))
        await settled()
        expect(screen()).toEqual({
            settingsOpen: false, settingsPage: 2, presetList: false, personaList: false, overlays: false, workScreens: false, opened: { kind: 'character-lorebook' }, tab: 3,
            characterPanel: true, sidebar: true, quickSettings: false, mobilePanel: 1,
        })
    })

    it('opens the lorebook tab of the mobile character panel', async () => {
        showWorkScreens(false)
        MobileGUI.set(true)
        openImportResult({ kind: 'character-lorebook', chaId: 'imported', characterName: 'Synthetic' })
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(1))
        await settled()
        expect(screen()).toEqual({
            settingsOpen: false, settingsPage: 2, presetList: false, personaList: false, overlays: false, workScreens: false, opened: { kind: 'character-lorebook' }, tab: 3,
            characterPanel: false, sidebar: false, quickSettings: true, mobilePanel: 2,
        })
    })

    it('leaves the screen as it was when the selection is refused or the character is gone', async () => {
        showWorkScreens(false)
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

    it.each([
        ['the HypaV3 summaries closes', () => hypaV3ModalOpen.set(true), () => hypaV3ModalOpen.set(false)],
        ['the Realm upload closes', () => ShowRealmFrameStore.set('character'), () => ShowRealmFrameStore.set('')],
        ['the character is selected', () => hypaV3ModalOpen.set(true), () => selectedCharID.set(1)],
    ] as const)('offers the other character again once %s', async (_, cover, uncover) => {
        showWorkScreens(false)
        cover()
        // The outcome dialog reads this while it draws, so the offer must follow the screens it depends on.
        const offered: boolean[] = []
        const stop = toStore(() => canOpenImportResult({ kind: 'character', chaId: 'imported' })).subscribe(value => offered.push(value))
        try {
            uncover()
            await vi.waitFor(() => expect(offered).toEqual([false, true]))
        } finally {
            stop()
        }
    })

    it.each(Object.keys(workScreens) as (keyof typeof workScreens)[])('opens no other character over %s', async name => {
        showWorkScreens(false)
        workScreens[name](true)
        const before = screen()
        const other = { kind: 'character', chaId: 'imported' } as const
        expect(canOpenImportResult(other)).toBe(false)
        openImportResult(other)
        await settled()
        expect(navigation.changeChar).not.toHaveBeenCalled()
        expect(screen()).toEqual(before)

        // A lorebook added to the selected character still opens.
        const own = { kind: 'character-lorebook', chaId: 'first', characterName: 'Synthetic' } as const
        expect(canOpenImportResult(own)).toBe(true)
        openImportResult(own)
        await vi.waitFor(() => expect(navigation.changeChar).toHaveBeenCalledExactlyOnceWith(0))
    })
})
