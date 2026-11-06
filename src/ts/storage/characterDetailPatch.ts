import type { CharacterDetail } from './persistentDataStore'
import { canonicalJson } from './saveCoordinatorHelpers'

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

/** The canonical JSON of one field of a character record; a missing field reads as `undefined`. */
export function characterDetailFieldJson(record: object, field: string): string {
    return canonicalJson({ value: (record as Record<string, unknown>)[field] })
}

/** The canonical JSON of every field of a character record except its chats, which a windowed selection holds only in part. */
export function captureCharacterDetailFields(record: object): ReadonlyMap<string, string> {
    return new Map(Object.keys(record)
        .filter((field) => field !== 'chats')
        .map((field) => [field, characterDetailFieldJson(record, field)]))
}

/** The fields of the patch whose value in the record is still what the capture holds, so a later edit of the others is kept. */
export function unchangedCharacterDetailPatch(
    patch: CharacterDetailPatch,
    record: object,
    captured: ReadonlyMap<string, string>,
): CharacterDetailPatch {
    return Object.fromEntries(Object.entries(patch).filter(([field]) =>
        characterDetailFieldJson(record, field) === (captured.get(field) ?? characterDetailFieldJson({}, field))))
}
