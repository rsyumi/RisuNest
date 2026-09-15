import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { openSync, closeSync } from 'node:fs'
import path from 'node:path'
import { connectVerifiedIdentity, delay, waitForInteractive } from './cdp.mjs'
import { addSyntheticAssets, seedExpression } from './fixture.mjs'
import { sanitizeMetrics } from './metrics.mjs'

const options = Object.fromEntries(
    process.argv.slice(2).map((arg) => arg.replace(/^--/, '').split('=')),
)
const adb = options.adb
const serial = options.serial
const packageName = 'io.github.rsyumi.risunest'
const port = 19367
const directory = path.resolve(options.directory ?? '.tmp/external-storage-m0/android')
const runs = Number(options.runs ?? 20)
const kinds = options.kind ? [options.kind] : ['reload', 'restart']
const apk = path.resolve(options.apk ?? '.tmp/external-storage-m0/android-agent.apk')
let phase = 'preflight'
let client
let provisioned = false
let previousStayAwake

function command(args, extra = {}) {
    const result = spawnSync(adb, ['-s', serial, ...args], {
        encoding: 'utf8',
        windowsHide: true,
        timeout: 30_000,
        maxBuffer: 1024 * 1024,
        ...extra,
    })
    if (result.status !== 0) throw new Error('Android benchmark command failed')
    return result.stdout?.trim() ?? ''
}

function verifyInstalledApk(expectedHash) {
    const installed = command(['shell', 'pm', 'path', packageName]).replace(/^package:/, '')
    if (
        !/^\/data\/app\/[A-Za-z0-9._=/-]+\/base\.apk$/.test(installed) ||
        !installed.includes(packageName)
    )
        throw new Error('Unexpected installed APK path')
    const actual = command(['shell', 'sha256sum', installed]).split(/\s+/)[0]
    if (actual !== expectedHash) throw new Error('Installed APK mismatch')
}

// Only created after proving this installation did not previously exist.
async function requireOwnership() {
    const record = JSON.parse(await readFile(path.join(directory, 'ownership.json'), 'utf8'))
    if (
        record.serial !== serial ||
        record.packageName !== packageName ||
        record.freshInstall !== true
    )
        throw new Error('Unowned Android installation')
    if (
        command(['shell', 'run-as', packageName, 'cat', 'files/risunest-m0-owner']) !==
        record.marker
    )
        throw new Error('Android ownership marker mismatch')
    return record
}

async function launch() {
    await requireOwnership()
    command(['shell', 'input', 'keyevent', 'KEYCODE_WAKEUP'])
    command(['shell', 'wm', 'dismiss-keyguard'])
    command(['shell', 'am', 'start', '-n', `${packageName}/.MainActivity`])
    let pid
    for (let i = 0; i < 30; i++) {
        try {
            pid = command(['shell', 'pidof', packageName])
            if (/^\d+$/.test(pid)) break
        } catch {}
        await delay(500)
    }
    if (!/^\d+$/.test(pid)) throw new Error('Android process unavailable')
    command(['forward', `tcp:${port}`, `localabstract:webview_devtools_remote_${pid}`])
    client = await connectVerifiedIdentity(port, packageName)
    if (!(await client.evaluate("document.visibilityState === 'visible'")))
        throw new Error('Android app is hidden; unlock the device before measuring')
    await waitForInteractive(client)
}

function walBytes() {
    command(['shell', 'run-as', packageName, 'sh', '-c', '"test -f persistent/persistent.sqlite"'])
    const value = command([
        'shell',
        'run-as',
        packageName,
        'sh',
        '-c',
        '"if [ -f persistent/persistent.sqlite-wal ]; then stat -c %s persistent/persistent.sqlite-wal; else echo 0; fi"',
    ])
    const bytes = Number(value)
    if (!Number.isSafeInteger(bytes) || bytes < 0) throw new Error('Invalid WAL size')
    return bytes
}

