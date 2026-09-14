import { spawn, execFileSync } from 'node:child_process'
import { once } from 'node:events'
import { mkdir, mkdtemp, readFile, writeFile, readdir, rename } from 'node:fs/promises'
import { createHash } from 'node:crypto'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { buildBenchmarkConfig } from '../phase3/tauri-cdp.mjs'
import { connect, delay, instrumentation, waitForInteractive } from './cdp.mjs'
import { sanitizeMetrics } from './metrics.mjs'
import { observeWal, readWindowsMemory } from './host-metrics.mjs'
import { runOnPrivateDesktop } from './background.mjs'
import { addSyntheticAssets, assertSyntheticProfile, seedExpression } from './fixture.mjs'

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
const options = Object.fromEntries(
    process.argv.slice(2).map((arg) => {
        const [key, value = 'true'] = arg.replace(/^--/, '').split('=')
        return [key, value]
    }),
)
const runs = Number(options.runs ?? (options.suite ? 20 : 5))
const assets = Number(options.assets ?? 11345)
const characters = Number(options.characters ?? (options.suite ? 500 : 11))
const bytes = Number(options.bytes ?? (options.suite ? 104_857_600 : 98_000_000))
const legacyAssets = Number(options.legacyAssets ?? 0)
const referencedAssets = Number(options.referencedAssets ?? 0)
const images = !!options.suite || options.images === 'true'
for (const value of [runs, assets, characters, bytes, legacyAssets, referencedAssets]) {
    if (!Number.isSafeInteger(value) || value < 0) throw new Error('Invalid numeric option')
}
if (referencedAssets > (options.suite ? 11345 : assets) || (images && assets < 16)) {
    throw new Error('Synthetic body references require matching asset objects')
}

async function run(command, args, env, output = 'ignore') {
    const child = spawn(command, args, {
        cwd: repository,
        env,
        windowsHide: true,
        stdio: output,
    })
    const [code] = await once(child, 'exit')
    if (code !== 0) throw new Error('Benchmark child failed')
}

