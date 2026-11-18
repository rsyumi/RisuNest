import { language } from '../../lang'
import { alertActionConfirm } from '../alert'
import { checkCharOrder, getFileSrc } from '../globalApi.svelte'
import type { folder } from '../storage/database.svelte'
import { DBState } from '../stores.svelte'
import { getCharacterIndexObject } from '../util'
import { applyFolderDraft, folderDraftFrom, releaseFolder, type FolderDraft } from './folderAppearance'

/** The folder the manager edits, where its tile is, and the unsaved draft the sidebar previews. */
export const folderManager = $state<{
    folderId: string | null
    anchor: { top: number, right: number } | null
    draft: FolderDraft | null
}>({ folderId: null, anchor: null, draft: null })

export function themeColors(): { bgcolor: string, darkbg: string } {
    const style = getComputedStyle(document.documentElement)
    return {
        bgcolor: style.getPropertyValue('--risu-theme-bgcolor').trim(),
        darkbg: style.getPropertyValue('--risu-theme-darkbg').trim(),
    }
}

export function findFolder(folderId: string): { index: number, folder: folder } | null {
    const index = DBState.db.characterOrder.findIndex((entry) => typeof entry !== 'string' && entry.id === folderId)
    return index === -1 ? null : { index, folder: DBState.db.characterOrder[index] as folder }
}

/** Characters of the folder that still exist. */
export function folderCharacterCount(stored: folder): number {
    const characters = getCharacterIndexObject()
    return stored.data.filter((id) => characters[id] !== undefined).length
}

/** Opens the manager for a folder; `anchor` is the top of its tile and the right edge of the sidebar. */
export function openFolderManager(folderId: string, anchor: { top: number, right: number }) {
    const found = findFolder(folderId)
    if (!found) return
    folderManager.folderId = folderId
    folderManager.anchor = anchor
    folderManager.draft = folderDraftFrom($state.snapshot(found.folder) as folder, themeColors())
}

export function closeFolderManager() {
    folderManager.folderId = null
    folderManager.anchor = null
    folderManager.draft = null
}

/**
 * Writes `opened`, the draft that was open when saving began, to its folder in one assignment and closes the manager.
 * Nothing is written once that draft's manager was closed, so a later one is never saved or closed by it.
 */
export async function saveFolderManager(opened = folderManager.draft) {
    if (!opened || folderManager.draft !== opened) return
    const folderId = folderManager.folderId
    const draft = $state.snapshot(opened) as FolderDraft
    const found = folderId ? findFolder(folderId) : null
    if (!folderId || !found) {
        closeFolderManager()
        return
    }
    const img = draft.imgFile && draft.imgFile !== found.folder.imgFile ? await getFileSrc(draft.imgFile) : undefined
    if (folderManager.draft !== opened) return
    // The folder may have moved or changed its characters while the image source resolved.
    const current = findFolder(folderId)
    if (current) {
        const next = applyFolderDraft($state.snapshot(current.folder) as folder, draft)
        if (img !== undefined) next.img = img
        DBState.db.characterOrder[current.index] = next
    }
    closeFolderManager()
}

/** After confirmation, replaces the folder with its characters at the same position and closes the manager. */
export async function deleteFolderFromManager() {
    const folderId = folderManager.folderId
    const found = folderId ? findFolder(folderId) : null
    if (!folderId || !found) return
    const text = language.risuNest.folderManager
    const confirmed = await alertActionConfirm({
        title: text.deleteTitle,
        description: text.deleteDescription.replace('{0}', String(folderCharacterCount(found.folder))),
        actionLabel: text.deleteAction,
        cancelLabel: language.cancel,
    })
    if (!confirmed || folderManager.folderId !== folderId) return
    DBState.db.characterOrder = releaseFolder($state.snapshot(DBState.db.characterOrder) as (string | folder)[], folderId)
    closeFolderManager()
    checkCharOrder()
}
