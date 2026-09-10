import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'
import corpus from './native-tokenizer-corpus.json' with { type: 'json' }

const CDP_HOST = '127.0.0.1'
const TEMP_PREFIX = 'risunest-tokenizer-tauri-'
const FINGERPRINTS = Object.fromEntries(
    Object.entries(corpus.artifacts).map(([tokenizerId, artifact]) => [tokenizerId, artifact.fingerprint]),
)

export function parseArguments(argumentsList) {
    const options = { output: null, keepProfile: false, samples: 20, timeoutMs: 300_000 }
    for (let index = 0; index < argumentsList.length; index++) {
        const argument = argumentsList[index]
        if (argument === '--output') {
            options.output = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--samples') {
            options.samples = positiveInteger(requiredValue(argumentsList, ++index, argument), argument)
        } else if (argument === '--timeout-ms') {
            options.timeoutMs = positiveInteger(requiredValue(argumentsList, ++index, argument), argument)
        } else if (argument === '--keep-profile') {
            options.keepProfile = true
        } else if (argument === '--help' || argument === '-h') {
            return { help: true }
        } else {
            throw new Error(`Unknown argument: ${argument}`)
        }
    }
    return options
}

function requiredValue(argumentsList, index, option) {
    const value = argumentsList[index]
    if (!value || value.startsWith('--')) throw new Error(`${option} requires a value`)
    return value
}

function positiveInteger(value, option) {
    const parsed = Number(value)
    if (!Number.isSafeInteger(parsed) || parsed <= 0) {
        throw new Error(`${option} requires a positive integer`)
    }
    return parsed
}

export function percentile(values, fraction) {
    if (values.length === 0) throw new Error('percentile requires at least one sample')
    const sorted = [...values].sort((left, right) => left - right)
    const index = Math.max(0, Math.ceil(sorted.length * fraction) - 1)
    return sorted[index]
}

export function buildShortSegments(count) {
    return Array.from(
        { length: count },
        (_, index) => `chat segment ${index}: user asks a concise tokenizer question.`,
    )
}

export function buildBenchmarkConfig(original, port, runId, repositoryRoot = process.cwd()) {
    const safeRunId = runId.replaceAll(/[^a-zA-Z0-9]/g, '')
    const browserArguments = [
        `--remote-debugging-port=${port}`,
        '--remote-allow-origins=*',
        '--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection',
    ].join(' ')
    const windows = (original.app?.windows ?? [{}]).map((window, index) => ({
        ...window,
        ...(index === 0
            ? {
                  dataDirectory: `tokenizer-benchmark-${runId}`,
                  additionalBrowserArgs: browserArguments,
              }
            : {}),
    }))
    return {
        ...structuredClone(original),
        build: {
            ...original.build,
            beforeBuildCommand: 'pnpm benchmark:tokenizer:build:agent',
            frontendDist: path.resolve(repositoryRoot, 'benchmarks/tokenizer/dist'),
        },
        identifier: `RisuNest.tokenizerbenchmark.${safeRunId}`,
        bundle: { ...original.bundle, active: false },
        plugins: {
            ...original.plugins,
            updater: { ...original.plugins?.updater, endpoints: [] },
        },
        app: { ...original.app, windows },
    }
}

export function resolveCargoTargetDirectory(repositoryRoot, configuredTarget) {
    return configuredTarget
        ? path.resolve(configuredTarget)
        : path.join(repositoryRoot, 'src-tauri', 'target')
}

class CdpClient {
    constructor(url) {
        this.url = url
        this.socket = null
        this.nextId = 1
        this.pending = new Map()
    }

