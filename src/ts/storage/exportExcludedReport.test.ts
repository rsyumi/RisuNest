import { describe, expect, it } from 'vitest'
import { language } from 'src/lang'
import {
    collectExportExcludedReport,
    collidingPluginValues,
    formatExportExcludedReport,
    isEmptyExportExcludedReport,
    type ExportExcludedReport,
} from './exportExcludedReport'
import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
import type { PluginStorageListItem } from './persistentDataStore'

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

    /** Invariant 14. */
    it('names only the keys more than one plugin holds, never their values', async () => {
        const listed: PluginStorageListItem[] = [
            { owner: 'alpha', key: 'api_key', valueType: 'string', byteSize: 10 },
            { owner: 'beta', key: 'api_key', valueType: 'string', byteSize: 12 },
            { owner: 'alpha', key: 'own', valueType: 'json', byteSize: 4 },
            { owner: UNOWNED_PLUGIN_OWNER, key: 'settings', valueType: 'json', byteSize: 4 },
            { owner: 'beta', key: 'settings', valueType: 'json', byteSize: 4 },
            { owner: 'alpha', key: 'device', space: 'string', valueType: 'string', byteSize: 4 },
            { owner: 'beta', key: 'device', space: 'string', valueType: 'string', byteSize: 4 },
        ]

        expect(collidingPluginValues(listed)).toEqual([
            { key: 'api_key', owners: ['alpha', 'beta'] },
            {
                key: 'settings',
                owners: [language.risuNest.pluginData.ownerUnknown, 'beta'],
            },
        ])

        const report = await collectExportExcludedReport([], async () => listed)
        expect(report.collidingPluginValues).toHaveLength(2)
        expect(report.archivedCharacters).toBe(0)
        expect(isEmptyExportExcludedReport(report)).toBe(false)
    })
})
