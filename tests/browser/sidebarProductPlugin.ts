import { resolve } from 'node:path'
import type { Plugin } from 'vite'
import { stubbed } from './autoStubPlugin'

export function sidebarProductBoundaries(): Plugin {
    return {
        name: 'sidebar-product-fixture-boundaries', enforce: 'pre',
        async resolveId(source, importer, options) {
            const owner = importer?.replaceAll('\\', '/')
            const adapter = resolve(import.meta.dirname, 'sidebarProductAdapters.svelte.ts').replaceAll('\\', '/')
            if (owner?.endsWith('/src/lib/SideBars/SidebarAvatar.svelte') && source.endsWith('/tooltip')) return stubbed(this, adapter, source, importer!, options)
            if (!owner?.endsWith('/src/lib/SideBars/Sidebar.svelte')) return null
            if (/(stores\.svelte|database\.svelte|characters|lang|util|globalApi\.svelte|folderManager\.svelte|guisize|workingSetNavigation)$/.test(source)) return stubbed(this, adapter, source, importer!, options)
            if (/\.svelte$/.test(source) && !/(stores|database)\.svelte$/.test(source)) {
                return /Sidebar(Avatar|Indicator)\.svelte$/.test(source) ? null : resolve(import.meta.dirname, 'Empty.svelte').replaceAll('\\', '/')
            }
            return null
        },
    }
}