    async connect(timeoutMs) {
        const socket = new WebSocket(this.url)
        this.socket = socket
        const timeout = setTimeout(() => socket.close(), timeoutMs)
        try {
            await Promise.race([
                onceEvent(socket, 'open'),
                onceEvent(socket, 'error').then(([event]) => {
                    throw event.error ?? new Error(`CDP WebSocket failed: ${this.url}`)
                }),
            ])
        } finally {
            clearTimeout(timeout)
        }
        socket.addEventListener('message', (event) => this.#onMessage(event.data))
        socket.addEventListener('close', () => this.#rejectPending(new Error('CDP WebSocket closed')))
    }

    call(method, params = {}) {
        if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
            return Promise.reject(new Error('CDP WebSocket is not open'))
        }
        const id = this.nextId++
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject, method })
            this.socket.send(JSON.stringify({ id, method, params }))
        })
    }

    close() {
        this.socket?.close()
    }

    #onMessage(data) {
        const message = JSON.parse(typeof data === 'string' ? data : Buffer.from(data).toString())
        if (!message.id) return
        const pending = this.pending.get(message.id)
        if (!pending) return
        this.pending.delete(message.id)
        if (message.error) pending.reject(new Error(`${pending.method}: ${message.error.message}`))
        else pending.resolve(message.result)
    }

    #rejectPending(error) {
        for (const pending of this.pending.values()) pending.reject(error)
        this.pending.clear()
    }
}

function onceEvent(target, name) {
    return new Promise((resolve) => {
        target.addEventListener(name, (...argumentsList) => resolve(argumentsList), { once: true })
    })
}

async function getFreePort() {
    const server = net.createServer()
    server.listen(0, CDP_HOST)
    await once(server, 'listening')
    const address = server.address()
    const port = typeof address === 'object' && address ? address.port : null
    server.close()
    await once(server, 'close')
    if (port === null) throw new Error('Failed to reserve a CDP port')
    return port
}

async function runCommand(command, args, options) {
    const child = spawn(command, args, {
        cwd: options.cwd,
        env: options.env,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
    })
    options.onSpawn?.(child)
    child.stdout.on('data', (chunk) => process.stderr.write(chunk))
    child.stderr.on('data', (chunk) => process.stderr.write(chunk))
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0) throw new Error(`${command} exited with code ${exitCode}`)
}

async function waitForCdp(port, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    let lastError
    while (Date.now() < deadline) {
        try {
            const response = await fetch(`http://${CDP_HOST}:${port}/json/list`)
            if (response.ok) {
                const targets = await response.json()
                const page = targets.find(
                    (target) => target.type === 'page' && typeof target.webSocketDebuggerUrl === 'string',
                )
                if (page) return page
            }
        } catch (error) {
            lastError = error
        }
        await delay(100)
    }
    throw new Error(`CDP endpoint did not become ready: ${lastError ?? 'no page target'}`)
}

async function evaluate(client, expression) {
    const response = await client.call('Runtime.evaluate', {
        expression,
        awaitPromise: true,
        returnByValue: true,
        userGesture: true,
    })
    if (response.exceptionDetails) {
        throw new Error(
            response.exceptionDetails.exception?.description ??
                response.exceptionDetails.text ??
                'Runtime evaluation failed',
        )
    }
    return response.result.value
}

async function waitForInvoke(page, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        if (await evaluate(page, 'Boolean(globalThis.__TAURI_INTERNALS__?.invoke)')) return
        await delay(100)
    }
    throw new Error('Tauri invoke did not become available')
}

async function waitForBenchmarkSeam(page, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        if (await evaluate(page, 'Boolean(globalThis.__RISUNEST_TOKENIZER_BENCHMARK__)')) return
        await delay(100)
    }
    throw new Error('Tokenizer benchmark WebView seam did not become available')
}

function delay(milliseconds) {
    return new Promise((resolve) => setTimeout(resolve, milliseconds))
}

export function installSignalCleanup(processLike, cleanup, exit) {
    let handlingSignal = false
    const remove = () => {
        processLike.removeListener('SIGINT', handleSigint)
        processLike.removeListener('SIGTERM', handleSigterm)
    }
    const handle = (signal) => {
        if (handlingSignal) return
        handlingSignal = true
        remove()
        const exitCode = signal === 'SIGINT' ? 130 : 143
        void Promise.resolve()
            .then(cleanup)
            .catch((error) => process.stderr.write(`Signal cleanup failed: ${error.stack ?? error}${os.EOL}`))
            .finally(() => exit(exitCode))
    }
    const handleSigint = () => handle('SIGINT')
    const handleSigterm = () => handle('SIGTERM')
    processLike.once('SIGINT', handleSigint)
    processLike.once('SIGTERM', handleSigterm)
    return remove
}

