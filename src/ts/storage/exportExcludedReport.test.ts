import { describe, expect, it } from 'vitest'
import { language } from 'src/lang'
import { formatExportExcludedReport, formatRisuSaveExportResult } from './exportExcludedReport'

describe('pinned export result notice', () => {
    it('combines exclusions and independent cleanup warnings', () => {
        const exclusions = { archivedCharacters: 2, collidingPluginValues: 1 }
        const message = formatRisuSaveExportResult({ exclusions, warningCodes: ['cleanup-failed'] })
        expect(message).toContain(language.risuSaveExportComplete)
        expect(message).toContain(language.risuSaveCleanupWarning)
        expect(message).toContain(language.risuNest.exportExcluded.archivedCharacters.replace('{0}', '2'))
        expect(message).toContain(language.risuNest.exportExcluded.collidingPluginValues.replace('{0}', '1'))
    })
    it('does not report nonexistent exclusions or invent an empty report', () => {
        expect(formatRisuSaveExportResult({ exclusions: { archivedCharacters: 0, collidingPluginValues: 0 }, warningCodes: [] }))
            .toBe(language.risuSaveExportComplete)
        expect(() => formatRisuSaveExportResult({ warningCodes: [] })).toThrow('exclusion report')
        expect(formatExportExcludedReport({ archivedCharacters: 0, collidingPluginValues: 1 }))
            .not.toContain(language.risuNest.exportExcluded.archivedHelp)
    })
})
