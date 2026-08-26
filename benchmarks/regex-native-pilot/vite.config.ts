import { resolve } from 'node:path'
import { defineConfig } from 'vite'

const repositoryRoot = resolve(import.meta.dirname, '..', '..')

export default defineConfig({
    root: import.meta.dirname,
    build: {
        outDir: resolve(repositoryRoot, 'node_modules', '.cache', 'regex-native-pilot'),
        emptyOutDir: true,
        target: 'baseline-widely-available',
    },
    worker: {
        format: 'es',
    },
})
