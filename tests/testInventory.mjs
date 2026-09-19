import { spawnSync } from 'node:child_process'
import { createRequire } from 'node:module'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { assertInventory, discoverTestEntries, normalizePath, selectNodeTests } from './suiteOwnership.mjs'

export function parseVitestInventory(output, root, owner) {
  const entries = JSON.parse(output)
  if (!Array.isArray(entries) || entries.length === 0) throw new Error(`Empty or invalid ${owner ?? 'root'} Vitest inventory`)
  const result = owner ? { [owner]: [] } : { app: [], 'app-extended': [], harness: [] }
  for (const entry of entries) {
    if (!entry || typeof entry.file !== 'string') throw new Error('Unexpected Vitest list output')
    const name = owner ?? entry.projectName
    if (!Object.hasOwn(result, name)) throw new Error(`Unknown Vitest project: ${name}`)
    const path = normalizePath(relative(root, entry.file))
    if (path.startsWith('../') || path.includes(':')) throw new Error(`Test outside repository: ${entry.file}`)
    result[name].push(path)
  }
  return result
}

export function vitestInventory(root, directory = '.', owner) {
  const cwd = join(root, directory)
  const require = createRequire(pathToFileURL(join(cwd, 'package.json')))
  const cli = join(dirname(require.resolve('vitest/package.json')), 'vitest.mjs')
  const directoryRoot = mkdtempSync(join(tmpdir(), 'risunest-vitest-inventory-'))
  try {
    const output = join(directoryRoot, 'inventory.json')
    const result = spawnSync(process.execPath, [cli, 'list', '--filesOnly', `--json=${output}`], {
      cwd, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024,
    })
    if (result.error) throw result.error
    if (result.status !== 0) throw new Error(`Vitest inventory failed (${owner ?? 'root'}): ${result.signal ?? result.status}\n${result.stdout}\n${result.stderr}`)
    return parseVitestInventory(readFileSync(output, 'utf8'), root, owner)
  } finally {
    rmSync(directoryRoot, { recursive: true, force: true })
  }
}

export async function main() {
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
  const files = await discoverTestEntries(root)
  const inventories = {
    ...vitestInventory(root),
    ...vitestInventory(root, 'server/manager/gui', 'manager'),
    ...vitestInventory(root, 'server/endpoint-registry', 'registry'),
    node: selectNodeTests(files),
  }
  assertInventory(files, inventories)
  for (const [owner, selected] of Object.entries(inventories)) console.log(`${owner}: ${selected.length} files`)
  console.log('Every discovered test entry has exactly one runner owner.')
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  main().catch(error => { console.error(error); process.exitCode = 1 })
}
