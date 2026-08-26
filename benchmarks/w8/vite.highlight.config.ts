import path from 'node:path'
import { fileURLToPath } from 'node:url'

import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'
import wasm from 'vite-plugin-wasm'

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..')

export default defineConfig({
    root: repositoryRoot,
    plugins: [
        svelte({
            preprocess: vitePreprocess(),
            onwarn: (warning, handler) => {
                if (warning.code.startsWith('a11y-')) return
                handler(warning)
            },
        }),
        wasm(),
    ],
    resolve: {
        alias: { src: path.join(repositoryRoot, 'src') },
    },
    build: {
        target: 'baseline-widely-available',
        minify: 'oxc',
        sourcemap: true,
        rolldownOptions: {
            input: path.join(repositoryRoot, 'benchmarks', 'w8', 'highlight-first-use.html'),
        },
    },
})
