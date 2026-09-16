import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'
import { pluginStorageStore } from './plugins.svelte'

/**
 * A plugin started after an upstream save arrived gets one window in which a
 * value the save left without an owner may become its own. The window belongs
 * to one run of one plugin, closes when that run's top level script has
 * finished, and is never reopened. Only a read of a key the plugin does not
 * already hold reaches it: listing, writing and the full database entry points
 * never take an unowned value.
 */
export interface PluginClaimSession {
    claim(key: string): Promise<unknown | null>
    close(): Promise<void>
}

/** A safety bound on the window, not a promise about how long a plugin needs. */
export const PLUGIN_CLAIM_SESSION_LIMIT_MS = 30_000

async function codeHash(script: string): Promise<string> {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(script))
    return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join('')
}

export async function beginPluginClaimSession(plugin: {
    name: string
    script: string
}): Promise<PluginClaimSession | null> {
    if (!isTauri) return null
    const owner = plugin.name
    const hash = await codeHash(plugin.script)
    const runtimeInstance = crypto.randomUUID()
    const sessionId = await invoke<string | null>('pds_begin_plugin_claim_session', {
        owner,
        codeHash: hash,
        runtimeInstance,
    })
    if (!sessionId) return null

    let closed = false
    const close = async (): Promise<void> => {
        if (closed) return
        closed = true
        clearTimeout(bound)
        await invoke('pds_close_plugin_claim_session', { sessionId })
    }
    const bound = setTimeout(() => {
        void close().catch(() => undefined)
    }, PLUGIN_CLAIM_SESSION_LIMIT_MS)
    return {
        async claim(key: string): Promise<unknown | null> {
            if (closed) return null
            const value = await invoke<unknown | null>('pds_claim_plugin_storage_value', {
                sessionId,
                owner,
                codeHash: hash,
                runtimeInstance,
                key,
            })
            if (value === null || value === undefined) return null
            pluginStorageStore.invalidateOwner(owner)
            return value
        },
        close,
    }
}
