import { readFile, writeFile } from 'node:fs/promises'
import { build } from 'vite'
import { instrumentation } from './cdp.mjs'
import { baselineFrontendPlugin, startupObservationPlugin } from './observe.mjs'

// Only this isolated benchmark build embeds the observer, before the app module executes.
await build({ mode: 'agent', plugins: [baselineFrontendPlugin(), startupObservationPlugin()] })
const file = 'dist/index.html'
const html = await readFile(file, 'utf8')
if (!html.includes('</head>')) throw new Error('Benchmark HTML has no head')
await writeFile(file, html.replace('</head>', `<script>${instrumentation}</script></head>`))