const stopped = new WeakSet()
async function stop(child, client) {
    if (!child || stopped.has(child) || child.exitCode !== null) return
    if (client) {
        const exited = once(child, 'exit')
        // A normal close lets WebView persist controls before the next process.
        void client
            .evaluate("window.__TAURI_INTERNALS__.invoke('plugin:window|close', {label: 'main'})")
            .catch(() => {})
        await Promise.race([exited, delay(5000)])
    }
    client?.close()
    if (child.exitCode === null)
        await run('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], process.env)
    if (child.exitCode === null && child.signalCode === null)
        await once(child, 'exit', { signal: AbortSignal.timeout(15_000) })
    stopped.add(child)
}

async function main() {
    if (options.kind && !['reload', 'restart'].includes(options.kind))
        throw new Error('Invalid measurement kind')
    const server = net.createServer()
    server.listen(0, '127.0.0.1')
    await once(server, 'listening')
    const port = server.address().port
    await new Promise((resolve) => server.close(resolve))
    const temporary = options.config
        ? path.dirname(path.resolve(options.config))
        : await mkdtemp(path.join(os.tmpdir(), 'risunest-startup-'))
    const revision = execFileSync(
        'git',
        ['-c', `safe.directory=${repository.replaceAll('\\', '/')}`, 'rev-parse', 'HEAD'],
        { cwd: repository, encoding: 'utf8' },
    ).trim()
    const original = JSON.parse(
        await readFile(path.join(repository, 'src-tauri/tauri.conf.json'), 'utf8'),
    )
    const config = options.config
        ? JSON.parse(await readFile(options.config, 'utf8'))
        : buildBenchmarkConfig(original, port, path.basename(temporary), revision)
    if (!config.identifier.startsWith('RisuNest.phase3benchmark.')) throw new Error('Unsafe config')
    const cdpPort = options.config
        ? Number(
              config.app.windows[0].additionalBrowserArgs.match(/--remote-debugging-port=(\d+)/)[1],
          )
        : port
    const configPath = path.join(temporary, 'tauri.startup.json')
    const agent = JSON.parse(
        await readFile(path.join(repository, 'src-tauri/tauri.agent.conf.json'), 'utf8'),
    )
    config.build = {
        ...config.build,
        ...agent.build,
        beforeBuildCommand: 'node benchmarks/startup/before-build.mjs',
        frontendDist: '../.tmp/startup-benchmark-dist',
    }
    // An absolute WebView directory prevents cwd-dependent reuse.
    config.app.windows[0].dataDirectory = path.join(temporary, 'webview')
    if (!options.config || options.rebuild === 'true')
        await writeFile(configPath, JSON.stringify(config))
    const env = {
        ...process.env,
        APPDATA: path.join(temporary, 'roaming'),
        LOCALAPPDATA: path.join(temporary, 'local'),
        CARGO_TARGET_DIR: path.join(repository, 'src-tauri/target'),
        VITE_RISU_LEGAL_CONFIGURED: 'TRUE',
        STARTUP_BENCHMARK_VARIANT: options.variant ?? 'optimized',
    }
    await Promise.all([
        mkdir(env.APPDATA, { recursive: true }),
        mkdir(env.LOCALAPPDATA, { recursive: true }),
    ])
    if (!options.config || options.rebuild === 'true') {
        await run(
            process.execPath,
            [
                path.join(repository, 'node_modules/@tauri-apps/cli/tauri.js'),
                'build',
                '--no-bundle',
                '--ci',
                '--config',
                'src-tauri/tauri.agent.conf.json',
                '--config',
                configPath,
            ],
            env,
            'inherit',
        )
    }
    if (
        process.platform === 'win32' &&
        process.env.STARTUP_BENCHMARK_DISPLAY !== 'private-desktop' &&
        options.display !== 'foreground'
    ) {
        // Compilers already run without GUI windows. Keep their native worker
        // threads on the regular desktop; isolate only the actual app runner.
        const backgroundArgs = process.argv
            .slice(2)
            .filter((arg) => !arg.startsWith('--config=') && !arg.startsWith('--rebuild='))
        await runOnPrivateDesktop(
            [...backgroundArgs, `--config=${configPath}`],
            options.output ?? 'benchmarks/startup/result.local.json',
        )
        return
    }
    if (options.fresh === 'true') {
        const savedRoot = JSON.parse(
            await readFile(path.join(temporary, 'profile-root.local.json'), 'utf8'),
        )
        if (savedRoot.identifier !== config.identifier) throw new Error('Unsafe fixture reset')
        const oldRoot = savedRoot.root
        assertSyntheticProfile(oldRoot, config.identifier)
        const archive = path.join(temporary, `retained-synthetic-${Date.now()}`)
        // Preserve the previous fixture. Never remove or move a path outside this isolated run.
        try {
            await rename(oldRoot, archive)
        } catch (error) {
            if (error.code !== 'ENOENT') throw error
        }
    }
    let child, client, launchEpoch
    const samples = []
    let fixture
    let seededStatistics,
        profileRoot,
        recentSnapshotReady = false
    const binarySha256 = createHash('sha256')
        .update(
            await readFile(
                path.join(repository, 'src-tauri/target/release', original.mainBinaryName + '.exe'),
            ),
        )
        .digest('hex')
    let scenario = {
        assets,
        mode: options.mode ?? 'compatibility',
        recentSnapshot: options.snapshot !== 'missing',
        interaction: !!options.suite || options.interact === 'true',
    }
    const output = path.resolve(options.output ?? 'benchmarks/startup/result.local.json')
    const persist = () =>
        writeFile(
            output,
            JSON.stringify(
                {
                    revision,
                    productPatch:
                        options.config && options.rebuild !== 'true'
                            ? 'verified-reused-binary'
                            : (options.variant ?? 'optimized'),
                    binarySha256,
                    display: process.env.STARTUP_BENCHMARK_DISPLAY ?? 'foreground',
                    fixture: {
                        ...fixture,
                        ...seededStatistics,
                        characters,
                        targetBytes: bytes,
                        legacyAssets,
                        referencedAssets,
                        bodyReferencedObjects: Math.max(
                            referencedAssets,
                            images ? Math.min(16, characters) : 0,
                        ),
                    },
                    samples,
                    limitations: [
                        'OS caches were not cleared',
                        'IPC duration includes transport and queueing, not an isolated native mutex timer',
                    ],
                },
                null,
                2,
            ),
        )
    const launch = async () => {
        const started = performance.now()
        launchEpoch = performance.timeOrigin + started
        child = spawn(
            path.join(repository, 'src-tauri/target/release', original.mainBinaryName + '.exe'),
            [],
            { cwd: temporary, env, windowsHide: true, stdio: 'ignore' },
        )
        client = await Promise.race([
            connect(cdpPort, config.identifier),
            once(child, 'exit').then(() => {
                throw new Error('Isolated app exited before connection')
            }),
        ])
        await waitForInteractive(client)
        return performance.now() - started
    }
    const prepareNext = async () => {
        if (legacyAssets > 0) {
            assertSyntheticProfile(profileRoot, config.identifier)
            const directory = path.join(profileRoot, 'assets')
            await mkdir(directory, { recursive: true })
            for (let i = 0; i < legacyAssets; i++)
                await writeFile(
                    path.join(directory, `startup-legacy-${i}.bin`),
                    'Synthetic legacy asset',
                )
        }
        await client.evaluate(`(async () => {
            const invoke = window.__TAURI_INTERNALS__.invoke;
            if (${!scenario.recentSnapshot || !recentSnapshotReady}) {
                const snapshots = await invoke('pds_snapshot_list');
                if (${scenario.recentSnapshot}) {
                    if (!snapshots.length) await invoke('pds_snapshot_create', {reason: 'manual'});
                } else {
                    for (const snapshot of snapshots) await invoke('pds_snapshot_delete', {path: snapshot.path});
                }
            }
            localStorage.setItem('startupInteract', '${scenario.interaction}');
            localStorage.setItem('risuNestDeviceSettings', JSON.stringify({schema: 'risunest.device-settings/v1',
                performanceProfile: 'normal', androidKeepAliveDuringGeneration: false, nativeFileLogEnabled: false}));
        })()`)
        recentSnapshotReady = scenario.recentSnapshot
    }
    const measure = async (kind, warmup = false) => {
        const finishWal =
            options.hostMetrics === 'true' ? await observeWal(profileRoot, config.identifier) : null
        try {
            let launchToReadyMs = null
            if (kind === 'restart') launchToReadyMs = await launch()
            else {
                const origin = await client.evaluate('performance.timeOrigin')
                await client.call('Page.reload')
                const navigationDeadline = Date.now() + 120_000
                while ((await client.evaluate('performance.timeOrigin')) === origin) {
                    if (Date.now() > navigationDeadline) throw new Error('Reload did not navigate')
                    await delay(50)
                }
                await waitForInteractive(client)
            }
            await delay(10_000)
            const deadline = Date.now() + 20_000
            let settled = false
            do {
                settled = await client.evaluate(`(() => {
                const s = window.__startupMetrics;
                if (!s) return true;
                return s.operations.some(o => o.stage === 'flush')
                    && !Object.values(s.active).some(n => n > 0)
                    && !s.calls.some(c => c.ms === null);
            })()`)
                if (settled) break
                await delay(250)
            } while (Date.now() < deadline)
            const sample = sanitizeMetrics(
                await client.evaluate(`(async () => ({
            interactiveMs: performance.getEntriesByName('boot:interactive')[0]?.startTime ?? null,
            firstPaintMs: performance.getEntriesByName('first-contentful-paint')[0]?.startTime ?? null,
            marks: performance.getEntriesByType('mark').filter(e => /^boot:[a-z-]+$/.test(e.name)).map(e => ({stage: e.name, ms: e.startTime})),
            calls: window.__startupMetrics?.calls ?? null,
            longTasks: window.__startupMetrics?.longTasks ?? null,
            operations: window.__startupMetrics?.operations,
            phases: window.__startupMetrics?.phases,
            active: window.__startupMetrics?.active,
            elapsedSeen: window.__startupMetrics?.elapsedSeen,
            elapsedVisibleAfterStartup: !!document.querySelector('.loading-progress > [aria-live="off"]'),
            interaction: window.__startupMetrics?.interaction,
            firstRevision: window.__startupMetrics?.firstRevision,
            lastRevision: (await window.__TAURI_INTERNALS__.invoke('pds_read_root')).revision,
            usedHeapBytes: performance.memory?.usedJSHeapSize ?? null,
            sampledPeakHeapBytes: window.__startupMetrics?.peakHeapBytes,
            maxMediaInFlight: window.__startupMetrics?.maxMediaInFlight,
            documentVisible: document.visibilityState === 'visible',
            cleanChunksReason: window.__startupMetrics?.cleanChunksReason,
            imageResources: performance.getEntriesByType('resource').filter(e => e.initiatorType === 'img').map(e => ({start: e.startTime, ms: e.duration})),
            stabilizationTimeout: ${!settled},
        }))()`),
            )
            const legacyAssetsAfter = (await readdir(path.join(profileRoot, 'assets'))).length
            const paintEpoch =
                kind === 'restart'
                    ? await client.evaluate(`(() => {
                const paint = performance.getEntriesByName('first-contentful-paint')[0];
                return paint ? performance.timeOrigin + paint.startTime : null;
            })()`)
                    : null
            const launchToFirstPaintMs =
                Number.isFinite(paintEpoch) && Number.isFinite(launchEpoch)
                    ? paintEpoch - launchEpoch
                    : null
            samples.push({
                ...scenario,
                kind,
                warmup,
                launchToReadyMs,
                launchToFirstPaintMs,
                legacyAssetsAfter,
                ...sample,
                ...(finishWal
                    ? {
                          host: {
                              wal: await finishWal(),
                              memory: await readWindowsMemory(child.pid),
                          },
                      }
                    : {}),
            })
            await persist()
            if (
                warmup &&
                scenario.interaction &&
                (!sample.interaction?.success || sample.interaction.failure !== null)
            )
                throw new Error('Visible interaction warmup failed')
            console.log(
                JSON.stringify({
                    kind,
                    warmup,
                    interactiveMs: sample.interactiveMs,
                    mode: scenario.mode,
                    objects: scenario.assets,
                    selectionMs: sample.interaction?.selectionMs,
                    settled: !sample.stabilizationTimeout,
                }),
            )
        } finally {
            if (finishWal) await finishWal()
        }
    }
    try {
        console.log(JSON.stringify({ phase: 'initial-launch' }))
        await launch()
        const root = await client.evaluate(
            `window.__TAURI_INTERNALS__.invoke('plugin:path|resolve_directory', {directory: 14})`,
        )
        assertSyntheticProfile(root, config.identifier)
        profileRoot = root
        await writeFile(
            path.join(temporary, 'profile-root.local.json'),
            JSON.stringify({ identifier: config.identifier, root }),
        )
        console.log(JSON.stringify({ phase: 'seed' }))
        const seeded = await client.evaluate(
            seedExpression({
                characters,
                bytes,
                compatibility: options.mode !== 'scalable',
                images: !!options.suite || options.images === 'true',
                investigation: options.shape === 'investigation',
                mutation: options.mutation === 'true',
                referencedAssets,
                coldStorage: options.coldStorage === 'true',
                locale: options.locale ?? 'en',
            }),
        )
        console.log(JSON.stringify({ phase: 'seed-result', ...seeded }))
        if (!seeded.success) throw new Error('Synthetic seed failed')
        seededStatistics = seeded
        await client.evaluate(
            `localStorage.setItem('startupInteract', ${JSON.stringify(options.suite || options.interact === 'true' ? 'true' : 'false')})`,
        )
        await stop(child, client)
        fixture = await addSyntheticAssets(
            root,
            config.identifier,
            assets,
            !!options.suite || options.images === 'true',
        )
        console.log(JSON.stringify({ phase: 'assets-ready', objects: fixture.objects }))
        const scenarios = options.suite
            ? [
                  {
                      assets: 11345,
                      mode: 'compatibility',
                      recentSnapshot: true,
                      interaction: true,
                  },
                  {
                      assets: 11345,
                      mode: 'scalable',
                      recentSnapshot: true,
                      interaction: true,
                  },
                  {
                      assets: 100000,
                      mode: 'compatibility',
                      recentSnapshot: true,
                      interaction: true,
                  },
                  {
                      assets: 100000,
                      mode: 'scalable',
                      recentSnapshot: true,
                      interaction: true,
                  },
              ]
            : [scenario]
        for (const next of scenarios) {
            scenario = next
            recentSnapshotReady = false
            // The initial fixture already has this count. Reopen the offline
            // store only when growing it, avoiding a redundant WAL checkpoint.
            if (fixture.objects !== scenario.assets) {
                fixture = await addSyntheticAssets(
                    root,
                    config.identifier,
                    scenario.assets,
                    !!options.suite || options.images === 'true',
                )
            }
            console.log(
                JSON.stringify({
                    phase: 'mode-control',
                    mode: scenario.mode,
                    objects: scenario.assets,
                }),
            )
            await launch()
            // A previous sample can leave the interaction flag enabled. Finish
            // that setup launch's first save before committing a mode switch.
            await delay(10_000)
            const modeResult = await client.evaluate(`(async () => {
                try {
                const invoke = window.__TAURI_INTERNALS__.invoke;
                const current = await invoke('pds_read_root');
                current.value.plugins[0].enabled = ${scenario.mode !== 'scalable'};
                await invoke('pds_commit', {commit: {expectedRevision: current.revision, root: current.value}, assetAliases: []});
                return {success: true};
                } catch (error) { return {success: false, conflict: error?.code === 'revision-conflict'}; }
            })()`)
            console.log(JSON.stringify({ phase: 'mode-result', ...modeResult }))
            if (!modeResult.success) throw new Error('Synthetic mode switch failed')
            await prepareNext()
            await stop(child, client)
            // Exclude normalization and the seed WAL checkpoint from measured samples.
            await measure('restart', true)
            await client.evaluate(
                `localStorage.setItem('startupInteract', ${JSON.stringify(options.suite || options.interact === 'true' ? 'true' : 'false')})`,
            )
            if (options.config && !(await client.evaluate('!!window.__startupRecord'))) {
                await client.call('Page.addScriptToEvaluateOnNewDocument', {
                    source: instrumentation,
                })
            }
            for (let i = 0; i < (options.kind === 'restart' ? 0 : runs); i++) {
                await prepareNext()
                await measure('reload')
            }
            await prepareNext()
            await stop(child, client)
            for (let i = 0; i < (options.kind === 'reload' ? 0 : runs); i++) {
                await measure('restart')
                await prepareNext()
                await stop(child, client)
            }
        }
        await persist()
    } finally {
        await stop(child, client)
    }
}

main().catch((error) => {
    // Emit source locations and fixed classifications, never exception text.
    console.error(
        JSON.stringify({
            success: false,
            sqliteError: Number.isSafeInteger(error?.errcode) ? error.errcode : null,
            categories: [
                'locked',
                'no such table',
                'disk I/O',
                'malformed',
                'readonly',
                'unable to open',
            ].filter((value) => String(error?.message ?? '').includes(value)),
            code: [
                'SQLITE_BUSY',
                'SQLITE_CANTOPEN',
                'SQLITE_ERROR',
                'ERR_SQLITE_ERROR',
                'ENOENT',
                'EACCES',
                'EPERM',
            ].includes(error?.code)
                ? error.code
                : null,
            locations: [
                ...String(error?.stack ?? '').matchAll(/(profile|fixture|cdp)\.mjs:(\d+):(\d+)/g),
            ]
                .slice(0, 4)
                .map((match) => ({
                    file: match[1],
                    line: Number(match[2]),
                    column: Number(match[3]),
                })),
        }),
    )
    process.exitCode = 1
})
