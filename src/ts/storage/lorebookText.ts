import type { loreBook } from './database.svelte'

/**
 * Imports, plugins and scripts can store a lorebook entry without these, which
 * the editor, the prompt builder and card export read as strings.
 */
export function fillLorebookText(entries: readonly loreBook[] | null | undefined): void {
    if (!Array.isArray(entries)) return
    for (const entry of entries) {
        if (!entry || typeof entry !== 'object') continue
        entry.key ??= ''
        entry.secondkey ??= ''
        entry.comment ??= ''
        entry.content ??= ''
    }
}
