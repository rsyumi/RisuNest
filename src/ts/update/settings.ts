export interface AppUpdateSettings {
    schema: 'risunest.app-update-settings/v1'
    autoUpdateCheck: boolean
    skippedVersion: string
    lastCheckedAt: number
}

export const appUpdateSettingsKey = 'risuNestUpdateSettings'

const defaults: AppUpdateSettings = {
    schema: 'risunest.app-update-settings/v1',
    autoUpdateCheck: true,
    skippedVersion: '',
    lastCheckedAt: 0,
}

let current: AppUpdateSettings | undefined
let readError: Error | null = null
const listeners = new Set<(settings: AppUpdateSettings) => void>()

export function validateAppUpdateSettings(value: unknown): AppUpdateSettings {
    if (!value || typeof value !== 'object') throw new Error('Update settings must be an object')
    const record = value as Record<string, unknown>
    if (
        Object.keys(record).length !== 4
        || record.schema !== defaults.schema
        || typeof record.autoUpdateCheck !== 'boolean'
        || typeof record.skippedVersion !== 'string'
        || typeof record.lastCheckedAt !== 'number'
        || !Number.isSafeInteger(record.lastCheckedAt)
        || record.lastCheckedAt < 0
    ) throw new Error('Update settings schema or values are invalid')
    return { ...(record as unknown as AppUpdateSettings) }
}

export function getAppUpdateSettings(): AppUpdateSettings {
    if (current) return { ...current }
    const raw = localStorage.getItem(appUpdateSettingsKey)
    if (raw === null) {
        current = { ...defaults }
        return { ...current }
    }
    try {
        current = validateAppUpdateSettings(JSON.parse(raw))
        readError = null
        return { ...current }
    } catch (error) {
        readError = error instanceof Error ? error : new Error(String(error))
        throw readError
    }
}

export function getAppUpdateSettingsError(): Error | null {
    return readError
}

export function reloadAppUpdateSettings(): AppUpdateSettings {
    current = undefined
    readError = null
    const next = getAppUpdateSettings()
    for (const listener of listeners) listener({ ...next })
    return next
}

export function updateAppUpdateSettings(
    patch: Partial<Omit<AppUpdateSettings, 'schema'>>,
): AppUpdateSettings {
    const next = validateAppUpdateSettings({ ...getAppUpdateSettings(), ...patch })
    localStorage.setItem(appUpdateSettingsKey, JSON.stringify(next))
    current = next
    readError = null
    for (const listener of listeners) listener({ ...next })
    return { ...next }
}

export function subscribeAppUpdateSettings(listener: (settings: AppUpdateSettings) => void): () => void {
    listeners.add(listener)
    return () => listeners.delete(listener)
}
