import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { execFileSync, spawnSync } from 'node:child_process'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { dirname, isAbsolute, relative, resolve } from 'node:path'
import { setTimeout as delay } from 'node:timers/promises'
import { fileURLToPath, pathToFileURL } from 'node:url'

import { compressSync, decompressSync } from 'fflate'
import { Packr, Unpackr } from 'msgpackr/index-no-eval'

const PACKAGE = 'io.github.rsyumi.risunest'
const ACTIVITY = `${PACKAGE}/.MainActivity`
const RISUSAVE_COMPRESSED_HEADER = Buffer.from([0, 82, 73, 83, 85, 83, 65, 86, 69, 0, 8])
const scriptDirectory = dirname(fileURLToPath(import.meta.url))
const repositoryRoot = resolve(scriptDirectory, '..')
const defaultApk = resolve(
    repositoryRoot,
    'src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk',
)
const fixtureDirectory = resolve(
    repositoryRoot,
    'docs/native-app-analysis/evidence/phase-3/step6-inputs',
)
const evidenceRoot = resolve(
    repositoryRoot,
    'docs/native-app-analysis/evidence/phase-3/step6-run',
)
const sourceFixturePath = resolve(repositoryRoot, 'src-tauri/fixtures/persistent-fixture.json')
const packr = new Packr({ useRecords: false })
const unpackr = new Unpackr({ int64AsType: 'number', useRecords: false })
const onePixelPng = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=',
    'base64',
)

function usage() {
    return `Phase 3 Step 6 Android smoke helper

fresh-install cuts the device off the network (airplane mode plus wifi and data off) and
refuses to continue until dumpsys connectivity reports no default network, so no
third-party RisuRealm content can reach a screenshot or a logcat dump. The smoke itself
only uses adb, so nothing needs the network. Run restore-network when the device is done.

Commands:
  generate
      Create and verify deterministic phase3-step6-a.bin and phase3-step6-b.bin.

  fresh-install --serial <adb-serial> [--apk <path>]
      Uninstall only ${PACKAGE}, install the x86_64 debug APK, clear logcat, and push A/B
      into /sdcard/Download/RisuNest-Step6. This never starts an emulator.

  launch --serial <adb-serial>
  home --serial <adb-serial>
  force-stop --serial <adb-serial>

  restore-network --serial <adb-serial>
      Turn airplane mode off and the wifi and data radios back on after a smoke run.

  collect --serial <adb-serial> --label <label>
      Collect logcat, dumpsys, screenshot, private persistent.db, snapshots, and SQLite
      inspection JSON. Force-stop first when a transactionally stable copy is required.

  inspect --db <path>
      Print revision, root marker, character/chat names, and STEP6 message markers.
`
}

function parseArguments(argv) {
    const [command = 'help', ...rest] = argv
    const options = {}
    for (let index = 0; index < rest.length; index += 1) {
        const token = rest[index]
        if (!token.startsWith('--')) throw new Error(`Unexpected argument: ${token}`)
        const value = rest[index + 1]
        if (!value || value.startsWith('--')) throw new Error(`Missing value for ${token}`)
        options[token.slice(2)] = value
        index += 1
    }
    return { command, options }
}

// 이 스모크는 adb만 쓰고 앱도 로컬 persistent store만 검증하므로 네트워크가 전혀
// 필요 없다. 그래서 아예 끊는다. RisuRealm 목록과 카드 이미지가 screencap이나
// logcat을 타고 나올 수 없게 만드는 가장 확실한 지점이다.
//
// 명령의 종료 코드는 믿지 않는다. `svc wifi disable`은 eth0으로 나가는 에뮬레이터
// 이미지에서 0을 돌려주면서도 아무것도 끊지 않는다. 그래서 ConnectivityService가
// 보고하는 기본 네트워크가 사라질 때까지 기다렸다가, 남아 있으면 거부한다.
// `run`과 `sleep`은 테스트에서 adb 없이 돌리기 위한 주입 지점이다.
export async function cutDeviceNetwork(serial, { run = adb, sleep = delay, attempts = 20 } = {}) {
    // 라디오 명령은 기기와 API 레벨에 따라 있고 없고가 달라 실패를 허용한다.
    // 실제 검증은 아래 dumpsys 확인이 한다.
    run(serial, ['shell', 'cmd', 'connectivity', 'airplane-mode', 'enable'], { allowFailure: true })
    run(serial, ['shell', 'svc', 'wifi', 'disable'], { allowFailure: true })
    run(serial, ['shell', 'svc', 'data', 'disable'], { allowFailure: true })

    let activeNetwork
    for (let attempt = 0; attempt < attempts; attempt += 1) {
        activeNetwork = readActiveDefaultNetwork(serial, run)
        if (activeNetwork === 'none') return
        if (attempt + 1 < attempts) await sleep(500)
    }
    if (activeNetwork === undefined) {
        throw new Error(
            'Refusing to run: could not verify that device networking is off because ' +
                'dumpsys connectivity did not report an "Active default network" line',
        )
    }
    throw new Error(
        `Refusing to run: device networking is still reachable (active default network ${activeNetwork}). ` +
            'An emulator image that routes through eth0 is not affected by airplane mode; ' +
            'take that interface down (adb root, ip link set eth0 down) and retry.',
    )
}

