import { get, writable } from 'svelte/store'
import { alertError } from '../alert'
import { parsePluginHeader, stripPluginBom } from '../plugins/pluginHeader'
import { importPlugin, type RisuPlugin } from '../plugins/plugins.svelte'
import { DBState } from '../stores.svelte'
import type { ImportIntakeItem, ImportIntakePluginHeader, ImportIntakeRefusal } from './types'

export type PluginInstallResult = 'installed' | 'cancelled' | 'failed'

/** What the plugin install dialog shows for one plugin file. */
export interface PluginInstallPrompt {
    header: ImportIntakePluginHeader
    fileName: string
    bytes?: number
    /** The installed plugin with the same `//@name`, when installing replaces it. */
    replaces?: { name: string; displayName?: string; version?: string }
}

export interface ActivePluginInstall {
    prompt: PluginInstallPrompt
    /** Installs the plugin; the dialog closes once the install settles. */
    install(): Promise<void>
    /** Closes the dialog without installing; no effect once the install started. */
    cancel(): void
}

/** The request the plugin install dialog is showing; requests wait their turn one at a time. */
export const pluginInstallDialog = writable<ActivePluginInstall | null>(null)

const waiting: Array<() => void> = []

function showNextPluginInstall(): void {
    if (get(pluginInstallDialog)) return
    waiting.shift()?.()
}

function updateHostOf(updateURL: string): string | undefined {
    if (!updateURL) return undefined
    try {
        return new URL(updateURL).host || undefined
    } catch {
        return undefined
    }
}

/** Reads a plugin file's header, or says why it cannot be installed. */
export function inspectPluginFile(code: string): { header: ImportIntakePluginHeader } | { refusal: ImportIntakeRefusal } {
    // Other header problems are reported by importPlugin when the user installs.
    const { header } = parsePluginHeader(stripPluginBom(code))
    if (!header.name) return { refusal: { code: 'unsupported-format' } }
    if (header.apiVersion !== '3.0') return { refusal: { code: 'plugin-api-version', version: header.apiVersion } }
    return {
        header: {
            name: header.name,
            displayName: header.displayName || undefined,
            version: header.versionOfPlugin || undefined,
            apiVersion: header.apiVersion,
            updateHost: updateHostOf(header.updateURL),
        },
    }
}

function pluginInstallPrompt(item: ImportIntakeItem, header: ImportIntakePluginHeader): PluginInstallPrompt {
    const installed = (DBState.db.plugins as RisuPlugin[] | undefined)?.find(plugin => plugin.name === header.name)
    return {
        header,
        fileName: item.name,
        bytes: item.bytes,
        replaces: installed && {
            name: installed.name,
            displayName: installed.displayName || undefined,
            version: installed.versionOfPlugin || undefined,
        },
    }
}

/** Shows the plugin install dialog for one plugin item and installs it on confirmation. */
export function requestPluginInstall(item: ImportIntakeItem): Promise<PluginInstallResult> {
    const plugin = item.plugin
    if (!plugin) return Promise.reject(new Error(`${item.name} is not a plugin file`))
    return new Promise(resolve => {
        waiting.push(() => {
            let settled = false
            const finish = (result: PluginInstallResult) => {
                if (get(pluginInstallDialog) === active) pluginInstallDialog.set(null)
                resolve(result)
                showNextPluginInstall()
            }
            const active: ActivePluginInstall = {
                // Read when shown, so an earlier request that installed the same name makes this one a replacement.
                prompt: pluginInstallPrompt(item, plugin.header),
                async install() {
                    if (settled) return
                    settled = true
                    let installed = false
                    try {
                        installed = await importPlugin(stripPluginBom(plugin.code), {
                            isTypescript: plugin.isTypescript,
                            confirmed: true,
                        })
                    } catch (error) {
                        // importPlugin alerts its own refusals; anything it throws is shown here.
                        alertError(error instanceof Error ? error : String(error))
                    } finally {
                        finish(installed ? 'installed' : 'failed')
                    }
                },
                cancel() {
                    if (settled) return
                    settled = true
                    finish('cancelled')
                },
            }
            pluginInstallDialog.set(active)
        })
        showNextPluginInstall()
    })
}
