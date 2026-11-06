import type { CharacterDetail } from './persistentDataStore'

/** Fields of a character's own record that a detail mutation committed. */
export type CharacterDetailPatch = Partial<CharacterDetail>

/** Writes the patch onto a character record; `undefined` removes the field instead of storing it. */
export function applyCharacterDetailPatch<T extends object>(target: T, patch: CharacterDetailPatch): T {
    for (const [key, value] of Object.entries(patch)) {
        if (value === undefined) delete (target as Record<string, unknown>)[key]
        else (target as Record<string, unknown>)[key] = value
    }
    return target
}
