import { spawnSync } from 'node:child_process'
import { access } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { discoverTestEntries, nodeTestExceptions, selectNodeTests } from '../tests/suiteOwnership.mjs'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
try {
  const files = [
    ...await discoverTestEntries(root, 'tests'),
    ...await discoverTestEntries(root, 'benchmarks'),
  ]
  for (const path of nodeTestExceptions) {
    await access(resolve(root, path))
    if (!files.includes(path)) files.push(path)
  }
  const selected = selectNodeTests(files)
  console.log(`Running ${selected.length} Node test files`)
  const result = spawnSync(process.execPath, ['--test', ...selected], { cwd: root, stdio: 'inherit' })
  if (result.error) throw result.error
  if (result.signal) throw new Error(`Node tests terminated by ${result.signal}`)
  process.exitCode = result.status ?? 1
} catch (error) {
  console.error(error)
  process.exitCode = 1
}
