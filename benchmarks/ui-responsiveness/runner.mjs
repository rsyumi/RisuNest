import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { createReadStream } from 'node:fs'
import { mkdtemp, rm, stat, writeFile } from 'node:fs/promises'
import http from 'node:http'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'

const HOST = '127.0.0.1'
const TEMP_PREFIX = 'risunest-ui-responsiveness-'
const repositoryRoot = path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    '..',
    '..',
)

class CdpClient {
    constructor(url) {
        this.socket = new WebSocket(url)
        this.nextId = 1
        this.pending = new Map()
        this.listeners = new Map()
    }

    async connect() {
        await onceEvent(this.socket, 'open')
        this.socket.addEventListener('message', (event) => {
            const message = JSON.parse(
                typeof event.data === 'string'
                    ? event.data
                    : Buffer.from(event.data).toString(),
            )
            if (message.id) {
                const pending = this.pending.get(message.id)
                if (!pending) return
                this.pending.delete(message.id)
                if (message.error)
                    pending.reject(new Error(message.error.message))
                else pending.resolve(message.result)
                return
            }
            for (const listener of this.listeners.get(message.method) ?? [])
                listener(message.params)
        })
        this.socket.addEventListener('close', () => {
            for (const pending of this.pending.values())
                pending.reject(new Error('CDP connection closed'))
            this.pending.clear()
        })
    }

    call(method, params = {}, timeoutMs = 30_000) {
        const id = this.nextId++
        return new Promise((resolve, reject) => {
            const timeout = setTimeout(() => {
                this.pending.delete(id)
                reject(new Error(`CDP command timed out: ${method}`))
            }, timeoutMs)
            this.pending.set(id, {
                resolve: (value) => {
                    clearTimeout(timeout)
                    resolve(value)
                },
                reject: (error) => {
                    clearTimeout(timeout)
                    reject(error)
                },
            })
            this.socket.send(JSON.stringify({ id, method, params }))
        })
    }

    on(method, listener) {
        const listeners = this.listeners.get(method) ?? []
        listeners.push(listener)
        this.listeners.set(method, listeners)
    }

    close() {
        this.socket.close()
    }
}

function onceEvent(target, name) {
    return new Promise((resolve, reject) => {
        target.addEventListener(name, resolve, { once: true })
        target.addEventListener('error', reject, { once: true })
    })
}

async function freePort() {
    const server = net.createServer()
    server.listen(0, HOST)
    await once(server, 'listening')
    const address = server.address()
    const port = typeof address === 'object' && address ? address.port : null
    server.close()
    await once(server, 'close')
    if (!port) throw new Error('Failed to reserve a local port')
    return port
}

async function findBrowser() {
    const candidates = [
        process.env.RISUNEST_UI_BENCH_BROWSER,
        'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
        'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
        'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
        'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
    ].filter(Boolean)
    for (const candidate of candidates) {
        try {
            await stat(candidate)
            return candidate
        } catch {}
    }
    throw new Error('No supported Edge or Chrome executable was found')
}

function serve(directory) {
    return http.createServer(async (request, response) => {
        const requestPath = decodeURIComponent(
            new URL(request.url, `http://${HOST}`).pathname,
        )
        const relativePath =
            requestPath === '/'
                ? 'benchmarks/ui-responsiveness/index.html'
                : requestPath.slice(1)
        const filePath = path.resolve(directory, relativePath)
        if (!filePath.startsWith(`${path.resolve(directory)}${path.sep}`)) {
            response.writeHead(403).end()
            return
        }
        try {
            const fileStat = await stat(filePath)
            if (!fileStat.isFile()) throw new Error('Not a file')
            response.setHeader('Cache-Control', 'no-store')
            const contentTypes = new Map([
                ['.css', 'text/css'],
                ['.html', 'text/html'],
                ['.js', 'text/javascript'],
                ['.json', 'application/json'],
                ['.wasm', 'application/wasm'],
            ])
            response.setHeader(
                'Content-Type',
                contentTypes.get(path.extname(filePath)) ??
                    'application/octet-stream',
            )
            createReadStream(filePath).pipe(response)
        } catch {
            response.writeHead(404).end()
        }
    })
}

