import type { Chat, Database } from './storage/database.svelte'

export type ToggleValues = Record<string, string>

export function snapshotToggleValues(variables: ToggleValues): ToggleValues {
    return Object.fromEntries(
        Object.entries(variables).filter(
            ([key, value]) => key.startsWith('toggle_') && typeof value === 'string',
        ),
    )
}

export function applyToggleValues(
    variables: ToggleValues,
    saved: ToggleValues,
    keys: readonly string[],
): void {
    for (const key of keys) {
        if (!key.startsWith('toggle_')) continue
        if (Object.hasOwn(saved, key)) variables[key] = saved[key]
        else delete variables[key]
    }
    Object.assign(variables, snapshotToggleValues(saved))
}

export function countToggleChanges(variables: ToggleValues, saved: ToggleValues): number {
    const current = snapshotToggleValues(variables)
    return [...new Set([...Object.keys(current), ...Object.keys(saved)])].filter(
        (key) => (current[key] ?? '') !== (saved[key] ?? ''),
    ).length
}

export function defaultChatToggleBinding(
    db: Pick<Database, 'disableToggleBinding' | 'defaultToggleValues'>,
): Pick<Chat, 'savedToggleValues'> {
    return !db.disableToggleBinding && db.defaultToggleValues !== undefined
        ? { savedToggleValues: { ...db.defaultToggleValues } }
        : {}
}
