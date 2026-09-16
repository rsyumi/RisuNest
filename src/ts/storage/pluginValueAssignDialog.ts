import { mount, unmount } from 'svelte'
import type { NativeStagedPluginChoice, NativeStagedPluginPreview } from './nativeFileJobs'

let open = false

/**
 * The one pass over an imported save's plugin values, taken before the
 * replacement is applied. Answering with nothing cancels the import.
 */
export async function selectPluginValueAssignment(
    preview: NativeStagedPluginPreview,
    remembered: NativeStagedPluginChoice | null = null,
): Promise<NativeStagedPluginChoice | null> {
    if (preview.values.length === 0) return { assignments: [], automatic: true }
    if (open) throw new Error('A plugin value assignment is already open')
    // Loaded here so an import that never meets an unowned value never pays for
    // the screen it would have shown.
    const { default: PluginValueAssignDialog } = await import(
        '../../lib/Setting/RisuNest/PluginValueAssignDialog.svelte'
    )
    open = true
    return await new Promise((resolve) => {
        const target = document.createElement('div')
        document.body.append(target)
        let component: ReturnType<typeof mount> | null = null
        let settled = false
        const finish = (choice: NativeStagedPluginChoice | null) => {
            if (settled) return
            settled = true
            queueMicrotask(() => {
                void (component ? unmount(component) : Promise.resolve()).finally(() => {
                    target.remove()
                    open = false
                    resolve(choice)
                })
            })
        }
        component = mount(PluginValueAssignDialog, {
            target,
            props: {
                values: preview.values,
                pluginNames: preview.pluginNames,
                remembered,
                onchoose: finish,
            },
        })
    })
}
