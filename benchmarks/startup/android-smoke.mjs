import { spawnSync } from 'node:child_process'
import { writeFile } from 'node:fs/promises'
import path from 'node:path'
import { cutDeviceNetwork } from '../../scripts/phase3AndroidSmoke.mjs'
import { connectSyntheticAndroid, delay, instrumentation, waitForInteractive } from './cdp.mjs'
import { seedExpression } from './fixture.mjs'
import { sanitizeMetrics } from './metrics.mjs'

const options = Object.fromEntries(
    process.argv.slice(2).map((arg) => arg.replace(/^--/, '').split('=')),
)
const adb = options.adb
const serial = 'emulator-5580'
const packageName = 'io.github.rsyumi.risunest'
const port = 19366
const output = options.output ?? 'benchmarks/startup/android-result.local.json'
const apk = path.resolve(
    options.apk ??
        'src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk',
)
let phase = 'identity'

function run(target, args, { allowFailure = false } = {}) {
    const result = spawnSync(adb, ['-s', target, ...args], {
        encoding: 'utf8',
        windowsHide: true,
        timeout: args[0] === 'install' ? 120_000 : 15_000,
    })
    if (!allowFailure && result.status !== 0) throw new Error('Synthetic Android command failed')
    return result
}

// Android's native invoke transport is not the Windows fetch IPC transport.
// Observe only the allowlisted command names, preserving the original promise.
const nativeObservation = `(() => {
    const native = window.__TAURI_INTERNALS__;
    if (!native?.invoke) return;
    const original = native.invoke;
    native.invoke = function(command, args, options) {
        const promise = original.call(this, command, args, options);
        if (!['pds_open','pds_commit','pds_replace_commit'].includes(command)) return promise;
        const entry = {command, start: performance.now(), ms: null, success: false, bytes: null};
        window.__startupMetrics.calls.push(entry);
        promise.then(result => {
            entry.ms = performance.now() - entry.start; entry.success = true;
            if (command === 'pds_open') {
                entry.bytes = new TextEncoder().encode(JSON.stringify(result)).byteLength;
                if (window.__startupMetrics.firstRevision === null) window.__startupMetrics.firstRevision = result.revision;
            }
        }, () => {entry.ms = performance.now() - entry.start;});
        return promise;
    };
})()`

async function main() {
    if (!adb || process.env.ANDROID_ADB_SERVER_PORT !== '5038')
        throw new Error('Private ADB server required')
    const name = run(serial, ['emu', 'avd', 'name']).stdout.trim().split(/\r?\n/)[0]
    if (name !== 'risunest_startup_synthetic') throw new Error('Unsafe Android AVD')
    phase = 'network'
    console.log(JSON.stringify({ phase }))
    await cutDeviceNetwork(serial, { run, sleep: delay })
    phase = 'install'
    console.log(JSON.stringify({ phase }))
    run(serial, ['install', '-r', apk])
    phase = 'launch'
    console.log(JSON.stringify({ phase }))
    run(serial, ['shell', 'am', 'start', '-n', `${packageName}/.MainActivity`])
    let client
    const samples = []
    try {
        await delay(2000)
        const pid = run(serial, ['shell', 'pidof', packageName]).stdout.trim()
        if (!/^\d+$/.test(pid)) throw new Error('Synthetic Android app unavailable')
        run(serial, ['forward', `tcp:${port}`, `localabstract:webview_devtools_remote_${pid}`])
        phase = 'connect'
        console.log(JSON.stringify({ phase }))
        client = await connectSyntheticAndroid(port, adb, serial)
        phase = 'bootstrap'
        console.log(JSON.stringify({ phase }))
        await waitForInteractive(client)
        await client.evaluate("localStorage.setItem('startupInteract', 'false')")
        await client.call('Page.addScriptToEvaluateOnNewDocument', {
            source: instrumentation + nativeObservation,
        })
        async function reload() {
            const origin = await client.evaluate('performance.timeOrigin')
            await client.call('Page.reload')
            const deadline = Date.now() + 120_000
            while ((await client.evaluate('performance.timeOrigin')) === origin) {
                if (Date.now() > deadline) throw new Error('Synthetic Android navigation timed out')
                await delay(100)
            }
            await waitForInteractive(client)
            await delay(10_000)
        }
        for (const mutation of [false, true]) {
            phase = mutation ? 'mutation' : 'noop'
            console.log(JSON.stringify({ phase }))
            const seeded = await client.evaluate(
                seedExpression({
                    characters: 3,
                    bytes: 8_388_608,
                    pluginBytes: 2_097_152,
                    locale: 'ko',
                    mutation,
                }),
            )
            if (!seeded.success) throw new Error('Synthetic Android seed failed')
            await reload() // Separate normalization and initial plugin execution from the sample.
            const before = await client.evaluate(
                "(async () => (await window.__TAURI_INTERNALS__.invoke('pds_read_root')).revision)()",
            )
            await reload()
            const sample = sanitizeMetrics(
                await client.evaluate(`(async () => ({
                ...window.__startupMetrics,
                interactiveMs: performance.getEntriesByName('boot:interactive')[0].startTime,
                lastRevision: (await window.__TAURI_INTERNALS__.invoke('pds_read_root')).revision,
                elapsedVisibleAfterStartup: !!document.querySelector('.loading-progress > [aria-live="off"]'),
                documentVisible: document.visibilityState === 'visible',
            }))()`),
            )
            const commits = sample.calls.filter(
                (c) => c.command === 'pds_commit' && c.success,
            ).length
            const revisionDelta = sample.lastRevision - before
            const success =
                commits === Number(mutation) &&
                revisionDelta === Number(mutation) &&
                sample.elapsedSeen &&
                !sample.elapsedVisibleAfterStartup &&
                sample.phases.some((p) => p.stage === 'compatibility') &&
                sample.phases.some((p) => p.stage === 'plugins')
            samples.push({ mutation, commits, revisionDelta, success, ...sample })
            await writeFile(
                output,
                JSON.stringify({ platform: 'android-emulator', synthetic: true, samples }, null, 2),
            )
            console.log(JSON.stringify({ mutation, commits, revisionDelta, success }))
            if (!success) throw new Error('Synthetic Android acceptance failed')
        }
    } finally {
        client?.close()
        run(serial, ['shell', 'am', 'force-stop', packageName], { allowFailure: true })
        run(serial, ['forward', '--remove', `tcp:${port}`], { allowFailure: true })
    }
}

main().catch(() => {
    console.error(JSON.stringify({ phase, success: false }))
    process.exitCode = 1
})
