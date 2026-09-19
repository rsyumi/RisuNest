import assert from 'node:assert/strict'
import { test } from 'node:test'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { hostManifests, nativeCommands, protocolManifests, sharedCargoTarget } from '../scripts/testNative.mjs'

test('native groups explicitly own dependency packages and full app integration targets', () => {
  const protocol = nativeCommands('protocol', 'win32')
  assert.deepEqual(protocol.map(command => command[3]), protocolManifests)
  assert.equal(protocol.length, 4)
  const host = nativeCommands('host', 'win32')
  assert.deepEqual(host.slice(0, 4).map(command => command[3]), hostManifests)
  for (const command of [...protocol, ...host]) {
    assert(command.includes('--locked'))
    assert(command.includes('--release'))
    assert(!command.includes('--ignored'))
  }
  assert(!host[0].includes('--lib'))
  assert(host.at(-1).includes('tauri-plugin-updater'))
  assert(host.at(-1).includes('--lib'))
  assert.deepEqual(nativeCommands('host', 'linux')[0], ['dbus-run-session', '--', 'bash', 'scripts/linux-native-tests.sh', '--release'])
  assert.throws(() => nativeCommands('everything'), /Expected/)
})

test('main checkout and worktrees share one target and reject an unrelated cache', () => {
  const main = join(tmpdir(), 'risunest-main')
  const worktree = join(tmpdir(), 'risunest-worktree')
  const target = join(main, 'src-tauri', 'target')
  assert.equal(sharedCargoTarget(join(main, '.git'), undefined, main), target)
  assert.equal(sharedCargoTarget(join(main, '.git'), target, worktree), target)
  assert.throws(() => sharedCargoTarget(join(main, '.git'), join(worktree, 'target'), worktree), /main checkout cache/)
  assert.throws(() => sharedCargoTarget(join(main, 'unknown'), undefined, worktree), /Cannot locate/)
})
