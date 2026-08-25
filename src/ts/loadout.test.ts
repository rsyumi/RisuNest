import { describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    changeToPreset: vi.fn(),
    changeUserPersona: vi.fn(),
    database: {
        botPresets: [{ name: 'Target' }],
        enabledModules: [],
        globalChatVariables: {},
        personas: [],
        loadouts: [],
    } as Record<string, unknown>,
}))

vi.mock('./storage/database.svelte', () => ({
    changeToPreset: mocks.changeToPreset,
    getCurrentCharacter: () => undefined,
}))
vi.mock('./persona', () => ({ changeUserPersona: mocks.changeUserPersona }))
vi.mock('./stores.svelte', () => ({ DBState: { db: mocks.database } }))

import { applyLoadout, type Loadout } from './loadout'

describe('applyLoadout', () => {
    it('does not finish applying the loadout before preset activation completes', async () => {
        let resolve!: () => void
        mocks.changeToPreset.mockReturnValueOnce(new Promise<void>((done) => {
            resolve = done
        }))
        const loadout = {
            name: 'Fixture',
            id: 'loadout-a',
            lastUsed: 0,
            favorite: false,
            characterIds: [],
            modules: ['module-a'],
            globalVariables: { key: 'value' },
            presetName: 'Target',
            personaId: '',
        } satisfies Loadout

        const applying = applyLoadout(loadout, ['preset', 'modules', 'globalVariables'])

        expect(mocks.database.lastLoadedLoadoutName).toBeUndefined()
        resolve()
        await applying
        expect(mocks.database.lastLoadedLoadoutName).toBe('Fixture')
    })
})