// ConnectivityService.dump()가 찍는 "Active default network: <netId|none>" 줄을 읽는다.
function readActiveDefaultNetwork(serial, run) {
    const dump = String(run(serial, ['shell', 'dumpsys', 'connectivity'], { allowFailure: true }).stdout || '')
    const match = dump.match(/^Active default network:\s*(\S+)/m)
    return match ? match[1] : undefined
}

export function restoreDeviceNetwork(serial, { run = adb } = {}) {
    run(serial, ['shell', 'cmd', 'connectivity', 'airplane-mode', 'disable'], { allowFailure: true })
    run(serial, ['shell', 'svc', 'wifi', 'enable'], { allowFailure: true })
    run(serial, ['shell', 'svc', 'data', 'enable'], { allowFailure: true })
}

function sha256(bytes) {
    return createHash('sha256').update(bytes).digest('hex').toUpperCase()
}

function encodeRisuSave(database) {
    const packed = Buffer.from(packr.encode(database))
    return Buffer.concat([
        RISUSAVE_COMPRESSED_HEADER,
        Buffer.from(compressSync(packed, { mtime: 0 })),
    ])
}

function decodeRisuSave(bytes) {
    assert.deepEqual(bytes.subarray(0, RISUSAVE_COMPRESSED_HEADER.length), RISUSAVE_COMPRESSED_HEADER)
    return unpackr.decode(decompressSync(bytes.subarray(RISUSAVE_COMPRESSED_HEADER.length)))
}

function encodeBackup(entries) {
    const chunks = []
    for (const [name, data] of entries) {
        const nameBytes = Buffer.from(name, 'utf8')
        const dataBytes = Buffer.from(data)
        const header = Buffer.allocUnsafe(8)
        header.writeUInt32LE(nameBytes.length, 0)
        header.writeUInt32LE(dataBytes.length, 4)
        chunks.push(header.subarray(0, 4), nameBytes, header.subarray(4), dataBytes)
    }
    return Buffer.concat(chunks)
}

function decodeBackup(bytes) {
    const entries = new Map()
    let offset = 0
    while (offset < bytes.length) {
        assert.ok(offset + 4 <= bytes.length, 'backup name length is truncated')
        const nameLength = bytes.readUInt32LE(offset)
        offset += 4
        assert.ok(offset + nameLength + 4 <= bytes.length, 'backup name is truncated')
        const name = bytes.subarray(offset, offset + nameLength).toString('utf8')
        offset += nameLength
        const dataLength = bytes.readUInt32LE(offset)
        offset += 4
        assert.ok(offset + dataLength <= bytes.length, `backup entry ${name} is truncated`)
        entries.set(name, bytes.subarray(offset, offset + dataLength))
        offset += dataLength
    }
    return entries
}

function makeFixture(source, variant) {
    const database = structuredClone(source)
    const sourceCharacter = database.characters.find((character) => character.chaId === 'char-a')
    assert.ok(sourceCharacter, 'persistent fixture is missing char-a')
    const sourceConversation = sourceCharacter.chats.find((chat) => chat.id === 'conv-short')
    assert.ok(sourceConversation, 'persistent fixture is missing conv-short')

    const character = structuredClone(sourceCharacter)
    character.name = `Phase 3 Step 6 ${variant}`
    character.image = 'char-a.png'
    character.chatPage = 0
    character.chats = [structuredClone(sourceConversation)]
    character.chats[0].name = `Step 6 ${variant} chat`
    character.chats[0].message.at(-1).data = `STEP6-${variant}-SNAPSHOT`

    database.username = `Phase 3 Step 6 ${variant}`
    database.characters = [character]
    return database
}