async function waitForPage(debugPort, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        try {
            const response = await fetch(
                `http://${HOST}:${debugPort}/json/list`,
            )
            const targets = await response.json()
            const page = targets.find(
                (target) =>
                    target.type === 'page' && target.webSocketDebuggerUrl,
            )
            if (page) return page
        } catch {}
        await new Promise((resolve) => setTimeout(resolve, 50))
    }
    throw new Error('Browser CDP endpoint did not become ready')
}

async function evaluate(client, expression) {
    const response = await client.call('Runtime.evaluate', {
        expression,
        awaitPromise: true,
        returnByValue: true,
    })
    if (response.exceptionDetails) {
        throw new Error(
            response.exceptionDetails.exception?.description ??
                response.exceptionDetails.text,
        )
    }
    return response.result.value
}

async function waitForReady(client, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        if (
            await evaluate(client, 'Boolean(globalThis.__risuUiBenchmarkReady)')
        )
            return
        await new Promise((resolve) => setTimeout(resolve, 25))
    }
    const diagnostics = await evaluate(
        client,
        `(() => {
        const rows = [...document.querySelectorAll('[data-chat-render-key][data-chat-index]')]
        const indices = rows.map((row) => Number(row.getAttribute('data-chat-index'))).filter(Number.isFinite)
        return {
            hasRoot: Boolean(document.querySelector('#ui-benchmark-root')),
            rowCount: rows.length,
            firstIndex: indices.length ? Math.min(...indices) : -1,
            lastIndex: indices.length ? Math.max(...indices) : -1,
            codeCount: document.querySelectorAll('[data-chat-index] pre code').length,
            tableCount: document.querySelectorAll('[data-chat-index] table').length,
            loadError: globalThis.__risuUiBenchmarkLoadError ?? null,
        }
    })()`,
    )
    throw new Error(
        `Synthetic UI benchmark did not become ready: ${JSON.stringify(diagnostics)}`,
    )
}

async function runProcess(command, argumentsList, options) {
    const child = spawn(command, argumentsList, {
        ...options,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stderr = ''
    child.stderr.on('data', (chunk) => {
        stderr += chunk.toString()
    })
    child.stdout.resume()
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0)
        throw new Error(
            `${path.basename(command)} failed (${exitCode}): ${stderr.slice(-4_000)}`,
        )
}

async function stopProcess(child) {
    if (!child || child.exitCode !== null) return
    child.kill()
    await Promise.race([
        once(child, 'exit'),
        new Promise((resolve) => setTimeout(resolve, 5_000)),
    ])
    if (child.exitCode !== null) return
    const killer = spawn(
        'taskkill.exe',
        ['/PID', String(child.pid), '/T', '/F'],
        {
            windowsHide: true,
            stdio: 'ignore',
        },
    )
    await once(killer, 'exit')
}

function assertSafeProfile(directory) {
    const resolved = path.resolve(directory)
    if (
        !resolved.startsWith(`${path.resolve(os.tmpdir())}${path.sep}`) ||
        !path.basename(resolved).startsWith(TEMP_PREFIX)
    ) {
        throw new Error(
            'Refusing to remove a profile outside the benchmark-owned temp path',
        )
    }
}

function percentile(values, fraction) {
    const sorted = [...values].sort((left, right) => left - right)
    return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)]
}

function summarize(samples, key) {
    const values = samples.map((sample) => sample[key])
    return {
        p50Ms: percentile(values, 0.5),
        p95Ms: percentile(values, 0.95),
        maxMs: Math.max(...values),
    }
}

