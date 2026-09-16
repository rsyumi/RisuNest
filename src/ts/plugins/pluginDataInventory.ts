import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'
import { getPersistentDataStore } from '../storage/persistentDataStoreFactory'
import { isUnownedPluginOwner, UNOWNED_PLUGIN_OWNER } from './pluginOwner'
import {
    createBrowserPluginDeviceBackend,
    type PluginDeviceSpace,
} from './pluginDeviceKeyspace'
import { pluginStorageStore } from './plugins.svelte'

/** Which store a value lives in. The device store never leaves this machine. */
export type PluginDataScope = 'library' | 'device'

export interface PluginDataItem {
    owner: string
    key: string
    /** Set for device values only, where a plugin chose which space to write in. */
    space?: PluginDeviceSpace
    valueType: 'json' | 'string'
    byteSize: number
    /** Set when the value moved to its owner on its own after an import. */
    automatic: boolean
    assignedAt?: number
}

export type PluginAssignCollision = 'replace' | 'discard' | 'defer'

export interface PluginAssignOutcome {
    moved: number
    replaced: number
    discarded: number
    deferred: number
}

interface DeviceListRow {
    owner: string
    space: PluginDeviceSpace
    key: string
    byteSize: number
}

async function deviceRows(): Promise<DeviceListRow[]> {
    if (isTauri) return await invoke<DeviceListRow[]>('pds_list_plugin_device_storage')
    // The web build has no device tier, so this only answers what a test seeded.
    return []
}

export async function listPluginDataItems(
    scope: PluginDataScope,
): Promise<PluginDataItem[]> {
    if (scope === 'device') {
        return (await deviceRows()).map((row) => ({
            owner: row.owner,
            key: row.key,
            space: row.space,
            valueType: row.space === 'json' ? 'json' : 'string',
            byteSize: row.byteSize,
            automatic: false,
        }))
    }
    const items = await getPersistentDataStore().listPluginStorage()
    return items
        .filter((item) => item.space === undefined)
        .map((item) => ({
            owner: item.owner,
            key: item.key,
            valueType: item.valueType === 'string' ? 'string' : 'json',
            byteSize: item.byteSize,
            automatic: item.claimedFrom === 'unowned',
            assignedAt: item.assignedAt,
        }))
}

export async function readPluginDataValue(item: PluginDataItem): Promise<string | null> {
    if (item.space !== undefined) {
        if (!isTauri) {
            return await createBrowserPluginDeviceBackend().read(item.owner, item.space, item.key)
        }
        return await invoke<string | null>('pds_read_plugin_device_value', {
            owner: item.owner,
            space: item.space,
            key: item.key,
        })
    }
    const stored = await getPersistentDataStore().readPluginStorage(item.owner, item.key)
    if (!stored) return null
    return typeof stored.value === 'string'
        ? stored.value
        : JSON.stringify(stored.value, null, 2)
}

export async function deletePluginDataItems(items: readonly PluginDataItem[]): Promise<void> {
    const byOwner = new Map<string, PluginDataItem[]>()
    for (const item of items) {
        const owned = byOwner.get(item.owner) ?? []
        owned.push(item)
        byOwner.set(item.owner, owned)
    }
    for (const [owner, owned] of byOwner) {
        const device = owned.filter((item) => item.space !== undefined)
        const library = owned.filter((item) => item.space === undefined)
        if (library.length > 0) {
            await pluginStorageStore
                .forOwner(owner)
                .mutate(library.map((item) => ({ type: 'delete', key: item.key })))
        }
        if (device.length === 0) continue
        const mutations = device.map((item) => ({
            type: 'delete' as const,
            space: item.space as PluginDeviceSpace,
            key: item.key,
        }))
        if (isTauri) {
            await invoke('pds_write_plugin_device_values', { owner, mutations })
        } else {
            await createBrowserPluginDeviceBackend().write(owner, mutations)
        }
    }
}

export async function collidingPluginDataKeys(
    owner: string,
    keys: readonly string[],
): Promise<string[]> {
    if (!isTauri || keys.length === 0) return []
    return await invoke<string[]>('pds_colliding_plugin_storage_keys', {
        owner,
        keys: [...keys],
    })
}

export async function assignPluginDataItems(
    items: readonly PluginDataItem[],
    owner: string,
    collision: PluginAssignCollision,
): Promise<PluginAssignOutcome> {
    if (!isTauri || items.length === 0) {
        return { moved: 0, replaced: 0, discarded: 0, deferred: 0 }
    }
    // An assignment moves rows, so the write coordinator owns the revision.
    const { getPersistentDataRuntime } = await import('../storage/persistentDataRuntime.svelte')
    let outcome: PluginAssignOutcome = { moved: 0, replaced: 0, discarded: 0, deferred: 0 }
    await getPersistentDataRuntime().runStorageOnlyMutation(async (expectedRevision) => {
        const answer = await invoke<PluginAssignOutcome & { revision: number }>(
            'pds_assign_plugin_storage',
            {
                owner,
                sources: items.map((item) => ({ owner: item.owner, key: item.key })),
                collision,
                expectedRevision,
            },
        )
        outcome = {
            moved: answer.moved,
            replaced: answer.replaced,
            discarded: answer.discarded,
            deferred: answer.deferred,
        }
        return answer.revision
    })
    pluginStorageStore.invalidate()
    return outcome
}

