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

/**
 * Plugins reload while the replacement that brought the values in still holds
 * the write fence, so a claim made right then waits for it instead of missing.
 */
const FENCE_WAIT_LIMIT_MS = 5_000
const FENCE_RETRY_DELAY_MS = 50

function isFenced(error: unknown): boolean {
    return error instanceof Error && error.name === 'PersistentMutationFencedError'
}

function delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms))
}

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
    let reportedFailure = false
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
            // A claim moves a row, so the write coordinator has to own the
            // revision it leaves behind rather than meet it as a conflict.
            let claimed: unknown | null = null
            const { getPersistentDataRuntime } = await import(
                '../storage/persistentDataRuntime.svelte'
            )
            const fenceDeadline = Date.now() + FENCE_WAIT_LIMIT_MS
            for (;;) {
                try {
                    await getPersistentDataRuntime().runStorageOnlyMutation(
                        async (expectedRevision) => {
                            const answer = await invoke<{
                                value: unknown | null
                                revision: number
                            }>('pds_claim_plugin_storage_value', {
                                sessionId,
                                owner,
                                codeHash: hash,
                                runtimeInstance,
                                key,
                                expectedRevision,
                            })
                            claimed = answer.value ?? null
                            return answer.revision
                        },
                    )
                    break
                } catch (error) {
                    if (isFenced(error) && !closed && Date.now() < fenceDeadline) {
                        await delay(FENCE_RETRY_DELAY_MS)
                        continue
                    }
                    // A read answers a key it cannot take with a miss, because
                    // the plugin API has no other answer for one.
                    if (!reportedFailure) {
                        reportedFailure = true
                        console.warn(
                            `[RisuAI Plugin: ${owner}] Could not take an unassigned value.`,
                            error,
                        )
                    }
                    return null
                }
            }
            if (claimed === null) return null
            pluginStorageStore.invalidateOwner(owner)
            return claimed
        },
        close,
    }
}
