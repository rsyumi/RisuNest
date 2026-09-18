import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vitest/config'
import { responsesInternalsPlugin } from './tests/support/responsesInternals'

const exclude = [
  '**/node_modules/**',
  '**/dist/**',
  '**/.git/**',
  '**/.tmp/**',
  '**/.worktrees/**',
  '**/.superpowers/**',
  '**/docs/research/**',
  // These suites use node:test and run with the Node test runner.
  'crates/sync-wire/tests/golden.test.mjs',
  'benchmarks/sync-server/transfer-comparison.test.mjs',
]

export default defineConfig({
  plugins: [responsesInternalsPlugin(), svelte()],
  resolve: {
    alias: {
      src: '/src',
    },
    conditions: ['browser'],
  },
  test: {
    exclude,
    benchmark: { exclude },
    environment: 'happy-dom',
    setupFiles: ['vitest.setup.ts'],
  },
})
