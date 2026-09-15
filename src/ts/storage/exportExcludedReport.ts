import { language } from 'src/lang'
import type { character, groupChat } from './database.svelte'
import { countArchivedCharacters } from './characterArchiveView'

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

export function collectExportExcludedReport(
    characters: readonly (character | groupChat)[],
): ExportExcludedReport {
    return {
        archivedCharacters: countArchivedCharacters(characters),
        collidingPluginValues: [],
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