function resolveCorpusInput(input) {
    if (input.kind === 'text') return input.value
    if (input.kind === 'utf16-code-units') {
        return String.fromCharCode(...input.codeUnits).toWellFormed()
    }
    return input.value.repeat(input.count)
}

function benchmarkSeamExpression(method, argumentsList) {
    return `globalThis.__RISUNEST_TOKENIZER_BENCHMARK__.${method}(...${JSON.stringify(argumentsList)})`
}

function callBenchmarkSeam(page, method, ...argumentsList) {
    return evaluate(page, benchmarkSeamExpression(method, argumentsList))
}

function nativeExpression(request) {
    return `(async () => {
        const request = ${JSON.stringify(request)}
        const startedAt = performance.now()
        try {
            const result = await globalThis.__TAURI_INTERNALS__.invoke('tokenize_batch', { request })
            return { ok: true, durationMs: performance.now() - startedAt, result }
        } catch (error) {
            return {
                ok: false,
                durationMs: performance.now() - startedAt,
                error: typeof error === 'object' && error !== null
                    ? { ...error, message: error.message ?? String(error) }
                    : { message: String(error) },
            }
        }
    })()`
}

async function invokeNative(page, request) {
    return evaluate(page, nativeExpression(request))
}

function assertEqual(actual, expected, label) {
    assertJsonEqual(actual, expected, label)
}

function assertJsonEqual(actual, expected, label) {
    const actualJson = JSON.stringify(actual)
    const expectedJson = JSON.stringify(expected)
    if (actualJson !== expectedJson) {
        throw new Error(`${label} mismatch\nexpected: ${expectedJson}\nactual: ${actualJson}`)
    }
}

async function runIntegratedParity(page, tokenizerId) {
    const entries = corpus.cases.filter((entry) => entry.tokenizerId === tokenizerId)
    const successes = entries.filter((entry) => entry.ids)
    const request = {
        tokenizer_id: tokenizerId,
        artifact_fingerprint: FINGERPRINTS[tokenizerId],
        mode: 'ids',
        texts: successes.map((entry) => resolveCorpusInput(entry.input)),
    }
    const response = await invokeNative(page, request)
    if (!response.ok) throw new Error(`${tokenizerId} integrated parity failed: ${JSON.stringify(response.error)}`)
    assertEqual(
        response.result,
        {
            mode: 'ids',
            artifact_fingerprint: FINGERPRINTS[tokenizerId],
            ids: successes.map((entry) => entry.ids),
        },
        `${tokenizerId} IDs corpus`,
    )

    let errorCases = 0
    for (const entry of entries.filter((candidate) => candidate.error)) {
        const errorResponse = await invokeNative(page, {
            tokenizer_id: tokenizerId,
            artifact_fingerprint: FINGERPRINTS[tokenizerId],
            mode: 'ids',
            texts: [resolveCorpusInput(entry.input)],
        })
        if (errorResponse.ok) throw new Error(`${entry.name} should have failed`)
        assertEqual(
            {
                code: errorResponse.error.code,
                index: errorResponse.error.index,
                special_token: errorResponse.error.special_token,
            },
            { code: entry.error.code, index: 0, special_token: entry.error.token },
            `${entry.name} error`,
        )
        errorCases++
    }
    return { idCases: successes.length, errorCases, passed: true }
}

function summarizeDurations(durations) {
    return {
        samples: durations.length,
        p50Ms: percentile(durations, 0.5),
        p95Ms: percentile(durations, 0.95),
        minMs: Math.min(...durations),
        maxMs: Math.max(...durations),
    }
}

