import { language } from 'src/lang'

export interface ExportExclusions {
    archivedCharacters: number
    collidingPluginValues: number
}

export function formatExportExcludedReport(report: ExportExclusions): string {
    const strings = language.risuNest.exportExcluded
    const lines = [strings.title, strings.body]
    if (report.archivedCharacters > 0) {
        lines.push(strings.archivedCharacters.replace('{0}', String(report.archivedCharacters)), strings.archivedHelp)
    }
    if (report.collidingPluginValues > 0) {
        lines.push(strings.collidingPluginValues.replace('{0}', String(report.collidingPluginValues)), strings.collidingHelp)
    }
    return lines.join('\n\n')
}

export function formatRisuSaveExportResult(result: { warningCodes: string[]; exclusions?: ExportExclusions }): string {
    if (!result.exclusions || !Object.values(result.exclusions).every(value => Number.isSafeInteger(value) && value >= 0)) throw new Error('The export did not return its exclusion report')
    const { archivedCharacters, collidingPluginValues } = result.exclusions
    const messages = [language.risuSaveExportComplete]
    if (archivedCharacters > 0 || collidingPluginValues > 0) messages.push(formatExportExcludedReport(result.exclusions))
    if (result.warningCodes.includes('cleanup-failed')) messages.push(language.risuSaveCleanupWarning)
    return messages.join('\n\n')
}
