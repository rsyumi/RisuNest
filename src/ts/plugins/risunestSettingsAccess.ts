import {
    isPlainJsonObject,
    jsonKindOf,
    requireDatabaseRead,
    runDatabaseWrite,
    throwIfAborted,
    type RisunestHostContext,
    type RisunestJsonKind,
    type RisunestWriteResult,
} from './risunestHostContext'
import type { RisunestSettingDescription } from './risunestSettingDescriptions'
import { risunestOfferedSettingKeys } from './risunestSettingKeys'
import { productionRisunestSettingsDependencies } from './risunestSettingsRuntime.svelte'

// Settings by key: risunestListSettingKeys, risunestReadSettings, risunestWriteSettings.

export type RisunestSettingKeyInfo = {
    key: string
    type: RisunestJsonKind
    nullable: boolean
    writable: boolean
} & RisunestSettingDescription

export interface RisunestSettingsDependencies {
    flushPendingData(reason: string): Promise<void>
    /**
     * The value the app uses for each named root key; a key without a value has no entry. A
     * stored null of a key the app gives a default on load (`loreBookDepth`) reads as that default.
     */
    readRootValues(keys: readonly string[]): Record<string, unknown>
    /** The first key whose value the settings screens never set, such as an unknown color scheme name. */
    findUnknownValue(values: readonly (readonly [string, unknown])[]): Promise<string | null>
    /**
     * Replaces the whole value of each root key in one commit, whatever it was before, together
     * with the keys the settings screens change with it, then does what they do after the change.
     */
    commitRootValues(reason: string, values: readonly (readonly [string, unknown])[]): Promise<void>
    loadDescriptions(keys: ReadonlySet<string>): Promise<ReadonlyMap<string, RisunestSettingDescription>>
}

/** Why a value cannot be written to a key, checked without stored data. */
function writeRejection(key: string, value: unknown): string | null {
    const entry = risunestOfferedSettingKeys.get(key)
    if (!entry) return 'unavailable'
    if (entry.access !== 'available') return 'read-only'
    if (value === null ? !entry.nullable : jsonKindOf(value) !== entry.type) return 'type'
    return null
}

export function createRisunestSettingsAccess(
    context: RisunestHostContext,
    injected?: RisunestSettingsDependencies,
) {
    const dependencies = () => injected ?? productionRisunestSettingsDependencies
    return {
        async listSettingKeys(): Promise<{ items: RisunestSettingKeyInfo[]; total: number }> {
            throwIfAborted(context.lifetimeSignal)
            const descriptions = await dependencies().loadDescriptions(new Set(risunestOfferedSettingKeys.keys()))
            throwIfAborted(context.lifetimeSignal)
            const items = [...risunestOfferedSettingKeys].map(([key, entry]): RisunestSettingKeyInfo => ({
                key,
                type: entry.type,
                nullable: entry.nullable,
                writable: entry.access === 'available',
                ...descriptions.get(key),
            }))
            return { items, total: items.length }
        },
        async readSettings(keys?: unknown): Promise<{ values: Record<string, unknown>; unavailable: string[] }> {
            if (!Array.isArray(keys)) throw new TypeError('keys must be an array of strings')
            const requested = new Set<string>()
            for (let index = 0; index < keys.length; index++) {
                const key: unknown = keys[index]
                if (typeof key !== 'string') throw new TypeError('keys must be an array of strings')
                requested.add(key)
            }
            await requireDatabaseRead(context, 'risunestReadSettings')
            await dependencies().flushPendingData('plugin-settings-read')
            throwIfAborted(context.lifetimeSignal)
            return {
                values: dependencies().readRootValues([...requested].filter((key) => risunestOfferedSettingKeys.has(key))),
                unavailable: [...requested].filter((key) => !risunestOfferedSettingKeys.has(key)),
            }
        },
        async writeSettings(values?: unknown): Promise<RisunestWriteResult & { key?: string }> {
            if (!isPlainJsonObject(values)) throw new TypeError('values must be a plain object')
            const entries = Object.entries(values)
            for (const [key, value] of entries) {
                const reason = writeRejection(key, value)
                if (reason) return { status: 'rejected', reason, key }
            }
            const unknown = await dependencies().findUnknownValue(entries)
            throwIfAborted(context.lifetimeSignal)
            if (unknown) return { status: 'rejected', reason: 'value', key: unknown }
            return runDatabaseWrite(context, async () => {
                await dependencies().commitRootValues('plugin-settings-write', entries)
                return { status: 'applied' as const }
            })
        },
    }
}
