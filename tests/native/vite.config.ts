import { defineConfig } from 'vite'
import { resolve } from 'node:path'
import { svelte } from '@sveltejs/vite-plugin-svelte'
export default defineConfig(({ mode }) => {
    if (mode !== 'agent') throw new Error('Native boundary requires agent mode')
    return {
        root: resolve(import.meta.dirname),
        plugins: [{ name: 'boundary-agent-endpoints', enforce: 'pre',
            async resolveId(source, importer, options) {
                if (!/realmEndpoints(\.ts)?$/.test(source)) return null
                const resolved = await this.resolve(source, importer, { ...options, skipSelf: true })
                if (!resolved?.id.replaceAll('\\', '/').endsWith('/src/ts/realmEndpoints.ts')) return null
                return resolved.id.replace(/realmEndpoints\.ts$/, 'realmEndpoints.blocked.ts')
            },
        }, svelte({ compilerOptions: { experimental: { async: true } } })],
        resolve: { alias: { src: resolve(import.meta.dirname, '../../src') } },
        build: { outDir: resolve(import.meta.dirname, '../../.tmp/test-results/native/dist'), emptyOutDir: true },
    }
})
