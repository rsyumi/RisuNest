import type { Chat, Database } from './storage/database.svelte'

export type ToggleValues = Record<string, string>

export function snapshotToggleValues(variables: ToggleValues): ToggleValues {
    return Object.fromEntries(
        Object.entries(variables).filter(
            ([key, value]) => key.startsWith('toggle_') && typeof value === 'string',
        ),
    )
}

/** Only the listed toggle keys that currently hold a value, in list order. */
export function pickToggleValues(variables: ToggleValues, keys: readonly string[]): ToggleValues {
    const values: ToggleValues = {}
    for (const key of keys) {
        const value = variables[key]
        if (key.startsWith('toggle_') && typeof value === 'string') values[key] = value
    }
    return values
}

/** Keeps the `toggle_` string entries of an untrusted record; null when it is not a record. */
export function sanitizeToggleValues(value: unknown): ToggleValues | null {
    if (!value || typeof value !== 'object' || Array.isArray(value)) return null
    return snapshotToggleValues(
        Object.fromEntries(Object.entries(value).filter(([, item]) => typeof item === 'string')),
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

export function toggleValueChanged(current: string | undefined, saved: string | undefined): boolean {
    return (current ?? '') !== (saved ?? '')
}

export function countToggleChanges(variables: ToggleValues, saved: ToggleValues): number {
    const current = snapshotToggleValues(variables)
    return [...new Set([...Object.keys(current), ...Object.keys(saved)])].filter((key) =>
        toggleValueChanged(current[key], saved[key]),
    ).length
}

export function defaultChatToggleBinding(
    db: Pick<Database, 'disableToggleBinding' | 'defaultToggleValues'>,
): Pick<Chat, 'savedToggleValues'> {
    return !db.disableToggleBinding && db.defaultToggleValues !== undefined
        ? { savedToggleValues: { ...db.defaultToggleValues } }
        : {}
}