async function generateFixtures() {
    const source = JSON.parse(await readFile(sourceFixturePath, 'utf8'))
    await mkdir(fixtureDirectory, { recursive: true })
    const manifest = {
        source: relative(repositoryRoot, sourceFixturePath).replaceAll('\\', '/'),
        expectedEditedMarker: 'STEP6-B-AFTER-EDIT',
        fixtures: {},
    }

    for (const variant of ['A', 'B']) {
        const database = makeFixture(source, variant)
        const databaseBytes = encodeRisuSave(database)
        const backupBytes = encodeBackup([
            ['char-a.png', onePixelPng],
            ['database.risudat', databaseBytes],
        ])
        const fileName = `phase3-step6-${variant.toLowerCase()}.bin`
        const outputPath = resolve(fixtureDirectory, fileName)
        await writeFile(outputPath, backupBytes)

        const decodedEntries = decodeBackup(await readFile(outputPath))
        assert.deepEqual(decodedEntries.get('char-a.png'), onePixelPng)
        const decodedDatabase = decodeRisuSave(decodedEntries.get('database.risudat'))
        assert.equal(decodedDatabase.username, database.username)
        assert.equal(decodedDatabase.characters[0].name, database.characters[0].name)
        assert.equal(
            decodedDatabase.characters[0].chats[0].message.at(-1).data,
            `STEP6-${variant}-SNAPSHOT`,
        )

        manifest.fixtures[variant] = {
            file: fileName,
            bytes: backupBytes.length,
            sha256: sha256(backupBytes),
            username: database.username,
            character: database.characters[0].name,
            conversation: database.characters[0].chats[0].name,
            marker: `STEP6-${variant}-SNAPSHOT`,
        }
    }

    const manifestPath = resolve(fixtureDirectory, 'manifest.json')
    await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`)
    process.stdout.write(`${JSON.stringify({ fixtureDirectory, manifest }, null, 2)}\n`)
}

function resolveAdb() {
    const executable = process.platform === 'win32' ? 'adb.exe' : 'adb'
    const sdkRoot = process.env.ANDROID_SDK_ROOT || process.env.ANDROID_HOME
    return sdkRoot ? resolve(sdkRoot, 'platform-tools', executable) : executable
}

function adb(serial, args, options = {}) {
    const result = spawnSync(resolveAdb(), ['-s', serial, ...args], {
        cwd: repositoryRoot,
        encoding: options.binary ? null : 'utf8',
        maxBuffer: 256 * 1024 * 1024,
    })
    if (result.error) throw result.error
    if (result.status !== 0 && !options.allowFailure) {
        throw new Error(
            `adb ${args.join(' ')} failed (${result.status}):\n${String(result.stderr || result.stdout)}`,
        )
    }
    return result
}

function requireSerial(options) {
    const serial = options.serial
    if (!serial) throw new Error('--serial is required')
    const devices = execFileSync(resolveAdb(), ['devices'], { encoding: 'utf8' })
    const connected = devices
        .split(/\r?\n/)
        .some((line) => line.startsWith(`${serial}\tdevice`))
    if (!connected) throw new Error(`ADB device is not ready: ${serial}`)
    return serial
}

function requireRepositoryFile(path) {
    const absolute = resolve(path)
    const relation = relative(repositoryRoot, absolute)
    if (relation.startsWith('..') || isAbsolute(relation)) {
        throw new Error(`Path must remain inside the repository: ${absolute}`)
    }
    return absolute
}

async function freshInstall(options) {
    await generateFixtures()
    const serial = requireSerial(options)
    const apk = requireRepositoryFile(options.apk || defaultApk)
    await readFile(apk)

    const installedBefore = String(
        adb(serial, ['shell', 'pm', 'path', PACKAGE], { allowFailure: true }).stdout || '',
    ).trim()
    if (installedBefore) {
        const uninstall = adb(serial, ['uninstall', PACKAGE])
        assert.match(String(uninstall.stdout), /Success/)
    }
    const installedAfter = String(
        adb(serial, ['shell', 'pm', 'path', PACKAGE], { allowFailure: true }).stdout || '',
    ).trim()
    assert.equal(installedAfter, '', `Fresh uninstall left ${PACKAGE} installed`)

    const install = adb(serial, ['install', '-r', '-t', apk])
    assert.match(String(install.stdout), /Success/)
    await cutDeviceNetwork(serial)
    adb(serial, ['shell', 'mkdir', '-p', '/sdcard/Download/RisuNest-Step6'])
    for (const variant of ['a', 'b']) {
        const source = resolve(fixtureDirectory, `phase3-step6-${variant}.bin`)
        adb(serial, ['push', source, `/sdcard/Download/RisuNest-Step6/phase3-step6-${variant}.bin`])
    }
    adb(serial, ['logcat', '-c'])

    process.stdout.write(
        `${JSON.stringify({ serial, package: PACKAGE, apk, remoteFixtures: '/sdcard/Download/RisuNest-Step6' }, null, 2)}\n`,
    )
}

function lifecycleCommand(command, options) {
    const serial = requireSerial(options)
    if (command === 'launch') {
        process.stdout.write(adb(serial, ['shell', 'am', 'start', '-W', '-n', ACTIVITY]).stdout)
    } else if (command === 'home') {
        process.stdout.write(adb(serial, ['shell', 'input', 'keyevent', 'KEYCODE_HOME']).stdout)
    } else if (command === 'force-stop') {
        process.stdout.write(adb(serial, ['shell', 'am', 'force-stop', PACKAGE]).stdout)
    }
}

async function inspectDatabase(databasePath) {
    const { DatabaseSync } = await import('node:sqlite')
    const database = new DatabaseSync(databasePath, { readOnly: true })
    try {
        const meta = Object.fromEntries(
            database.prepare('SELECT key, value FROM meta ORDER BY key').all().map((row) => [row.key, JSON.parse(row.value)]),
        )
        const generation = meta.activeGeneration
        const rootRow = database.prepare('SELECT value FROM root WHERE generation = ?').get(generation)
        const root = rootRow ? JSON.parse(rootRow.value) : null
        const characters = database
            .prepare('SELECT character_id, name, conversation_count FROM characters WHERE generation = ? ORDER BY configured_index')
            .all(generation)
        const conversations = database
            .prepare('SELECT character_id, conversation_id, name, message_count FROM conversations WHERE generation = ? ORDER BY character_id, configured_index')
            .all(generation)
        const messageRows = database
            .prepare('SELECT character_id, conversation_id, message_index, value FROM messages WHERE generation = ? ORDER BY character_id, conversation_id, message_index')
            .all(generation)
        const markers = messageRows
            .map((row) => ({ ...row, value: JSON.parse(row.value) }))
            .filter((row) => String(row.value.data || '').includes('STEP6'))
            .map((row) => ({
                characterId: row.character_id,
                conversationId: row.conversation_id,
                messageIndex: Number(row.message_index),
                marker: row.value.data,
            }))
        return {
            databasePath,
            sha256: sha256(await readFile(databasePath)),
            userVersion: database.prepare('PRAGMA user_version').get().user_version,
            revision: meta.currentRevision,
            generation,
            username: root?.username ?? null,
            characters,
            conversations,
            markers,
        }
    } finally {
        database.close()
    }
}

function safeLabel(value) {
    if (!value || !/^[a-zA-Z0-9._-]+$/.test(value)) {
        throw new Error('--label must contain only letters, numbers, dot, underscore, or hyphen')
    }
    return value
}

async function copyPrivateFile(serial, remotePath, outputPath) {
    const result = adb(serial, ['exec-out', 'run-as', PACKAGE, 'cat', remotePath], { binary: true })
    await writeFile(outputPath, result.stdout)
}

async function copyPrivateFileIfPresent(serial, remotePath, outputPath) {
    const result = adb(serial, ['exec-out', 'run-as', PACKAGE, 'cat', remotePath], {
        binary: true,
        allowFailure: true,
    })
    if (result.status !== 0 || result.stdout.length === 0) return false
    await writeFile(outputPath, result.stdout)
    return true
}

async function collectEvidence(options) {
    const serial = requireSerial(options)
    const label = safeLabel(options.label)
    const stamp = new Date().toISOString().replaceAll(':', '').replaceAll('.', '')
    const outputDirectory = resolve(evidenceRoot, `${stamp}-${label}`)
    await mkdir(outputDirectory, { recursive: true })

    const textCommands = {
        'device.txt': ['shell', 'getprop'],
        'webview.txt': ['shell', 'dumpsys', 'webviewupdate'],
        'package.txt': ['shell', 'dumpsys', 'package', PACKAGE],
        'activity.txt': ['shell', 'dumpsys', 'activity', 'activities'],
        'private-files.txt': ['shell', 'run-as', PACKAGE, 'find', 'files', '-maxdepth', '6', '-type', 'f'],
        'persistent-listing.txt': ['shell', 'run-as', PACKAGE, 'ls', '-laR', 'files/persistent'],
        'logcat.txt': ['logcat', '-d', '-v', 'threadtime'],
    }
    for (const [fileName, args] of Object.entries(textCommands)) {
        const result = adb(serial, args, { allowFailure: true })
        await writeFile(resolve(outputDirectory, fileName), `${result.stdout || ''}${result.stderr || ''}`)
    }

    const screenshot = adb(serial, ['exec-out', 'screencap', '-p'], { binary: true, allowFailure: true })
    if (screenshot.status === 0) await writeFile(resolve(outputDirectory, 'screen.png'), screenshot.stdout)

    const databasePath = resolve(outputDirectory, 'persistent.db')
    await copyPrivateFile(serial, 'files/persistent/persistent.db', databasePath)
    // The store runs in WAL mode, so commits since the last truncate checkpoint
    // live only in the -wal sidecar; without it the inspection under-reports.
    await copyPrivateFileIfPresent(serial, 'files/persistent/persistent.db-wal', `${databasePath}-wal`)
    const inspections = { active: await inspectDatabase(databasePath), snapshots: [] }

    const snapshotFiles = String(
        adb(serial, ['shell', 'run-as', PACKAGE, 'find', 'files/persistent/snapshots', '-maxdepth', '1', '-type', 'f'], {
            allowFailure: true,
        }).stdout || '',
    )
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter((line) => line.endsWith('.db'))
    for (const remotePath of snapshotFiles) {
        const outputPath = resolve(outputDirectory, remotePath.split('/').at(-1))
        await copyPrivateFile(serial, remotePath, outputPath)
        inspections.snapshots.push(await inspectDatabase(outputPath))
    }

    const inspectionPath = resolve(outputDirectory, 'sqlite-inspection.json')
    await writeFile(inspectionPath, `${JSON.stringify(inspections, null, 2)}\n`)
    process.stdout.write(`${JSON.stringify({ outputDirectory, inspections }, null, 2)}\n`)
}

async function main() {
    const { command, options } = parseArguments(process.argv.slice(2))
    if (command === 'help' || command === '--help') {
        process.stdout.write(usage())
        return
    }
    if (command === 'generate') {
        await generateFixtures()
    } else if (command === 'fresh-install') {
        await freshInstall(options)
    } else if (['launch', 'home', 'force-stop'].includes(command)) {
        lifecycleCommand(command, options)
    } else if (command === 'restore-network') {
        restoreDeviceNetwork(requireSerial(options))
    } else if (command === 'collect') {
        await collectEvidence(options)
    } else if (command === 'inspect') {
        if (!options.db) throw new Error('--db is required')
        process.stdout.write(`${JSON.stringify(await inspectDatabase(requireRepositoryFile(options.db)), null, 2)}\n`)
    } else {
        throw new Error(`Unknown command: ${command}\n\n${usage()}`)
    }
}

// 테스트가 cutDeviceNetwork를 import할 수 있도록, 직접 실행됐을 때만 main을 돌린다.
const invokedDirectly =
    Boolean(process.argv[1]) && import.meta.url === pathToFileURL(resolve(process.argv[1])).href
if (invokedDirectly) {
    main().catch((error) => {
        process.stderr.write(`${error instanceof Error ? error.stack : String(error)}\n`)
        process.exitCode = 1
    })
}
