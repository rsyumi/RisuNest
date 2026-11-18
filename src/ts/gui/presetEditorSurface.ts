import { getContext, setContext } from 'svelte'
import { derived, get, type Readable } from 'svelte/store'
import { MobileGUI, MobileGUIStack, MobileSideBar, selectedCharID, settingsOpen } from '../stores.svelte'

const surfaceKey = Symbol('presetEditorSurface')
let settingsWindow: Readable<boolean> | undefined

/** The settings window, or the mobile screen that shows settings when no character is selected. */
export function settingsWindowStore(): Readable<boolean> {
    settingsWindow ??= derived(
        [settingsOpen, MobileGUI, MobileGUIStack, MobileSideBar, selectedCharID],
        ([open, mobile, screen, sideBar, character]) => open || (mobile && screen === 2 && sideBar === 0 && character === -1),
    )
    return settingsWindow
}

/** Marks the components below as a surface outside the settings window that edits the selected preset. */
export function markPresetEditorSurface(): void {
    setContext(surfaceKey, true)
}

/**
 * For a component's init. The returned function tells, when the component acts, whether it edits
 * the preset the settings window edits: inside a marked surface, or while the settings window shows.
 */
export function presetEditorOrigin(): () => boolean {
    const inSurface = getContext<boolean | undefined>(surfaceKey) === true
    return () => inSurface || get(settingsWindowStore())
}
