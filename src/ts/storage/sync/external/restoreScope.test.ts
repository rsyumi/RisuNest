import { describe, expect, it } from 'vitest'
import { externalRestorableSections, externalRestoreAreas } from './restoreScope'

const entry = (
    includedSections: Array<'hypa' | 'local-plugins' | 'local-settings'>,
    sameDevice: boolean,
) => ({ includedSections, sameDevice })

/**
 * Invariant 31. The chosen scope is what a restore follows: an area outside it
 * is left alone, and an area the selected backup does not carry cannot be
 * chosen at all.
 */
describe('external restore scope', () => {
    it('offers only the sections the backup carries', () => {
        expect(externalRestorableSections(entry([], true))).toEqual([])
        expect(externalRestorableSections(entry(['local-plugins'], true))).toEqual([
            'local-plugins',
        ])
        expect(
            externalRestorableSections(entry(['local-settings', 'hypa'], true)),
        ).toEqual(['hypa', 'local-settings'])
    })

    it('offers local settings only for a backup this device made', () => {
        expect(
            externalRestorableSections(entry(['hypa', 'local-settings'], false)),
        ).toEqual(['hypa'])
        expect(
            externalRestorableSections(entry(['hypa', 'local-settings'], true)),
        ).toEqual(['hypa', 'local-settings'])
    })

    it('asks for the library and the chosen sections only', () => {
        const carried = entry(['hypa', 'local-plugins', 'local-settings'], true)
        expect(externalRestoreAreas(carried, [])).toEqual(['library', 'referencedAssets'])
        expect(externalRestoreAreas(carried, ['local-plugins'])).toEqual([
            'library',
            'referencedAssets',
            'local-plugins',
        ])
        expect(
            externalRestoreAreas(carried, ['local-settings', 'hypa', 'local-plugins']),
        ).toEqual([
            'library',
            'referencedAssets',
            'hypa',
            'local-plugins',
            'local-settings',
        ])
    })

    it('drops a choice the backup does not cover or this device may not take', () => {
        expect(
            externalRestoreAreas(entry(['hypa'], true), ['hypa', 'local-plugins']),
        ).toEqual(['library', 'referencedAssets', 'hypa'])
        expect(
            externalRestoreAreas(entry(['hypa', 'local-settings'], false), [
                'hypa',
                'local-settings',
            ]),
        ).toEqual(['library', 'referencedAssets', 'hypa'])
    })
})
