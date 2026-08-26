import { resolve } from 'node:path'
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'
import wasm from 'vite-plugin-wasm'

const repositoryRoot = resolve(import.meta.dirname, '..', '..')

export default defineConfig({
  root: import.meta.dirname,
  publicDir: resolve(repositoryRoot, 'public'),
  plugins: [
    svelte({ preprocess: vitePreprocess() }),
    wasm(),
  ],
  build: {
    outDir: resolve(repositoryRoot, 'dist-lua-worker-pilot'),
    emptyOutDir: true,
    target: 'baseline-widely-available',
  },
  worker: {
    format: 'es',
    plugins: () => [wasm()],
  },
  optimizeDeps: {
    exclude: ['@browsermt/bergamot-translator'],
  },
})
