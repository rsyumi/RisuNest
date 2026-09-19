import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vitest/config'
import { responsesInternalsPlugin } from './support/responsesInternals'
import { separateRunnerPaths, testExcludes } from './suiteOwnership.mjs'

export function sharedVitestConfig() {
  return defineConfig({
    plugins: [responsesInternalsPlugin(), svelte()],
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
