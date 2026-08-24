import { mkdir, writeFile } from 'node:fs/promises'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { fixtureDatabase } from '../src/ts/storage/tests/persistentDataFixtures.ts'

const scriptDirectory = dirname(fileURLToPath(import.meta.url))
const repositoryRoot = resolve(scriptDirectory, '..')
const fixturePath = resolve(repositoryRoot, 'src-tauri', 'fixtures', 'persistent-fixture.json')

await mkdir(dirname(fixturePath), { recursive: true })
await writeFile(fixturePath, `${JSON.stringify(fixtureDatabase, null, 2)}\n`)
