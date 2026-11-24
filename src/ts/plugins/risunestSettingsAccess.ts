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
import { risunestSettingKeyClasses } from './risunestSettingKeys'
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
    /** The current value of each named root key; a key without a value has no entry. */
    readRootValues(keys: readonly string[]): Record<string, unknown>
    /** Replaces the whole value of each root key in one commit, whatever it was before. */
    commitRootValues(reason: string, values: readonly (readonly [string, unknown])[]): Promise<void>
    loadDescriptions(keys: ReadonlySet<string>): Promise<ReadonlyMap<string, RisunestSettingDescription>>
}

const listedKeys = new Map([...risunestSettingKeyClasses].flatMap(([key, entry]) =>
    entry.access === 'excluded' ? [] : [[key, entry] as const]))
const describedKeys: ReadonlySet<string> = new Set(listedKeys.keys())

/** Why a value cannot be written to a key, checked without stored data. */
function writeRejection(key: string, value: unknown): string | null {
    const entry = listedKeys.get(key)
    if (!entry) return 'unavailable'
    if (entry.access !== 'available') return 'read-only'
    if (value === null ? !entry.nullable : jsonKindOf(value) !== entry.type) return 'type'
    return null
}

export function createRisunestSettingsAccess(
    context: RisunestHostContext,
    dependencies: RisunestSettingsDependencies = productionRisunestSettingsDependencies,
) {
    return {
        async listSettingKeys(): Promise<{ items: RisunestSettingKeyInfo[]; total: number }> {
            throwIfAborted(context.lifetimeSignal)
            const descriptions = await dependencies.loadDescriptions(describedKeys)
            throwIfAborted(context.lifetimeSignal)
            const items = [...listedKeys].map(([key, entry]): RisunestSettingKeyInfo => ({
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
            await dependencies.flushPendingData('plugin-settings-read')
            throwIfAborted(context.lifetimeSignal)
            return {
                values: dependencies.readRootValues([...requested].filter((key) => listedKeys.has(key))),
                unavailable: [...requested].filter((key) => !listedKeys.has(key)),
            }
        },
        async writeSettings(values?: unknown): Promise<RisunestWriteResult & { key?: string }> {
            if (!isPlainJsonObject(values)) throw new TypeError('values must be a plain object')
            const entries = Object.entries(values)
            for (const [key, value] of entries) {
                const reason = writeRejection(key, value)
                if (reason) return { status: 'rejected', reason, key }
            }
            return runDatabaseWrite(context, async () => {
                await dependencies.commitRootValues('plugin-settings-write', entries)
                return { status: 'applied' as const }
            })
        },
    }
}
