import { describe, expect, it, vi } from 'vitest'

vi.mock('src/ts/characters', () => ({
    createBlankChar: () => ({
        globalLore: [],
        customscript: [],
        triggerscript: [],
    }),
}))

import { convertCharacterToModule, convertModuleToCharacter } from './interchangeability'
import type { RisuModule } from './process/modules'

describe('module and character interchangeability', () => {
    it('preserves module namespace and hide-icon behavior', () => {
        const source: RisuModule = {
            name: 'Round-trip module',
            description: 'Module metadata fixture',
            id: 'original-id',
            namespace: 'round-trip.namespace',
            hideIcon: true,
        }

        const character = convertModuleToCharacter(source)
        expect(character.moduleNamespace).toBe(source.namespace)
        expect(character.hideChatIcon).toBe(true)

        const imported = convertCharacterToModule(character)
        expect(imported.namespace).toBe(source.namespace)
        expect(imported.hideIcon).toBe(true)
    })
})
