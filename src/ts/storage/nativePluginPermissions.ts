import { invoke } from '@tauri-apps/api/core'
import localforage from 'localforage'
import { isTauri } from '../platform'

/**
 * Plugin consent belongs to this installation. A native install keeps it in the
 * device file; the web build has no device tier and keeps it in the browser.
 *
 * A restored grant never stands in for the checks: the caller still hashes the
 * script it is about to run and judges the permission again.
 */
export interface PluginPermissionStore {
    isGranted(codeHash: string, permission: string): Promise<boolean>
    grant(codeHash: string, permission: string): Promise<void>
    lastGrantAt(pluginName: string, permission: string): Promise<number | null>
    recordGrant(pluginName: string, permission: string, at: number): Promise<void>
    clearAll(): Promise<void>
}

interface NativePermissionState {
    permissions: { codeHash: string, permission: string, granted: boolean }[]
    grants: { pluginName: string, permission: string, lastGrantAt: number }[]
}

export type PluginPermissionInvoke =
    (command: string, args?: Record<string, unknown>) => Promise<unknown>

const pairKey = (first: string, second: string) => `${first}\u0000${second}`

export function createNativePluginPermissionStore(
    invokeCommand: PluginPermissionInvoke = invoke,
): PluginPermissionStore {
    // One read covers every plugin, so a start that runs several of them does
    // not pay a round trip each.
    let loaded: Promise<{ granted: Set<string>, grants: Map<string, number> }> | null = null
    const load = () => {
        loaded ??= (async () => {
            const state = await invokeCommand('pds_read_plugin_permissions') as NativePermissionState
            const granted = new Set<string>()
            for (const row of state.permissions) {
                if (row.granted) granted.add(pairKey(row.codeHash, row.permission))
            }
            const grants = new Map<string, number>()
            for (const row of state.grants) {
                grants.set(pairKey(row.pluginName, row.permission), row.lastGrantAt)
            }
            return { granted, grants }
        })()
        return loaded
    }
    return {
        async isGranted(codeHash, permission) {
            return (await load()).granted.has(pairKey(codeHash, permission))
        },
        async grant(codeHash, permission) {
            await invokeCommand('pds_write_plugin_permission', {
                codeHash,
                permission,
                granted: true,
            })
            ;(await load()).granted.add(pairKey(codeHash, permission))
        },
        async lastGrantAt(pluginName, permission) {
            return (await load()).grants.get(pairKey(pluginName, permission)) ?? null
        },
        async recordGrant(pluginName, permission, at) {
            await invokeCommand('pds_write_plugin_permission_grant', {
                pluginName,
                permission,
                lastGrantAt: at,
            })
            ;(await load()).grants.set(pairKey(pluginName, permission), at)
        },
        async clearAll() {
            await invokeCommand('pds_clear_plugin_permissions')
            if (loaded) {
                const state = await loaded
                state.granted.clear()
                state.grants.clear()
            }
        },
    }
}

/** The instance is created on first use, so a native install never opens it. */
export function createLocalPluginPermissionStore(
    createInstance: () => LocalForage = () => localforage.createInstance({
        name: 'plugin_permissions',
        storeName: 'plugin_permissions',
    }),
): PluginPermissionStore {
    let forage: LocalForage | null = null
    const store = () => (forage ??= createInstance())
    return {
        async isGranted(codeHash, permission) {
            return Boolean(await store().getItem(`${codeHash}_${permission}`))
        },
        async grant(codeHash, permission) {
            await store().setItem(`${codeHash}_${permission}`, true)
        },
        async lastGrantAt(pluginName, permission) {
            return await store().getItem<number>(`${pluginName}_${permission}_lastGrantTime`)
        },
        async recordGrant(pluginName, permission, at) {
            await store().setItem(`${pluginName}_${permission}_lastGrantTime`, at)
        },
        async clearAll() {
            await store().clear()
        },
    }
}

let shared: PluginPermissionStore | null = null

export function getPluginPermissionStore(): PluginPermissionStore {
    if (shared) return shared
    shared = isTauri
        ? createNativePluginPermissionStore()
        : createLocalPluginPermissionStore()
    return shared
}

export function installPluginPermissionStore(store: PluginPermissionStore | null): void {
    shared = store
}
