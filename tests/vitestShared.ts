import { svelte } from '@sveltejs/vite-plugin-svelte'
import { createRequire } from 'node:module'
import { defineConfig } from 'vitest/config'
import { responsesInternalsPlugin } from './support/responsesInternals'
import { separateRunnerPaths, testExcludes } from './suiteOwnership.mjs'

export function sharedVitestConfig() {
  return defineConfig({
    plugins: [responsesInternalsPlugin(), svelte({ compilerOptions: { experimental: { async: true } } }), {
      name: 'lua-node-wasm-asset',
      enforce: 'pre',
      resolveId(id) {
        if (id === 'wasmoon/dist/glue.wasm?url') return '\0lua-node-wasm-asset'
      },
      load(id) {
        if (id === '\0lua-node-wasm-asset') {
          return `export default ${JSON.stringify(createRequire(import.meta.url).resolve('wasmoon/dist/glue.wasm'))}`
        }
      },
    }],
    resolve: {
      alias: { src: '/src' },
      conditions: ['browser'],
    },
    test: {
      exclude: [...testExcludes, ...separateRunnerPaths],
      benchmark: { exclude: [...testExcludes, ...separateRunnerPaths] },
      environment: 'happy-dom',
      setupFiles: ['vitest.setup.ts'],
      isolate: true,
    },
  })
}
