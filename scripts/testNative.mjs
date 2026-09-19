import { spawnSync } from 'node:child_process'
import { basename, dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

export const protocolManifests = [
  'crates/sync-wire/Cargo.toml',
  'crates/sync-connect/Cargo.toml',
  'crates/external-storage-format/Cargo.toml',
  'crates/release-update/Cargo.toml',
]
export const hostManifests = [
  'src-tauri/Cargo.toml',
  'server/sync/Cargo.toml',
  'server/manager/Cargo.toml',
  'server/manager/gui/src-tauri/Cargo.toml',
]

export function sharedCargoTarget(commonGitDirectory, configuredTarget, root) {
  const common = resolve(root, commonGitDirectory.trim())
  if (basename(common) !== '.git') throw new Error('Cannot locate the main checkout shared Cargo target')
  const target = join(dirname(common), 'src-tauri', 'target')
  if (configuredTarget && resolve(root, configuredTarget) !== target) {
    throw new Error(`CARGO_TARGET_DIR must use the main checkout cache: ${target}`)
  }
  return target
}

export function nativeCommands(group, platform = process.platform) {
  if (!['protocol', 'host'].includes(group)) throw new Error('Expected protocol or host')
  const commands = []
  for (const manifest of group === 'protocol' ? protocolManifests : hostManifests) {
    if (manifest === 'src-tauri/Cargo.toml' && platform === 'linux') {
      commands.push(['dbus-run-session', '--', 'bash', 'scripts/linux-native-tests.sh', '--release'])
    } else {
      commands.push(['cargo', 'test', '--manifest-path', manifest, '--release', '--locked'])
    }
  }
  if (group === 'host') commands.push([
    'cargo', 'test', '--manifest-path', 'src-tauri/Cargo.toml',
    '--package', 'tauri-plugin-updater', '--release', '--locked', '--lib',
  ])
  return commands
}

export function runNative(group) {
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
  const git = spawnSync('git', ['rev-parse', '--path-format=absolute', '--git-common-dir'], { cwd: root, encoding: 'utf8' })
  if (git.error || git.status !== 0) throw git.error ?? new Error(git.stderr)
  const env = { ...process.env, CARGO_TARGET_DIR: sharedCargoTarget(git.stdout, process.env.CARGO_TARGET_DIR, root) }
  for (const [command, ...args] of nativeCommands(group)) {
    console.log(`> ${command} ${args.join(' ')}`)
    const result = spawnSync(command, args, { cwd: root, env, stdio: 'inherit' })
    if (result.error) throw result.error
    if (result.status !== 0) throw new Error(`${command} failed: ${result.signal ?? result.status}`)
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try {
    if (process.argv[2] === '--help') {
      console.log('Usage: node scripts/testNative.mjs protocol|host\nRequires the checked-in Rust toolchain and native build dependencies. Linux host tests require dbus-run-session and gnome-keyring-daemon. Uses only the main checkout src-tauri/target; never installs toolchains or skips missing prerequisites.')
    } else {
      if (process.argv.length !== 3) throw new Error('Usage: node scripts/testNative.mjs protocol|host')
      runNative(process.argv[2])
    }
  } catch (error) {
    console.error(error)
    process.exitCode = 1
  }
}
