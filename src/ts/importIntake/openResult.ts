import { fromStore, get } from 'svelte/store'
import { showRealmInfoStore } from '../characterCards'
import type { NativeFileOperationImportLocation } from '../storage/nativeFileJobManager'
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
} from '../stores.svelte'
import { openedImportResult } from './sheetStore'

const MODULE_SETTINGS_PAGE = 14
const LOREBOOK_TAB = 3

/** Goes to what an import added: the character, the module settings, the preset list or a character's lorebook. */
export function openImportResult(location: NativeFileOperationImportLocation): void {
    if (!canOpenImportResult(location)) return
    switch (location.kind) {
        case 'module':
        case 'module-from-charx':
        case 'module-from-lorebook':
            closeOverlays()
            SettingsMenuIndex.set(MODULE_SETTINGS_PAGE)
            settingsOpen.set(true)
            return
        case 'preset':
            closeOverlays()
            openPresetList.set(true)
            return
        case 'character':
        case 'character-lorebook':
            void openCharacter(location)
            return
    }
}

/**
 * Whether what an import added can be opened now. The HypaV3 summaries, the Realm upload, EasyPanel,
 * loadouts and Iris work on the selected character until they close, so no other character opens over them.
 * Read while a dialog draws, it follows the stores it depends on.
 */
export function canOpenImportResult(location: NativeFileOperationImportLocation): boolean {
    if (!('chaId' in location) || location.chaId === DBState.db.characters?.[fromStore(selectedCharID).current]?.chaId) return true
    return ![fromStore(hypaV3ModalOpen).current, fromStore(ShowRealmFrameStore).current, easyPanelStore.open, loadoutModalStore.open, irisStore.open].some(Boolean)
}

/**
 * Closes the lists and menus that show over every screen. Screens that keep unsaved work only while
 * open, such as the HypaV3 summaries or the Realm upload, stay open.
 */
function closeOverlays(): void {
    for (const open of [openPresetList, openPersonaList, bookmarkListOpen, moduleScopeMenuOpen]) open.set(false)
    for (const panel of [popUpEditorStore, customSideBarConfigDialogStore]) panel.open = false
    showRealmInfoStore.set(null)
}

async function openCharacter(location: Extract<NativeFileOperationImportLocation, { chaId: string }>): Promise<void> {
    const index = (DBState.db.characters ?? []).findIndex(character => character?.chaId === location.chaId)
    if (index < 0) return
    const { changeChar } = await import('../characters')
    // A refused selection (a running generation, an archived character) leaves the screen as it was.
    if (!await changeChar(index)) return
    closeOverlays()
    settingsOpen.set(false)
    openedImportResult.set({ kind: location.kind })
    if (location.kind === 'character') {
        MobileSideBar.set(0)
        return
    }
    CharConfigSubMenu.set(LOREBOOK_TAB)
    if (get(MobileGUI)) {
        MobileSideBar.set(2)
        return
    }
    // The desktop sidebar shows the character panel only in its character mode, without quick settings over it.
    QuickSettings.open = false
    botMakerMode.set(true)
    sideBarStore.set(true)
}