function benchmarkFixtures() {
    const promptParagraph =
        'System: answer carefully. User: Explain tokenizer batching with Unicode 안녕하세요 and code `value += 1`. '
    return [
        ...[1, 10, 100, 1_000].map((size) => ({
            name: `short-segments-${size}`,
            texts: buildShortSegments(size),
        })),
        { name: 'prompt-32-kib', texts: ['P'.repeat(32 * 1024)] },
        {
            name: 'realistic-prompt-512-kib',
            texts: [promptParagraph.repeat(Math.ceil((512 * 1024) / promptParagraph.length)).slice(0, 512 * 1024)],
        },
    ]
}

async function collectWebViewGarbage(page) {
    await page.call('HeapProfiler.collectGarbage')
}

async function getWebViewHeap(page) {
    const heap = await page.call('Runtime.getHeapUsage')
    return { usedBytes: heap.usedSize, totalBytes: heap.totalSize }
}

async function measureWebViewImplementation(page, implementation, request, samples) {
    await collectWebViewGarbage(page)
    const heapBefore = await getWebViewHeap(page)
    const measurement = await callBenchmarkSeam(page, 'measure', implementation, request, samples)
    await collectWebViewGarbage(page)
    const heapAfter = await getWebViewHeap(page)
    return {
        timing: summarizeDurations(measurement.durationsMs),
        result: measurement.result,
        heap: {
            beforeUsedBytes: heapBefore.usedBytes,
            afterUsedBytes: heapAfter.usedBytes,
            deltaUsedBytes: heapAfter.usedBytes - heapBefore.usedBytes,
            beforeTotalBytes: heapBefore.totalBytes,
            afterTotalBytes: heapAfter.totalBytes,
        },
    }
}

async function stopOwnedProcessTree(child) {
    if (!child || child.exitCode !== null) return
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
        windowsHide: true,
        stdio: 'ignore',
    })
    await once(killer, 'exit')
    if (child.exitCode !== null) return
    await Promise.race([once(child, 'exit'), delay(5_000)])
}

function assertSafeTemporaryDirectory(directory) {
    const temporaryRoot = path.resolve(os.tmpdir()) + path.sep
    const resolved = path.resolve(directory)
    if (!resolved.startsWith(temporaryRoot) || !path.basename(resolved).startsWith(TEMP_PREFIX)) {
        throw new Error(`Refusing to remove unexpected directory: ${resolved}`)
    }
}

