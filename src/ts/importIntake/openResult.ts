import { get } from 'svelte/store'
import type { NativeFileOperationImportLocation } from '../storage/nativeFileJobManager'
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
} from '../stores.svelte'
import { openedImportResult } from './sheetStore'

const MODULE_SETTINGS_PAGE = 14
const LOREBOOK_TAB = 3

/** Goes to what an import added: the character, the module settings, the preset list or a character's lorebook. */
export function openImportResult(location: NativeFileOperationImportLocation): void {
    switch (location.kind) {
        case 'module':
        case 'module-from-charx':
        case 'module-from-lorebook':
            closeLists()
            SettingsMenuIndex.set(MODULE_SETTINGS_PAGE)
            settingsOpen.set(true)
            return
        case 'preset':
            openPersonaList.set(false)
            openPresetList.set(true)
            return
        case 'character':
        case 'character-lorebook':
            void openCharacter(location)
            return
    }
}

/** Closes the preset and persona lists, which show over every screen. */
function closeLists(): void {
    openPresetList.set(false)
    openPersonaList.set(false)
}

async function openCharacter(location: Extract<NativeFileOperationImportLocation, { chaId: string }>): Promise<void> {
    const index = (DBState.db.characters ?? []).findIndex(character => character?.chaId === location.chaId)
    if (index < 0) return
    const { changeChar } = await import('../characters')
    // A refused selection (a running generation, an archived character) leaves the screen as it was.
    if (!await changeChar(index)) return
    closeLists()
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
