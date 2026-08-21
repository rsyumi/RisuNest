import { describe, expect, it } from 'vitest'
import moduleSettingsSource from './ModuleSettings.svelte?raw'

describe('module character conversion persistence', () => {
    it('awaits the named detached addition before reporting success', () => {
        expect(moduleSettingsSource).toContain(
            "await commitDetachedCharacter(char, 'convert-module-to-character')",
        )
        expect(moduleSettingsSource).not.toContain('DBState.db.characters.push(char)')
        expect(moduleSettingsSource.indexOf('await commitDetachedCharacter')).toBeLessThan(
            moduleSettingsSource.indexOf('alertNormal(language.successfullyConverted)'),
        )
    })
})