async function runBenchmark(options) {
    if (process.platform !== 'win32') throw new Error('This benchmark supports Windows only')
    const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
    const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), TEMP_PREFIX))
    const runId = path.basename(temporaryRoot).slice(TEMP_PREFIX.length)
    const isolatedRoaming = path.join(temporaryRoot, 'roaming')
    const isolatedLocal = path.join(temporaryRoot, 'local')
    const port = await getFreePort()
    let buildProcess
    let appProcess
    let page
    let cleanupPromise

    const cleanupOwnedResources = (removeProfile) => {
        cleanupPromise ??= (async () => {
            page?.close()
            await Promise.all([
                stopOwnedProcessTree(buildProcess),
                stopOwnedProcessTree(appProcess),
            ])
            if (removeProfile) {
                assertSafeTemporaryDirectory(temporaryRoot)
                await rm(temporaryRoot, { recursive: true, force: true })
            } else {
                process.stderr.write(`Kept isolated benchmark profile: ${temporaryRoot}${os.EOL}`)
            }
        })()
        return cleanupPromise
    }
    const removeSignalCleanup = installSignalCleanup(
        process,
        () => cleanupOwnedResources(true),
        (code) => process.exit(code),
    )

    try {
        await Promise.all([
            mkdir(isolatedRoaming, { recursive: true }),
            mkdir(isolatedLocal, { recursive: true }),
        ])
        const baseConfig = JSON.parse(
            await readFile(path.join(repositoryRoot, 'src-tauri', 'tauri.conf.json'), 'utf8'),
        )
        const benchmarkConfig = buildBenchmarkConfig(baseConfig, port, runId, repositoryRoot)
        const configPath = path.join(temporaryRoot, 'tauri.tokenizer-benchmark.json')
        await writeFile(configPath, JSON.stringify(benchmarkConfig), 'utf8')
        const environment = {
            ...process.env,
            VITE_RISU_LEGAL_CONFIGURED: 'TRUE',
            APPDATA: isolatedRoaming,
            LOCALAPPDATA: isolatedLocal,
        }

        await runCommand(
            process.execPath,
            [
                path.join(repositoryRoot, 'node_modules', '@tauri-apps', 'cli', 'tauri.js'),
                'build',
                '--no-bundle',
                '--ci',
                '--config',
                configPath,
            ],
            {
                cwd: repositoryRoot,
                env: environment,
                onSpawn: (child) => {
                    buildProcess = child
                },
            },
        )
        buildProcess = undefined

        const cargoTarget = resolveCargoTargetDirectory(repositoryRoot, environment.CARGO_TARGET_DIR)
        const executable = path.join(cargoTarget, 'release', `${baseConfig.mainBinaryName}.exe`)
        appProcess = spawn(executable, [], {
            cwd: repositoryRoot,
            env: environment,
            windowsHide: true,
            stdio: ['ignore', 'pipe', 'pipe'],
        })
        appProcess.stdout.on('data', (chunk) => process.stderr.write(chunk))
        appProcess.stderr.on('data', (chunk) => process.stderr.write(chunk))

        const target = await waitForCdp(port, options.timeoutMs)
        page = new CdpClient(target.webSocketDebuggerUrl)
        await page.connect(options.timeoutMs)
        await page.call('Runtime.enable')
        await page.call('Network.enable')
        // Agent-mode endpoint blocking applies from boot; CDP keeps a second block.
        await page.call('Network.setBlockedURLs', { urls: REALM_BLOCKED_URL_PATTERNS })
        await page.call('HeapProfiler.enable')
        await waitForInvoke(page, options.timeoutMs)
        await waitForBenchmarkSeam(page, options.timeoutMs)
        const userAgent = await evaluate(page, 'navigator.userAgent')
        await collectWebViewGarbage(page)
        const heapBefore = await getWebViewHeap(page)

        const tokenizerResults = []
        for (const tokenizerId of ['cl100k_base', 'o200k_base']) {
            const javascriptInitialization = await callBenchmarkSeam(page, 'initialize', tokenizerId)
            const javascriptParity = await callBenchmarkSeam(
                page,
                'verifyJavaScriptCorpus',
                tokenizerId,
            )
            const coldRequest = {
                tokenizer_id: tokenizerId,
                artifact_fingerprint: FINGERPRINTS[tokenizerId],
                mode: 'count',
                texts: ['cold singleton initialization'],
            }
            const cold = await invokeNative(page, coldRequest)
            if (!cold.ok) throw new Error(`Cold native call failed: ${JSON.stringify(cold.error)}`)
            const nativeParity = await runIntegratedParity(page, tokenizerId)
            const cases = []
            for (const fixture of benchmarkFixtures()) {
                for (const mode of ['count', 'ids']) {
                    const request = {
                        tokenizerId,
                        mode,
                        texts: fixture.texts,
                    }
                    const javascriptWarmup = await callBenchmarkSeam(
                        page,
                        'warm',
                        'javascript',
                        request,
                    )
                    const nativeWarmup = await callBenchmarkSeam(page, 'warm', 'native', request)
                    assertEqual(nativeWarmup, javascriptWarmup, 'untimed warm-up result')

                    const javascript = await measureWebViewImplementation(
                        page,
                        'javascript',
                        request,
                        options.samples,
                    )
                    const native = await measureWebViewImplementation(
                        page,
                        'native',
                        request,
                        options.samples,
                    )
                    assertEqual(native.result, javascript.result, 'measured tokenizer result')
                    cases.push({
                        fixture: fixture.name,
                        mode,
                        batchItems: fixture.texts.length,
                        aggregateInputBytes: fixture.texts.reduce(
                            (total, text) => total + Buffer.byteLength(text),
                            0,
                        ),
                        warmup: {
                            performed: true,
                            javascript: javascriptWarmup,
                            native: nativeWarmup,
                        },
                        result: javascript.result,
                        javascript: { ...javascript.timing, heap: javascript.heap },
                        nativeEndToEnd: { ...native.timing, heap: native.heap },
                    })
                }
            }
            tokenizerResults.push({
                tokenizerId,
                fingerprint: FINGERPRINTS[tokenizerId],
                javascriptColdInitializationMs: javascriptInitialization.durationMs,
                nativeColdEndToEndMs: cold.durationMs,
                javascriptParity,
                nativeParity,
                cases,
            })
        }
        await collectWebViewGarbage(page)
        const heapAfter = await getWebViewHeap(page)

        return {
            schemaVersion: 2,
            measuredAt: new Date().toISOString(),
            platform: {
                os: `${os.type()} ${os.release()}`,
                arch: os.arch(),
                node: process.version,
                webViewUserAgent: userAgent,
            },
            build: {
                release: true,
                realmDisabled: true,
                isolatedProfile: true,
                cargoTargetDirectory: cargoTarget,
            },
            samplesPerWarmCase: options.samples,
            measurementContract: {
                timingRealm: 'release-tauri-webview',
                javascriptOutput: 'count number[] or IDs Uint32Array[]',
                nativeOutput: 'count number[] or timed IPC IDs conversion to Uint32Array[]',
                warmup: 'one untimed call per implementation, fixture, and mode',
                heap: 'WebView garbage collection and Runtime.getHeapUsage before and after each implementation',
                nodeDriverTimed: false,
            },
            webViewHeap: {
                beforeBytes: heapBefore.usedBytes,
                afterBytes: heapAfter.usedBytes,
                deltaBytes: heapAfter.usedBytes - heapBefore.usedBytes,
            },
            tokenizers: tokenizerResults,
            adoption: {
                productionRoutingEnabled: true,
                productionRoute: 'supported Tauri strongBan ID batches with at least 100 segments',
                reason: 'Corrected same-WebView Windows evidence shows a clear win for 100 and 1,000 segment batches.',
            },
            limits: [
                'JavaScript and native timing both run inside the same release Tauri WebView.',
                'The Node process orchestrates CDP and is not part of either measured interval.',
                'Production native routing is limited to supported Tauri strongBan ID batches of at least 100 segments.',
                'Small, single, unsupported, and Web tokenization remains JavaScript.',
                'Physical Android measurements were not collected for this evidence.',
                'Live RisuRealm and live account services are intentionally not exercised.',
            ],
        }
    } finally {
        removeSignalCleanup()
        await cleanupOwnedResources(!options.keepProfile)
    }
}

function usage() {
    return [
        'Usage: node benchmarks/tokenizer/tauri-cdp.mjs [options]',
        '',
        'Options:',
        '  --samples <integer>       Warm samples per case, default 20.',
        '  --timeout-ms <integer>    Build and CDP timeout, default 300000.',
        '  --output <path>           Also write the JSON result.',
        '  --keep-profile            Keep the isolated temporary profile.',
        '  -h, --help                Show this help.',
    ].join(os.EOL)
}

async function main() {
    const options = parseArguments(process.argv.slice(2))
    if (options.help) {
        process.stdout.write(`${usage()}${os.EOL}`)
        return
    }
    const result = await runBenchmark(options)
    const json = `${JSON.stringify(result, null, 2)}${os.EOL}`
    if (options.output) {
        const outputPath = path.resolve(options.output)
        await mkdir(path.dirname(outputPath), { recursive: true })
        await writeFile(outputPath, json, 'utf8')
    }
    process.stdout.write(json)
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
    main().catch((error) => {
        process.stderr.write(`${error.stack ?? error}${os.EOL}`)
        process.exitCode = 1
    })
}
