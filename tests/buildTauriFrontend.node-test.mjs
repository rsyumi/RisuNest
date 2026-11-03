import assert from 'node:assert/strict'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import test from 'node:test'
import { syncOutput } from '../scripts/buildTauriFrontend.mjs'

const past = new Date('2020-01-01T00:00:00Z')

function tree(root, files) {
  for (const [path, contents] of Object.entries(files)) {
    const file = join(root, path)
    mkdirSync(dirname(file), { recursive: true })
    writeFileSync(file, contents)
    utimesSync(file, past, past)
  }
}

function withDirectories(run) {
  const root = mkdtempSync(join(tmpdir(), 'risunest-frontend-sync-'))
  try {
    run(join(root, 'staging'), join(root, 'dist'))
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
}

const modified = path => statSync(path).mtimeMs

test('a rebuilt frontend replaces changed, added and removed files and leaves identical ones untouched', () => withDirectories((staging, dist) => {
  tree(dist, { 'index.html': 'page', 'assets/kept.js': 'kept', 'assets/old.js': 'old', 'icon.png': 'icon 1' })
  // Vite copies public/ files with their source time on Windows.
  tree(staging, { 'index.html': 'page', 'assets/kept.js': 'kept', 'assets/new.js': 'new', 'icon.png': 'icon 2' })
  syncOutput(staging, dist)
  assert.equal(modified(join(dist, 'index.html')), past.getTime())
  assert.equal(modified(join(dist, 'assets/kept.js')), past.getTime())
  assert.equal(existsSync(join(dist, 'assets/old.js')), false)
  assert.equal(readFileSync(join(dist, 'assets/new.js'), 'utf8'), 'new')
  assert.equal(readFileSync(join(dist, 'icon.png'), 'utf8'), 'icon 2')
  assert.ok(modified(join(dist, 'icon.png')) > past.getTime())

  const paths = ['', 'assets', 'index.html', 'assets/kept.js', 'assets/new.js', 'icon.png'].map(path => join(dist, path))
  const times = paths.map(modified)
  syncOutput(staging, dist)
  assert.deepEqual(paths.map(modified), times)
}))

test('a source map that differs only in Rolldown file references keeps the existing copy', () => withDirectories((staging, dist) => {
  const map = id => JSON.stringify({ sources: ['../../src/etc/bg.jpg'], sourcesContent: [`export default import.meta.ROLLDOWN_FILE_URL_${id}`] })
  tree(dist, { 'assets/a.js.map': map('Ngmgj7KrmSrUoXH_NFNtFA'), 'assets/b.js.map': map('x'), 'assets/a.js': 'import.meta.ROLLDOWN_FILE_URL_x' })
  tree(staging, {
    'assets/a.js.map': map('8HSUfh7cJHNFxCkInNzNXw'),
    'assets/b.js.map': map('x').replace('bg.jpg', 'bg.webp'),
    'assets/a.js': 'import.meta.ROLLDOWN_FILE_URL_y',
  })
  syncOutput(staging, dist)
  assert.equal(readFileSync(join(dist, 'assets/a.js.map'), 'utf8'), map('Ngmgj7KrmSrUoXH_NFNtFA'))
  assert.equal(modified(join(dist, 'assets/a.js.map')), past.getTime())
  assert.match(readFileSync(join(dist, 'assets/b.js.map'), 'utf8'), /bg\.webp/)
  assert.equal(readFileSync(join(dist, 'assets/a.js'), 'utf8'), 'import.meta.ROLLDOWN_FILE_URL_y')
}))