/** The leading run up to and including the first separator, when there is one. */
export function pluginKeyPrefix(key: string): string | null {
    const match = /^[^_\-:.]+[_\-:.]/.exec(key)
    return match ? match[0] : null
}

export interface PluginDataGroup {
    prefix: string | null
    items: PluginDataItem[]
    byteSize: number
}

/** Groups only exist to select many keys at once, never as a recommendation. */
export function groupPluginDataByPrefix(
    items: readonly PluginDataItem[],
): PluginDataGroup[] {
    const groups = new Map<string, PluginDataItem[]>()
    const ungrouped: PluginDataItem[] = []
    for (const item of items) {
        const prefix = pluginKeyPrefix(item.key)
        if (prefix === null) {
            ungrouped.push(item)
            continue
        }
        const group = groups.get(prefix) ?? []
        group.push(item)
        groups.set(prefix, group)
    }
    const result: PluginDataGroup[] = []
    for (const [prefix, group] of groups) {
        if (group.length < 2) {
            ungrouped.push(...group)
            continue
        }
        result.push({ prefix, items: group, byteSize: totalPluginDataBytes(group) })
    }
    result.sort((left, right) => (left.prefix ?? '').localeCompare(right.prefix ?? ''))
    if (ungrouped.length > 0) {
        ungrouped.sort((left, right) => left.key.localeCompare(right.key))
        result.push({
            prefix: null,
            items: ungrouped,
            byteSize: totalPluginDataBytes(ungrouped),
        })
    }
    return result
}

export function totalPluginDataBytes(items: readonly PluginDataItem[]): number {
    return items.reduce((total, item) => total + item.byteSize, 0)
}

export interface PluginDataAssignmentChoice {
    owner: string
    keys: readonly string[]
}

export interface PluginDataAssignmentPrefill {
    selectedIds: string[]
    groupOwners: { prefix: string | null; owner: string }[]
}

/**
 * Reads assignments back into the selection they were made in. Keys the items
 * no longer hold and plugins the list no longer offers are left out, and a
 * bundle keeps the one owner it can show.
 */
export function pluginDataAssignmentPrefill(
    items: readonly PluginDataItem[],
    assignments: readonly PluginDataAssignmentChoice[],
    owners: readonly string[],
): PluginDataAssignmentPrefill {
    const offered = new Set(owners)
    const ownerByKey = new Map<string, string>()
    for (const assignment of assignments) {
        if (!offered.has(assignment.owner)) continue
        for (const key of assignment.keys) {
            if (!ownerByKey.has(key)) ownerByKey.set(key, assignment.owner)
        }
    }
    const prefill: PluginDataAssignmentPrefill = { selectedIds: [], groupOwners: [] }
    for (const group of groupPluginDataByPrefix(items)) {
        let owner: string | undefined
        for (const item of group.items) {
            const chosen = ownerByKey.get(item.key)
            if (chosen === undefined) continue
            owner ??= chosen
            if (chosen !== owner) continue
            prefill.selectedIds.push(pluginDataItemId(item))
        }
        if (owner !== undefined) prefill.groupOwners.push({ prefix: group.prefix, owner })
    }
    return prefill
}

export interface PluginOwnerBucket {
    owner: string
    count: number
    byteSize: number
}

export function pluginDataOwnerBuckets(
    items: readonly PluginDataItem[],
): PluginOwnerBucket[] {
    const buckets = new Map<string, PluginOwnerBucket>()
    for (const item of items) {
        const bucket = buckets.get(item.owner) ?? {
            owner: item.owner,
            count: 0,
            byteSize: 0,
        }
        bucket.count += 1
        bucket.byteSize += item.byteSize
        buckets.set(item.owner, bucket)
    }
    const named = [...buckets.values()].filter((bucket) => !isUnownedPluginOwner(bucket.owner))
    named.sort((left, right) => right.byteSize - left.byteSize)
    const unowned = buckets.get(UNOWNED_PLUGIN_OWNER)
    return unowned ? [...named, unowned] : named
}

export interface PluginDataFilter {
    owner: string | null
    automaticOnly: boolean
    key: string
    value: string
}

export function filterPluginDataItems(
    items: readonly PluginDataItem[],
    filter: PluginDataFilter,
    values: ReadonlyMap<string, string>,
): PluginDataItem[] {
    const key = filter.key.trim().toLowerCase()
    const value = filter.value.trim().toLowerCase()
    return items.filter((item) => {
        if (filter.automaticOnly && !item.automatic) return false
        if (filter.owner !== null && item.owner !== filter.owner) return false
        if (key.length > 0 && !item.key.toLowerCase().includes(key)) return false
        if (value.length > 0) {
            const stored = values.get(pluginDataItemId(item))
            if (stored === undefined || !stored.toLowerCase().includes(value)) return false
        }
        return true
    })
}

export function pluginDataItemId(item: PluginDataItem): string {
    return JSON.stringify([item.space ?? '', item.owner, item.key])
}
