import { describe, expect, it } from 'vitest'
import { language } from 'src/lang'
import {
    formatExportExcludedReport,
    isEmptyExportExcludedReport,
    type ExportExcludedReport,
} from './exportExcludedReport'

const empty: ExportExcludedReport = { archivedCharacters: 0, collidingPluginValues: [] }

describe('export excluded report', () => {
    it('reports nothing excluded when the library has no archive', () => {
        expect(isEmptyExportExcludedReport(empty)).toBe(true)
    })

    it('names the archived characters and how to include them', () => {
        const strings = language.risuNest.exportExcluded
        const report: ExportExcludedReport = {
            archivedCharacters: 4,
            collidingPluginValues: [],
        }

        expect(isEmptyExportExcludedReport(report)).toBe(false)
        const text = formatExportExcludedReport(report)
        expect(text).toContain(strings.title)
        expect(text).toContain(strings.body)
        expect(text).toContain(strings.archivedCharacters.replace('{0}', '4'))
        expect(text).toContain(strings.archivedHelp)
        expect(text).not.toContain(strings.collidingHelp)
    })

    it('holds a colliding plugin value section alongside the archived one', () => {
        const strings = language.risuNest.exportExcluded
        const text = formatExportExcludedReport({
            archivedCharacters: 1,
            collidingPluginValues: [{ key: 'api_key', owners: ['alpha', 'beta'] }],
        })

        expect(text).toContain(strings.archivedCharacters.replace('{0}', '1'))
        expect(text).toContain(strings.collidingPluginValues.replace('{0}', '1'))
        expect(text).toContain('api_key · alpha · beta')
    })
})
