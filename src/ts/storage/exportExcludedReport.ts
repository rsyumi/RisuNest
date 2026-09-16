import { language } from 'src/lang'
import type { character, groupChat } from './database.svelte'
import { countArchivedCharacters } from './characterArchiveView'
import { isUnownedPluginOwner } from '../plugins/pluginOwner'
import { getPersistentDataStore } from './persistentDataStoreFactory'
import type { PluginStorageListItem } from './persistentDataStore'

export interface ExportExcludedPluginValue {
    key: string
    owners: string[]
}

export interface ExportExcludedReport {
    archivedCharacters: number
    collidingPluginValues: ExportExcludedPluginValue[]
}

export function isEmptyExportExcludedReport(report: ExportExcludedReport): boolean {
    return report.archivedCharacters === 0 && report.collidingPluginValues.length === 0
}

/**
 * An upstream save holds one value per key, so a key two plugins both hold
 * cannot go out without handing one plugin the other's value. The list carries
 * owners and keys only, never the values themselves.
 */
export function collidingPluginValues(
    items: readonly PluginStorageListItem[],
): ExportExcludedPluginValue[] {
    const owners = new Map<string, string[]>()
    for (const item of items) {
        if (item.space !== undefined) continue
        const holders = owners.get(item.key) ?? []
        holders.push(item.owner)
        owners.set(item.key, holders)
    }
    const colliding: ExportExcludedPluginValue[] = []
    for (const [key, holders] of owners) {
        if (holders.length < 2) continue
        colliding.push({
            key,
            owners: holders.map((owner) =>
                isUnownedPluginOwner(owner) ? language.risuNest.pluginData.ownerUnknown : owner,
            ),
        })
    }
    return colliding
}

export async function collectExportExcludedReport(
    characters: readonly (character | groupChat)[],
    listPluginStorage: () => Promise<PluginStorageListItem[]> = () =>
        getPersistentDataStore().listPluginStorage(),
): Promise<ExportExcludedReport> {
    return {
        archivedCharacters: countArchivedCharacters(characters),
        collidingPluginValues: collidingPluginValues(await listPluginStorage()),
    }
}

export function formatExportExcludedReport(report: ExportExcludedReport): string {
    const strings = language.risuNest.exportExcluded
    const lines = [strings.title, strings.body]
    if (report.archivedCharacters > 0) {
        lines.push(
            strings.archivedCharacters.replace('{0}', String(report.archivedCharacters)),
            strings.archivedHelp,
        )
    }
    if (report.collidingPluginValues.length > 0) {
        lines.push(
            strings.collidingPluginValues.replace(
                '{0}',
                String(report.collidingPluginValues.length),
            ),
            strings.collidingHelp,
            report.collidingPluginValues
                .map((value) => `${value.key} · ${value.owners.join(' · ')}`)
                .join('\n'),
        )
    }
    return lines.join('\n\n')
}
