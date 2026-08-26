import { defineConfig, type Plugin } from 'vite'

import baseConfig from '../../vite.config'

const sortableShimId = '\0risunest-w8-sortable-upper-bound'

function sortableUpperBoundPlugin(): Plugin {
    return {
        name: 'risunest-w8-sortable-upper-bound',
        enforce: 'pre',
        resolveId(source) {
            if (source === 'sortablejs' || source.startsWith('sortablejs/')) return sortableShimId
        },
        load(id) {
            if (id !== sortableShimId) return
            return `
                class SortableUpperBoundShim {
                    static create() { return new SortableUpperBoundShim() }
                    destroy() {}
                }
                export default SortableUpperBoundShim
            `
        },
    }
}

export default defineConfig(async (environment) => {
    const resolved = typeof baseConfig === 'function' ? await baseConfig(environment) : baseConfig
    return {
        ...resolved,
        plugins: [sortableUpperBoundPlugin(), ...(resolved.plugins ?? [])],
    }
})
