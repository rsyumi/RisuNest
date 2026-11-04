import { spawnSync } from 'node:child_process'
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')

// Rolldown gives asset file references a new ID on each build, and source maps carry the
// referencing module sources verbatim.
const fileReference = /import\.meta\.ROLLDOWN_FILE_URL_[\w-]+/g

function sameOutput(name, current, next) {
  if (current.equals(next)) return true
  return name.endsWith('.map')
    && current.toString('latin1').replace(fileReference, '') === next.toString('latin1').replace(fileReference, '')
}

// Mirrors `source` into `target` without touching files whose output did not change, so
// Cargo keeps the app crate that embeds `target` fresh. Every file written gets the
// current time, including Vite's copies of `public/`, which keep their source time on
// Windows.
export function syncOutput(source, target) {
  mkdirSync(target, { recursive: true })
  const entries = new Map(readdirSync(source, { withFileTypes: true }).map(entry => [entry.name, entry]))
  for (const entry of readdirSync(target, { withFileTypes: true })) {
    const next = entries.get(entry.name)
    if (!next || next.isDirectory() !== entry.isDirectory()) rmSync(join(target, entry.name), { recursive: true, force: true })
  }
  for (const [name, entry] of entries) {
    const from = join(source, name)
    const to = join(target, name)
    if (entry.isDirectory()) {
      syncOutput(from, to)
      continue
    }
    const next = readFileSync(from)
    let current = null
    try {
      current = readFileSync(to)
    } catch (error) {
      if (error.code !== 'ENOENT') throw error
    }
    if (!current || !sameOutput(name, current, next)) writeFileSync(to, next)
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  // A sibling of dist/, so source map paths match a build written to dist/ directly.
  const staging = join(root, 'dist-staging')
  const require = createRequire(import.meta.url)
  const vite = join(dirname(require.resolve('vite/package.json')), 'bin', 'vite.js')
  const result = spawnSync(process.execPath, [vite, 'build', ...process.argv.slice(2), '--outDir', staging, '--emptyOutDir'], {
    cwd: root, stdio: 'inherit', windowsHide: true,
  })
  if (result.error) throw result.error
  if (result.status === 0) {
    syncOutput(staging, join(root, 'dist'))
    rmSync(staging, { recursive: true, force: true, maxRetries: 3 })
  }
  process.exitCode = result.status ?? 1
}
