import { readdir } from 'node:fs/promises'
import { join } from 'node:path'

export const testSuffix = '**/*.{test,spec}.?(c|m)[jt]s?(x)'
export const appRoots = ['src', 'tests', 'scripts', 'src-tauri/src']
export const appTestIncludes = appRoots.map(root => `${root}/${testSuffix}`)
export const extendedAppTests = ['src/ts/storage/selectedConversationEvictionCorpus.test.ts']
export const harnessVitestTests = [
  `benchmarks/${testSuffix}`,
  'src/ts/process/luaWorkerPilotClient.test.ts',
]
export const nodeTestExceptions = [
  'crates/sync-wire/tests/golden.test.mjs',
  'benchmarks/sync-server/transfer-comparison.test.mjs',
]
export const separateRunnerPaths = ['tests/browser/**', 'tests/native/**', ...nodeTestExceptions]
export const ignoredDirectories = new Set([
  'node_modules', 'dist', '.git', '.tmp', '.worktrees', '.superpowers',
  'target', 'build', '.gradle', '.svelte-kit', 'coverage', 'graft', 'docs',
])
export const testExcludes = [...ignoredDirectories].map(name => `**/${name}/**`)

export const normalizePath = path => path.replaceAll('\\', '/').replace(/^\.\//, '')
const ordinaryTest = /\.(?:test|spec)\.[cm]?[jt]sx?$/
const under = (path, root) => path.startsWith(`${root}/`)

export function isTestEntry(path) {
  path = normalizePath(path)
  return ordinaryTest.test(path) || path.endsWith('.node-test.mjs') || /(?:^|\/)tests\/.*\.worker\.ts$/.test(path)
}

export function ownerOf(path) {
  path = normalizePath(path)
  if (!isTestEntry(path)) return null
  if (nodeTestExceptions.includes(path)
    || (path.endsWith('.node-test.mjs') && ['tests', 'benchmarks'].some(root => under(path, root)))) return 'node'
  if (under(path, 'tests/browser')) return 'browser'
  if (under(path, 'tests/native')) return 'native'
  if (under(path, 'server/endpoint-registry/tests') && path.endsWith('.worker.ts')) return 'registry'
  if (under(path, 'server/manager/gui') && ordinaryTest.test(path)) return 'manager'
  if (extendedAppTests.includes(path)) return 'app-extended'
  if (ordinaryTest.test(path) && (under(path, 'benchmarks') || harnessVitestTests.includes(path))) return 'harness'
  if (ordinaryTest.test(path) && appRoots.some(root => under(path, root))) return 'app'
  throw new Error(`Unowned test entry: ${path}`)
}

// Walk independently of Vitest include patterns so new unowned roots cannot disappear silently.
export async function discoverTestEntries(root, relative = '') {
  const files = []
  for (const entry of await readdir(join(root, relative), { withFileTypes: true })) {
    if (entry.isSymbolicLink()) continue
    const path = relative ? `${relative}/${entry.name}` : entry.name
    if (entry.isDirectory() && !ignoredDirectories.has(entry.name)) {
      files.push(...await discoverTestEntries(root, path))
    } else if (entry.isFile() && isTestEntry(path)) {
      files.push(path)
    }
  }
  return files.sort()
}

export function selectNodeTests(files) {
  const selected = files.map(normalizePath).filter(path => ownerOf(path) === 'node').sort()
  if (selected.length === 0) throw new Error('No Node test entries found')
  if (new Set(selected).size !== selected.length) throw new Error('Duplicate Node test entries')
  return selected
}

export function assertInventory(files, inventories) {
  const expected = new Map(files.map(path => [normalizePath(path), ownerOf(path)]).filter(([, owner]) => owner !== null))
  const seen = new Set()
  for (const [owner, selected] of Object.entries(inventories)) {
    for (const rawPath of selected) {
      const path = normalizePath(rawPath)
      if (seen.has(path)) throw new Error(`Duplicate test ownership: ${path}`)
      seen.add(path)
      if (expected.get(path) !== owner) throw new Error(`Unexpected ${owner} entry: ${path} (expected ${expected.get(path) ?? 'none'})`)
    }
  }
  for (const [path, owner] of expected) {
    if (!Object.hasOwn(inventories, owner)) throw new Error(`Missing runner inventory: ${owner} (${path})`)
    if (!seen.has(path)) throw new Error(`Runner omitted ${owner} test: ${path}`)
  }
}