async function main() {
    const args = process.argv.slice(2)
    const sourceIndex = args.indexOf('--source')
    const samplesIndex = args.indexOf('--samples')
    const screenshotRequested = args.includes('--screenshot')
    const source = sourceIndex === -1 ? 'baseline' : args[sourceIndex + 1]
    const sampleCount = samplesIndex === -1 ? 5 : Number(args[samplesIndex + 1])
    if (!['baseline', 'candidate'].includes(source))
        throw new Error('--source must be baseline or candidate')
    if (!Number.isSafeInteger(sampleCount) || sampleCount < 3)
        throw new Error('--samples must be at least 3')

    const benchmarkDirectory = path.join(
        repositoryRoot,
        'benchmarks',
        'ui-responsiveness',
    )
    const distDirectory = await mkdtemp(
        path.join(benchmarkDirectory, `.run-${source}-`),
    )
    let profile
    let server
    let browser
    let client
    try {
        await runProcess(
            process.execPath,
            [
                path.join(
                    repositoryRoot,
                    'node_modules',
                    'vite',
                    'bin',
                    'vite.js',
                ),
                'build',
                '--mode',
                'agent',
                '--config',
                'benchmarks/ui-responsiveness/vite.config.ts',
            ],
            {
                cwd: repositoryRoot,
                env: {
                    ...process.env,
                    RISUNEST_UI_BENCH_SOURCE: source,
                    RISUNEST_UI_BENCH_OUT_DIR: distDirectory,
                },
            },
        )
        process.stderr.write('ui-benchmark: build complete\n')
        const [serverPort, debugPort, browserExecutable] = await Promise.all([
            freePort(),
            freePort(),
            findBrowser(),
        ])
        profile = await mkdtemp(path.join(os.tmpdir(), TEMP_PREFIX))
        server = serve(distDirectory)
        server.listen(serverPort, HOST)
        await once(server, 'listening')
        browser = spawn(
            browserExecutable,
            [
                '--headless=new',
                `--remote-debugging-port=${debugPort}`,
                `--user-data-dir=${profile}`,
                '--disable-background-networking',
                '--disable-component-update',
                '--disable-default-apps',
                '--no-first-run',
                'about:blank',
            ],
            { windowsHide: true, stdio: 'ignore' },
        )
        process.stderr.write('ui-benchmark: browser launched\n')
        const page = await waitForPage(debugPort, 30_000)
        process.stderr.write('ui-benchmark: CDP connected\n')
        client = new CdpClient(page.webSocketDebuggerUrl)
        await client.connect()
        await client.call('Runtime.enable')
        await client.call('Page.enable')
        await client.call('Network.enable')
        await client.call('Network.setCacheDisabled', { cacheDisabled: true })
        await client.call('Network.setBlockedURLs', {
            urls: REALM_BLOCKED_URL_PATTERNS,
        })

        let blockedExternalRequestCount = 0
        let externalResponseCount = 0
        let interceptionError = null
        const isExternalHttpUrl = (value) => {
            try {
                const target = new URL(value)
                return (
                    (target.protocol === 'http:' ||
                        target.protocol === 'https:') &&
                    target.hostname !== HOST &&
                    target.hostname !== 'localhost'
                )
            } catch {
                return false
            }
        }
        client.on('Fetch.requestPaused', ({ requestId, request }) => {
            const method = isExternalHttpUrl(request.url)
                ? 'Fetch.failRequest'
                : 'Fetch.continueRequest'
            const params =
                method === 'Fetch.failRequest'
                    ? { requestId, errorReason: 'BlockedByClient' }
                    : { requestId }
            if (method === 'Fetch.failRequest') blockedExternalRequestCount += 1
            void client.call(method, params).catch((error) => {
                interceptionError = error
            })
        })
        client.on('Network.responseReceived', ({ response }) => {
            if (isExternalHttpUrl(response.url)) externalResponseCount += 1
        })
        await client.call('Fetch.enable', {
            patterns: [{ urlPattern: '*', requestStage: 'Request' }],
        })

        const pageUrl = `http://${HOST}:${serverPort}/benchmarks/ui-responsiveness/index.html`
        const samples = []
        for (let index = 0; index < sampleCount; index += 1) {
            await client.call('Page.navigate', {
                url: `${pageUrl}?sample=${index}`,
            })
            await waitForReady(client, 30_000)
            const result = await evaluate(
                client,
                'globalThis.__risuUiRunNavigation()',
            )
            if (!result.validOutput)
                throw new Error(
                    `Synthetic output validation failed in sample ${index}`,
                )
            samples.push(result)
            if (screenshotRequested && index === 0) {
                const screenshot = await client.call('Page.captureScreenshot', {
                    format: 'png',
                })
                await writeFile(
                    path.join(benchmarkDirectory, 'synthetic-layout.png'),
                    Buffer.from(screenshot.data, 'base64'),
                )
            }
            process.stderr.write(
                `ui-benchmark: sample ${index + 1}/${sampleCount} complete\n`,
            )
        }
        if (interceptionError) throw interceptionError
        if (externalResponseCount !== 0)
            throw new Error('Benchmark received an external response')
        const fixtureHashes = new Set(
            samples.map((sample) => sample.fixture.contentHash),
        )
        const firstSignatures = new Set(
            samples.map(
                (sample) => sample.verification.firstChat.outputSignature,
            ),
        )
        const navigationSignatures = new Set(
            samples.map(
                (sample) => sample.verification.navigation.outputSignature,
            ),
        )
        if (
            fixtureHashes.size !== 1 ||
            firstSignatures.size !== 1 ||
            navigationSignatures.size !== 1
        ) {
            throw new Error(
                'Fixture or rendered output signatures differed between samples',
            )
        }

        process.stdout.write(
            `${JSON.stringify(
                {
                    schemaVersion: 1,
                    source,
                    baselineRevision: BASELINE_REVISION,
                    browser: path.basename(browserExecutable),
                    sampleCount,
                    blockedExternalRequestCount,
                    fixture: samples[0].fixture,
                    summary: {
                        totalReadyDuration: summarize(
                            samples,
                            'totalReadyDurationMs',
                        ),
                        moduleLoadToFirstChatReadyDuration: summarize(
                            samples,
                            'moduleLoadToFirstChatReadyDurationMs',
                        ),
                        firstChatDuration: summarize(
                            samples,
                            'firstChatDurationMs',
                        ),
                        navigationDuration: summarize(
                            samples,
                            'navigationDurationMs',
                        ),
                        longTaskMaxMs: Math.max(
                            ...samples.map((sample) => sample.longTasks.maxMs),
                        ),
                        longTaskCount: samples.reduce(
                            (sum, sample) => sum + sample.longTasks.count,
                            0,
                        ),
                        longTaskTotalMs: samples.reduce(
                            (sum, sample) => sum + sample.longTasks.totalMs,
                            0,
                        ),
                        animationFrameGapMaxMs: Math.max(
                            ...samples.map(
                                (sample) => sample.animationFrameGaps.maxMs,
                            ),
                        ),
                        animationFrameGapCount: samples.reduce(
                            (sum, sample) =>
                                sum + sample.animationFrameGaps.count,
                            0,
                        ),
                        animationFrameGapTotalMs: samples.reduce(
                            (sum, sample) =>
                                sum + sample.animationFrameGaps.totalMs,
                            0,
                        ),
                    },
                    samples,
                },
                null,
                2,
            )}\n`,
        )
    } finally {
        client?.close()
        server?.close()
        await stopProcess(browser)
        if (profile) {
            assertSafeProfile(profile)
            await rm(profile, {
                recursive: true,
                force: true,
                maxRetries: 10,
                retryDelay: 200,
            })
        }
        const resolvedDist = path.resolve(distDirectory)
        if (
            !resolvedDist.startsWith(
                `${path.resolve(benchmarkDirectory)}${path.sep}`,
            ) ||
            !path.basename(resolvedDist).startsWith(`.run-${source}-`)
        ) {
            throw new Error(
                'Refusing to remove an unexpected benchmark build directory',
            )
        }
        await rm(resolvedDist, {
            recursive: true,
            force: true,
            maxRetries: 10,
            retryDelay: 200,
        })
    }
}

const BASELINE_REVISION = '793930f17731547c7e9f2fe7bf0be6f6a5bbd462'

main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`)
    process.exitCode = 1
})
