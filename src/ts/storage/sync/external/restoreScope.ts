/**
 * What a restore may bring back, and what it actually asks for. The selected
 * entry decides this, never the connection settings, so the same rules hold
 * wherever a restore is started from.
 */

import type {
    ExternalHistoryItem,
    ExternalRestoreArea,
    ExternalRestoreSection,
} from './types'

/** The library and its attachments are always replaced. */
const LIBRARY_AREAS: ExternalRestoreArea[] = ['library', 'referencedAssets']

const SECTION_ORDER: ExternalRestoreSection[] = ['hypa', 'local-plugins', 'local-settings']

/**
 * The areas of an entry this device is allowed to restore. Local settings hold
 * values that belong to the device that wrote them, so they are offered only
 * when this device wrote them.
 */
export function externalRestorableSections(
    item: Pick<ExternalHistoryItem, 'includedSections' | 'sameDevice'>,
): ExternalRestoreSection[] {
    const carried = new Set(item.includedSections)
    return SECTION_ORDER.filter(section => (
        carried.has(section) && (section !== 'local-settings' || item.sameDevice)
    ))
}

/**
 * The request a restore sends. The chosen sections are narrowed to what the
 * entry carries, so a stale choice cannot widen the scope.
 */
export function externalRestoreAreas(
    item: Pick<ExternalHistoryItem, 'includedSections' | 'sameDevice'>,
    chosen: readonly ExternalRestoreSection[],
): ExternalRestoreArea[] {
    const allowed = externalRestorableSections(item)
    return [...LIBRARY_AREAS, ...allowed.filter(section => chosen.includes(section))]
}