function memory() {
    // Return only numeric process counters, never dumpsys output or command lines.
    const dump = command(['shell', 'dumpsys', 'meminfo', packageName])
    const total = dump.match(/^\s*TOTAL\s+(\d+)\s+/m)
    const rss = dump.match(/TOTAL RSS:\s*(\d+)/)
    const status = command([
        'shell',
        'run-as',
        packageName,
        'cat',
        `/proc/${command(['shell', 'pidof', packageName])}/status`,
    ])
    const nativeRss = status.match(/^VmRSS:\s+(\d+)\s+kB/m)
    if (!total || !nativeRss) throw new Error('Android memory counters unavailable')
    return {
        appPssBytes: Number(total[1]) * 1024,
        appRssBytes: Number(nativeRss[1]) * 1024,
        dumpsysRssBytes: rss ? Number(rss[1]) * 1024 : null,
    }
}

async function main() {
    if (kinds.some((kind) => !['reload', 'restart'].includes(kind)))
        throw new Error('Invalid measurement kind')
    if (!/^[0-9a-f]{40}$/.test(options.revision ?? ''))
        throw new Error('Explicit build revision required')
    if (!adb || !/^[a-zA-Z0-9]+$/.test(serial ?? '') || !Number.isSafeInteger(runs) || runs < 1)
        throw new Error('Explicit device and positive runs required')
    await mkdir(directory, { recursive: true })
    if (options.provision === 'true') {
        if (
            command(['shell', 'pm', 'list', 'packages', packageName])
                .split('\n')
                .includes(`package:${packageName}`)
        )
            throw new Error('Refusing existing Android installation')
        const apkSha256 = createHash('sha256')
            .update(await readFile(apk))
            .digest('hex')
        try {
            command(['install', apk], { timeout: 180_000 })
        } catch {
            // A timed-out install can have committed. Reconcile bytes before ownership.
            verifyInstalledApk(apkSha256)
        }
        verifyInstalledApk(apkSha256)
        command(['shell', 'run-as', packageName, 'mkdir', '-p', 'files'])
        const marker = createHash('sha256')
            .update(`${serial}:${apkSha256}:${Date.now()}`)
            .digest('hex')
        const markerFile = path.join(directory, 'owner.txt')
        await writeFile(markerFile, marker)
        command(['push', markerFile, '/data/local/tmp/risunest-m0-owner'])
        command([
            'shell',
            'run-as',
            packageName,
            'cp',
            '/data/local/tmp/risunest-m0-owner',
            'files/risunest-m0-owner',
        ])
        command(['shell', 'rm', '/data/local/tmp/risunest-m0-owner'])
        await writeFile(
            path.join(directory, 'ownership.json'),
            JSON.stringify({ serial, packageName, freshInstall: true, apkSha256, marker }),
        )
        provisioned = true
    }
    const ownership = await requireOwnership()
    verifyInstalledApk(ownership.apkSha256)
    previousStayAwake = command(['shell', 'settings', 'get', 'global', 'stay_on_while_plugged_in'])
    if (!/^\d+$/.test(previousStayAwake)) throw new Error('Invalid power setting')
    command(['shell', 'svc', 'power', 'stayon', 'usb'])
    if (options.prepare === 'true' || options.resumeSeed === 'true')
        command(['shell', 'am', 'force-stop', packageName])
    phase = 'launch'
    console.log(JSON.stringify({ phase }))
    await launch()
    if (provisioned || options.prepare === 'true' || options.resumeSeed === 'true') {
        phase = 'seed'
        console.log(JSON.stringify({ phase }))
        let seeded
        if (options.resumeSeed === 'true') {
            const saved = JSON.parse(await readFile(path.join(directory, 'seed.json'), 'utf8'))
            if (saved.marker !== ownership.marker) throw new Error('Seed owner mismatch')
            seeded = saved.seeded
        } else {
            const seed = seedExpression({
                characters: 500,
                bytes: 104857600,
                images: true,
                compatibility: true,
            })
                .replace(
                    'let step = 0;',
                    `let step = 0;
                Object.defineProperty(window, '__m0SeedStep', {get: () => step, configurable: true});
                window.__m0SeedCount = 0;`,
                )
                .replace(
                    "await invoke('pds_replace_add_characters', {stagingId, characters: [character]});",
                    "await invoke('pds_replace_add_characters', {stagingId, characters: [character]}); window.__m0SeedCount = i + 1;",
                )
            await client.evaluate(
                `void (${seed}).then(result => {window.__m0SeedResult = result;})`,
            )
            const seedDeadline = Date.now() + 600_000
            while (Date.now() < seedDeadline) {
                await delay(15000)
                const progress = await client.evaluate(`({step:window.__m0SeedStep,
                characters:window.__m0SeedCount,result:window.__m0SeedResult??null})`)
                console.log(JSON.stringify({ phase: 'seed-progress', ...progress }))
                if (progress.result) {
                    seeded = progress.result
                    break
                }
            }
        }
        if (!seeded) throw new Error('Synthetic seed timed out')
        console.log(JSON.stringify({ phase: 'seed-result', ...seeded }))
        if (!seeded.success) throw new Error('Android synthetic seed failed')
        await writeFile(
            path.join(directory, 'seed.json'),
            JSON.stringify({ marker: ownership.marker, seeded }),
        )
        await client.evaluate(
            "window.__TAURI_INTERNALS__.invoke('pds_checkpoint', {mode:'truncate'})",
        )
        client.close()
        client = null
        command(['shell', 'am', 'force-stop', packageName])
        command(['shell', 'run-as', packageName, 'sh', '-c', '"test -f persistent/persistent.sqlite"'])
        const identifier = 'RisuNest.phase3benchmark.r000000000000.androidm0'
        const root = path.join(directory, identifier)
        await mkdir(root, { recursive: true })
        const archive = path.join(directory, 'persistent.tar')
        const fd = openSync(archive, 'w')
        try {
            command(
                ['exec-out', 'run-as', packageName, 'tar', '-C', '.', '-cf', '-', 'persistent'],
                { stdio: ['ignore', fd, 'pipe'], timeout: 180_000 },
            )
        } finally {
            closeSync(fd)
        }
        const unpack = spawnSync('tar', ['-xf', archive, '-C', root], {
            timeout: 120_000,
            windowsHide: true,
        })
        if (unpack.status !== 0) throw new Error('Synthetic archive extraction failed')
        phase = 'assets'
        console.log(JSON.stringify({ phase }))
        const fixture = await addSyntheticAssets(root, identifier, 100000, true)
        const assembled = path.join(directory, 'fixture.tar')
        const uid = command(['shell', 'run-as', packageName, 'id', '-u'])
        const gid = command(['shell', 'run-as', packageName, 'id', '-g'])
        if (!/^\d+$/.test(uid) || !/^\d+$/.test(gid)) throw new Error('Invalid app archive owner')
        const pack = spawnSync(
            'tar',
            ['--uid', uid, '--gid', gid, '-cf', assembled, '-C', root, 'persistent', 'assets-v2'],
            {
                timeout: 180_000,
                windowsHide: true,
            },
        )
        if (pack.status !== 0) throw new Error('Synthetic archive creation failed')
        command(['push', assembled, '/data/local/tmp/risunest-m0-fixture.tar'], {
            timeout: 180_000,
        })
        command(
            [
                'shell',
                'run-as',
                packageName,
                'tar',
                '-xf',
                '/data/local/tmp/risunest-m0-fixture.tar',
                '-C',
                '.',
            ],
            { timeout: 180_000 },
        )
        command(['shell', 'rm', '/data/local/tmp/risunest-m0-fixture.tar'])
        await writeFile(
            path.join(directory, 'fixture.json'),
            JSON.stringify({ ...fixture, ...seeded, characters: 500, targetBytes: 104857600 }),
        )
        await launch()
        await delay(15000)
        await client.evaluate("localStorage.setItem('startupInteract', 'true')")
        phase = 'snapshot'
        const snapshotStarted = Date.now()
        await client.evaluate(
            `window.__startupSnapshotState = 'pending';
            void window.__TAURI_INTERNALS__.invoke('pds_snapshot_create', {reason:'manual'}).then(
                () => { window.__startupSnapshotState = 'ready'; },
                () => { window.__startupSnapshotState = 'failed'; }
            );`,
        )
        let snapshotReady = false
        while (Date.now() - snapshotStarted < 600_000) {
            await delay(15000)
            const status = await client.evaluate('window.__startupSnapshotState')
            console.log(JSON.stringify({ phase, status, elapsedMs: Date.now() - snapshotStarted }))
            if (status === 'failed') throw new Error('Synthetic snapshot preparation failed')
            if (status === 'ready') {
                snapshotReady = true
                break
            }
        }
        if (!snapshotReady) throw new Error('Synthetic snapshot preparation timed out')
        if (options.prepareOnly === 'true') return
    }
    const samples = []
    const persist = async () =>
        writeFile(
            path.join(directory, 'result.json'),
            JSON.stringify(
                {
                    platform: 'android-arm64',
                    build: 'debug-agent',
                    revision: options.revision,
                    binarySha256: ownership.apkSha256,
                    fixture: JSON.parse(
                        await readFile(path.join(directory, 'fixture.json'), 'utf8'),
                    ),
                    samples,
                },
                null,
                2,
            ),
        )
    const measure = async (kind, warmup = false) => {
        phase = kind
        const startBytes = walBytes()
        if (kind === 'restart') {
            client?.close()
            client = null
            command(['shell', 'am', 'force-stop', packageName])
            await launch()
        } else {
            const origin = await client.evaluate('performance.timeOrigin')
            await client.call('Page.reload')
            for (let i = 0; i < 1200; i++) {
                if ((await client.evaluate('performance.timeOrigin')) !== origin) break
                if (i === 1199) throw new Error('Android reload timeout')
                await delay(100)
            }
            await waitForInteractive(client)
        }
        await delay(15000)
        const raw = await client.evaluate(`({
            ...window.__startupMetrics,
            interactiveMs:performance.getEntriesByName('boot:interactive')[0]?.startTime,
            usedHeapBytes:performance.memory?.usedJSHeapSize,
            documentVisible:document.visibilityState==='visible',
            stabilizationTimeout:!window.__startupMetrics?.operations.some(o=>o.stage==='flush')||Object.values(window.__startupMetrics?.active??{}).some(n=>n>0)
        })`)
        const sample = sanitizeMetrics(raw)
        phase = 'host-metrics'
        const host = { wal: { startBytes, endBytes: walBytes() }, memory: memory() }
        samples.push({ kind, warmup, ...sample, host })
        await persist()
        console.log(
            JSON.stringify({
                kind,
                warmup,
                success: sample.interaction?.success,
                settled: !sample.stabilizationTimeout,
            }),
        )
        if (!sample.interaction?.success || sample.stabilizationTimeout)
            throw new Error('Android baseline sample failed')
    }
    for (const kind of kinds) {
        await measure(kind, true)
        for (let i = 0; i < runs; i++) await measure(kind)
    }
}

try {
    await main()
} catch (error) {
    console.error(
        JSON.stringify({
            success: false,
            phase,
            timeout: String(error?.message).includes('timed out'),
            pageFailure: String(error?.message).includes('page evaluation'),
            adbFailure: String(error?.message).includes('Android benchmark command'),
            cdpConnectionFailure: error?.message === 'CDP connection failed',
            cdpCommandFailure: error?.message === 'CDP command failed',
            hidden: error?.message === 'Android app is hidden; unlock the device before measuring',
        }),
    )
    process.exitCode = 1
} finally {
    client?.close()
    if (previousStayAwake !== undefined) {
        try {
            command([
                'shell',
                'settings',
                'put',
                'global',
                'stay_on_while_plugged_in',
                previousStayAwake,
            ])
        } catch {}
    }
    if (adb && serial) {
        try {
            command(['forward', '--remove', `tcp:${port}`])
        } catch {}
    }
}
