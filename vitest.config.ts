import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vitest/config'

const exclude = [
  '**/node_modules/**',
  '**/dist/**',
  '**/.git/**',
  '**/.worktrees/**',
  '**/.superpowers/**',
  '**/docs/research/**',
]

export default defineConfig({
  plugins: [svelte()],
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
