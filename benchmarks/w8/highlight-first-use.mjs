import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { createReadStream } from 'node:fs'
import { mkdtemp, rm, stat } from 'node:fs/promises'
import http from 'node:http'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'
import { percentileNearestRank } from './bundle-gates.mjs'

const HOST = '127.0.0.1'
const TEMP_PREFIX = 'risunest-w8-highlight-'

class CdpClient {
    constructor(url) {
        this.socket = new WebSocket(url)
        this.nextId = 1
        this.pending = new Map()
    }

    async connect() {
        await onceEvent(this.socket, 'open')
        this.socket.addEventListener('message', (event) => {
            const message = JSON.parse(typeof event.data === 'string' ? event.data : Buffer.from(event.data).toString())
            if (!message.id) return
            const pending = this.pending.get(message.id)
            if (!pending) return
            this.pending.delete(message.id)
            if (message.error) pending.reject(new Error(message.error.message))
            else pending.resolve(message.result)
        })
    }

    call(method, params = {}) {
        const id = this.nextId++
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject })
            this.socket.send(JSON.stringify({ id, method, params }))
        })
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

async function getFreePort() {
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

function browserExecutable() {
    const candidates = [
        process.env.RISUNEST_W8_BROWSER,
        'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
        'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
        'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
        'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
    ].filter(Boolean)
    return candidates
}

async function findBrowser() {
    for (const candidate of browserExecutable()) {
        try {
            await stat(candidate)
            return candidate
        } catch {}
    }
    throw new Error('No supported Edge or Chrome executable was found')
}

export async function discoverBrowserThenCreateProfile({
    discoverBrowser = findBrowser,
    createProfile = () => mkdtemp(path.join(os.tmpdir(), TEMP_PREFIX)),
} = {}) {
    const executable = await discoverBrowser()
    const profile = await createProfile()
    return { executable, profile }
}

function serve(directory) {
    const server = http.createServer(async (request, response) => {
        const requestPath = decodeURIComponent(new URL(request.url, `http://${HOST}`).pathname)
        const relativePath = requestPath === '/' ? 'benchmarks/w8/highlight-first-use.html' : requestPath.slice(1)
        const filePath = path.resolve(directory, relativePath)
        if (!filePath.startsWith(path.resolve(directory) + path.sep)) {
            response.writeHead(403).end()
            return
        }
        try {
            const fileStat = await stat(filePath)
            if (!fileStat.isFile()) throw new Error('Not a file')
            response.setHeader('Cache-Control', 'no-store')
            response.setHeader('Content-Type', filePath.endsWith('.js') ? 'text/javascript' : 'text/html')
            createReadStream(filePath).pipe(response)
        } catch {
            response.writeHead(404).end()
        }
    })
    return server
}

async function waitForPage(debugPort, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        try {
            const response = await fetch(`http://${HOST}:${debugPort}/json/list`)
            const targets = await response.json()
            const page = targets.find((target) => target.type === 'page' && target.webSocketDebuggerUrl)
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
        throw new Error(response.exceptionDetails.exception?.description ?? response.exceptionDetails.text)
    }
    return response.result.value
}

async function waitForReady(client, timeoutMs, sampleIndex) {
    const deadline = Date.now() + timeoutMs
    const expectedSearch = JSON.stringify(`?sample=${sampleIndex}`)
    while (Date.now() < deadline) {
        if (await evaluate(client, `location.search === ${expectedSearch} && Boolean(globalThis.__risuW8HighlightReady)`)) return
        await new Promise((resolve) => setTimeout(resolve, 25))
    }
    throw new Error('Highlight benchmark page did not become ready')
}

async function stopProcess(child) {
    if (!child || child.exitCode !== null) return
    child.kill()
    await Promise.race([once(child, 'exit'), new Promise((resolve) => setTimeout(resolve, 5_000))])
    if (child.exitCode !== null) return
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
        windowsHide: true,
        stdio: 'ignore',
    })
    await once(killer, 'exit')
}

function assertSafeProfile(directory) {
    const resolved = path.resolve(directory)
    if (!resolved.startsWith(path.resolve(os.tmpdir()) + path.sep) || !path.basename(resolved).startsWith(TEMP_PREFIX)) {
        throw new Error(`Refusing to remove unexpected profile: ${resolved}`)
    }
}

async function main() {
    const args = process.argv.slice(2)
    const distIndex = args.indexOf('--dist')
    const samplesIndex = args.indexOf('--samples')
    if (distIndex === -1 || !args[distIndex + 1]) throw new Error('--dist is required')
    const distDirectory = path.resolve(args[distIndex + 1])
    const sampleCount = samplesIndex === -1 ? 20 : Number(args[samplesIndex + 1])
    if (!Number.isSafeInteger(sampleCount) || sampleCount < 2) throw new Error('--samples must be at least 2')

    const [serverPort, debugPort] = await Promise.all([getFreePort(), getFreePort()])
    const { executable, profile } = await discoverBrowserThenCreateProfile()
    let server
    let browser
    let client
    try {
        server = serve(distDirectory)
        server.listen(serverPort, HOST)
        await once(server, 'listening')
        const pageUrl = `http://${HOST}:${serverPort}/benchmarks/w8/highlight-first-use.html`
        browser = spawn(executable, [
            '--headless=new',
            `--remote-debugging-port=${debugPort}`,
            `--user-data-dir=${profile}`,
            '--disable-background-networking',
            '--disable-component-update',
            '--disable-default-apps',
            '--no-first-run',
            pageUrl,
        ], { windowsHide: true, stdio: 'ignore' })
        const page = await waitForPage(debugPort, 30_000)
        client = new CdpClient(page.webSocketDebuggerUrl)
        await client.connect()
        await client.call('Runtime.enable')
        await client.call('Page.enable')
        await client.call('Network.enable')
        await client.call('Network.setCacheDisabled', { cacheDisabled: true })
        // 이 엔트리는 ParseMarkdown만 묶어 Realm에 닿을 수 없지만, 다른 CDP 러너와
        // 같은 세션 차단을 걸어 두어 "모든 벤치마크 러너가 Realm을 막는다"를 유지한다.
        await client.call('Network.setBlockedURLs', { urls: REALM_BLOCKED_URL_PATTERNS })

        const samples = []
        for (let index = 0; index < sampleCount; index += 1) {
            await client.call('Page.navigate', { url: `${pageUrl}?sample=${index}` })
            await waitForReady(client, 30_000, index)
            const result = await evaluate(client, 'globalThis.__risuW8MeasureHighlight()')
            if (!result.validOutput) throw new Error(`Invalid highlighted output in sample ${index}`)
            samples.push(result)
        }
        const durations = samples.map((sample) => sample.durationMs)
        process.stdout.write(`${JSON.stringify({
            schemaVersion: 1,
            browser: executable,
            sampleCount,
            p50Ms: percentileNearestRank(durations, 0.5),
            p95Ms: percentileNearestRank(durations, 0.95),
            maxMs: Math.max(...durations),
            samples,
        }, null, 2)}\n`)
    } finally {
        client?.close()
        server?.close()
        await stopProcess(browser)
        assertSafeProfile(profile)
        await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 })
    }
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
    main().catch((error) => {
        process.stderr.write(`${error.stack ?? error}\n`)
        process.exitCode = 1
    })
}
