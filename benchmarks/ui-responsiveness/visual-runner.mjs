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
const PROFILE_PREFIX = 'risunest-ui-visual-'
const repositoryRoot = path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    '..',
    '..',
)
const benchmarkDirectory = path.join(
    repositoryRoot,
    'benchmarks',
    'ui-responsiveness',
)
const researchDirectory = path.join(repositoryRoot, 'docs', 'research')

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
                ? 'benchmarks/ui-responsiveness/visual-index.html'
                : requestPath.slice(1)
        const filePath = path.resolve(directory, relativePath)
        if (!filePath.startsWith(`${path.resolve(directory)}${path.sep}`)) {
            response.writeHead(403).end()
            return
        }
        try {
            const fileStat = await stat(filePath)
            if (!fileStat.isFile()) throw new Error('Not a file')
            const contentTypes = new Map([
                ['.css', 'text/css'],
                ['.html', 'text/html'],
                ['.js', 'text/javascript'],
            ])
            response.setHeader('Cache-Control', 'no-store')
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

async function waitForPage(debugPort) {
    const deadline = Date.now() + 30_000
    while (Date.now() < deadline) {
        try {
            const response = await fetch(
                `http://${HOST}:${debugPort}/json/list`,
            )
            const page = (await response.json()).find(
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

async function waitForReady(client) {
    const deadline = Date.now() + 30_000
    while (Date.now() < deadline) {
        if (await evaluate(client, 'Boolean(globalThis.__risuUiVisualReady)'))
            return
        await new Promise((resolve) => setTimeout(resolve, 25))
    }
    throw new Error('Synthetic visual harness did not become ready')
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

function assertSafeOwnedPath(directory, parent, prefix) {
    const resolved = path.resolve(directory)
    if (
        !resolved.startsWith(`${path.resolve(parent)}${path.sep}`) ||
        !path.basename(resolved).startsWith(prefix)
    ) {
        throw new Error(
            'Refusing to remove a path outside the visual harness ownership boundary',
        )
    }
}

async function captureCase(client, pageUrl, name, width, height) {
    await client.call('Emulation.setDeviceMetricsOverride', {
        width,
        height,
        deviceScaleFactor: 1,
        mobile: false,
    })
    await client.call('Page.navigate', { url: `${pageUrl}?case=${name}` })
    await waitForReady(client)
    const controlCenter = await evaluate(
        client,
        `(() => {
        const rect = document.querySelector('[data-synthetic-underlying-control]').getBoundingClientRect()
        return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 }
    })()`,
    )
    await client.call('Input.dispatchMouseEvent', {
        type: 'mousePressed',
        x: controlCenter.x,
        y: controlCenter.y,
        button: 'left',
        clickCount: 1,
    })
    await client.call('Input.dispatchMouseEvent', {
        type: 'mouseReleased',
        x: controlCenter.x,
        y: controlCenter.y,
        button: 'left',
        clickCount: 1,
    })
    const verification = await evaluate(
        client,
        'globalThis.__risuUiVisualVerify()',
    )
    if (!verification.validOutput)
        throw new Error(`Synthetic ${name} visual assertions failed`)
    const screenshot = await client.call('Page.captureScreenshot', {
        format: 'png',
    })
    await writeFile(
        path.join(
            researchDirectory,
            `ui-responsiveness-${name}-2026-09-08.png`,
        ),
        Buffer.from(screenshot.data, 'base64'),
    )
    return { viewport: { width, height }, ...verification }
}

async function main() {
    const distDirectory = await mkdtemp(
        path.join(benchmarkDirectory, '.visual-run-'),
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
                'benchmarks/ui-responsiveness/visual-vite.config.ts',
            ],
            {
                cwd: repositoryRoot,
                env: {
                    ...process.env,
                    RISUNEST_UI_VISUAL_OUT_DIR: distDirectory,
                },
            },
        )
        const [serverPort, debugPort, browserExecutable] = await Promise.all([
            freePort(),
            freePort(),
            findBrowser(),
        ])
        profile = await mkdtemp(path.join(os.tmpdir(), PROFILE_PREFIX))
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
        const page = await waitForPage(debugPort)
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
            const external = isExternalHttpUrl(request.url)
            if (external) blockedExternalRequestCount += 1
            void client.call(
                external ? 'Fetch.failRequest' : 'Fetch.continueRequest',
                external
                    ? { requestId, errorReason: 'BlockedByClient' }
                    : { requestId },
            )
        })
        client.on('Network.responseReceived', ({ response }) => {
            if (isExternalHttpUrl(response.url)) externalResponseCount += 1
        })
        await client.call('Fetch.enable', {
            patterns: [{ urlPattern: '*', requestStage: 'Request' }],
        })

        const pageUrl = `http://${HOST}:${serverPort}/benchmarks/ui-responsiveness/visual-index.html`
        const desktop = await captureCase(
            client,
            pageUrl,
            'overlay-desktop',
            1100,
            760,
        )
        const narrow = await captureCase(
            client,
            pageUrl,
            'overlay-narrow',
            360,
            680,
        )
        if (externalResponseCount !== 0)
            throw new Error('Visual harness received an external response')
        process.stdout.write(
            `${JSON.stringify(
                {
                    schemaVersion: 1,
                    scope: 'synthetic-browser-production-chat-screen-navigation-overlay',
                    browser: path.basename(browserExecutable),
                    blockedExternalRequestCount,
                    externalResponseCount,
                    cases: { desktop, narrow },
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
            assertSafeOwnedPath(profile, os.tmpdir(), PROFILE_PREFIX)
            await rm(profile, {
                recursive: true,
                force: true,
                maxRetries: 10,
                retryDelay: 200,
            })
        }
        assertSafeOwnedPath(distDirectory, benchmarkDirectory, '.visual-run-')
        await rm(distDirectory, {
            recursive: true,
            force: true,
            maxRetries: 10,
            retryDelay: 200,
        })
    }
}

main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`)
    process.exitCode = 1
})
