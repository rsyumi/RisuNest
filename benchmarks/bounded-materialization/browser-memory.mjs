import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdtemp, rm, readFile } from 'node:fs/promises'
import net from 'node:net'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { transformWithEsbuild } from 'vite'
import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds))

class Cdp {
    nextId = 1
    pending = new Map()

    async connect(url) {
        this.socket = new WebSocket(url)
        await once(this.socket, 'open')
        this.socket.addEventListener('message', (event) => {
            const message = JSON.parse(String(event.data))
            const pending = this.pending.get(message.id)
            if (!pending) return
            this.pending.delete(message.id)
            message.error
                ? pending.reject(new Error(message.error.message))
                : pending.resolve(message.result)
        })
    }

    call(method, params = {}) {
        const id = this.nextId++
        return new Promise((resolve, reject) => {
            this.pending.set(id, { resolve, reject })
            this.socket.send(JSON.stringify({ id, method, params }))
        })
    }

    close() { this.socket?.close() }
}

async function freePort() {
    const server = net.createServer()
    server.listen(0, '127.0.0.1')
    await once(server, 'listening')
    const address = server.address()
    const port = typeof address === 'object' && address ? address.port : 0
    server.close()
    await once(server, 'close')
    return port
}

async function endpoint(port, route) {
    for (let attempt = 0; attempt < 100; attempt += 1) {
        try {
            const response = await fetch(`http://127.0.0.1:${port}${route}`)
            if (response.ok) return response.json()
        } catch {}
        await delay(100)
    }
    throw new Error(`CDP endpoint did not become ready: ${route}`)
}

async function evaluate(page, expression) {
    const result = await page.call('Runtime.evaluate', {
        expression,
        awaitPromise: true,
        returnByValue: true,
    })
    if (result.exceptionDetails) {
        throw new Error(result.exceptionDetails.exception?.description ?? 'evaluation failed')
    }
    return result.result.value
}

async function processTree(browser) {
    const information = await browser.call('SystemInfo.getProcessInfo')
    const ids = [...new Set(information.processInfo.map(({ id }) => id))]
        .filter(Number.isSafeInteger)
    const command = [
        `$ids=@(${ids.join(',')})`,
        '$rows=Get-Process -Id $ids -ErrorAction SilentlyContinue | ForEach-Object {',
        '  [pscustomobject]@{pid=$_.Id;workingSetBytes=$_.WorkingSet64;privateBytes=$_.PrivateMemorySize64}',
        '}',
        '$rows | ConvertTo-Json -Compress',
    ].join(';')
    const child = spawn('powershell.exe', [
        '-NoProfile', '-NonInteractive', '-Command', command,
    ], { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] })
    let stdout = ''
    child.stdout.on('data', (chunk) => { stdout += chunk })
    const [code] = await once(child, 'exit')
    if (code !== 0) throw new Error('process memory query failed')
    const parsed = stdout.trim() ? JSON.parse(stdout) : []
    const rows = Array.isArray(parsed) ? parsed : [parsed]
    return {
        workingSetBytes: rows.reduce((sum, row) => sum + row.workingSetBytes, 0),
        privateBytes: rows.reduce((sum, row) => sum + row.privateBytes, 0),
        processCount: rows.length,
    }
}

async function sample(page, browser, label) {
    const heap = await page.call('Runtime.getHeapUsage')
    return {
        label,
        webViewHeapUsedBytes: heap.usedSize,
        webViewHeapTotalBytes: heap.totalSize,
        processTree: await processTree(browser),
    }
}

const edge = 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe'
const profile = await mkdtemp(path.join(os.tmpdir(), 'risunest-bounded-memory-'))
const port = await freePort()
const fixtureServer = http.createServer((_request, response) => { response.end('<!doctype html><body></body>') })
fixtureServer.listen(0, '127.0.0.1')
await once(fixtureServer, 'listening')
const fixtureUrl = `http://127.0.0.1:${fixtureServer.address().port}/`
const child = spawn(edge, [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--disable-extensions',
    '--disable-sync',
    '--disable-features=msImplicitSignin',
    '--js-flags=--expose-gc',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    fixtureUrl,
], { windowsHide: true, stdio: 'ignore' })

const page = new Cdp()
const browser = new Cdp()
try {
    const version = await endpoint(port, '/json/version')
    const targets = await endpoint(port, '/json/list')
    const target = targets.find(({ type, url }) => type === 'page' && url === fixtureUrl)
    if (!target) throw new Error('browser page target is unavailable')
    await page.connect(target.webSocketDebuggerUrl)
    await browser.connect(version.webSocketDebuggerUrl)
    await page.call('Runtime.enable')
    await page.call('Network.enable')
    await page.call('Network.setBlockedURLs', { urls: REALM_BLOCKED_URL_PATTERNS })
    for (let attempt = 0; attempt < 100; attempt++) {
        if (await evaluate(page, 'globalThis.isSecureContext && document.readyState === "complete"')) break
        await delay(50)
    }
    const source = await readFile(new URL('../../src/ts/plugins/apiV3/factory.ts', import.meta.url), 'utf8')
    const compiled = await transformWithEsbuild(source, 'factory.ts', { format: 'iife', globalName: 'SandboxModule', define: { 'import.meta.env.DEV': 'false' } })
    await evaluate(page, compiled.code)
    await evaluate(page, `(() => {
        window.logical = { maximumParentChunkBytes: 0 };
        const frame = document.createElement('iframe');
        document.body.append(frame);
        window.fixtureHost = new SandboxModule.SandboxHost({
            _getPropertiesForInitialization: () => ({ apiVersion: '3.0', list: ['apiVersion'] }),
            _getAliases: () => ({}),
            snapshot: () => ({
                __type: 'IFRAME_OBJECT_STREAM',
                value: new ReadableStream({
                    start(controller) { window.fixtureController = controller; }
                }, { highWaterMark: 1 }),
            }),
            assembled: (metrics) => { window.assembledMetrics = metrics; },
        });
        window.fixtureHost.run(frame, 'window.snapshotPromise = risuai.snapshot().then(value => { window.snapshot = value; return risuai.assembled({ messageCount: value.characters[0].chats[0].message.length, characterCount: value.characters.length, jsonBytes: new TextEncoder().encode(JSON.stringify(value)).byteLength }); });', 'synthetic-memory');
    })()`)
    for (let attempt = 0; attempt < 100; attempt++) {
        if (await evaluate(page, 'Boolean(window.fixtureController)')) break
        await delay(50)
    }
    await evaluate(page, `(() => {
        window.fixtureController.enqueue({ type: 'arrayStart', key: 'characters' });
        window.fixtureController.enqueue({ type: 'characterStart', key: 'characters', value: { chaId: 'synthetic-character', name: 'Synthetic' } });
        window.fixtureController.enqueue({ type: 'conversationStart', key: 'characters', value: { id: 'synthetic-conversation', name: 'Synthetic' } });
    })()`)
    await evaluate(page, 'globalThis.gc?.()')
    const samples = [await sample(page, browser, 'baseline')]
    for (let start = 0; start < 16384; start += 512) {
        await evaluate(page, `(async () => {
            for (let index = ${start}; index < ${start + 512}; index++) {
                while (window.fixtureController.desiredSize <= 0) await new Promise(resolve => setTimeout(resolve, 0));
                const chunk = { type: 'message', key: 'characters', value: {
                    role: index % 2 ? 'char' : 'user', data: String(index).padStart(6, '0') + 'x'.repeat(2042), chatId: 'synthetic-' + index,
                } };
                window.logical.maximumParentChunkBytes = Math.max(window.logical.maximumParentChunkBytes, new TextEncoder().encode(JSON.stringify(chunk)).byteLength);
                window.fixtureController.enqueue(chunk);
            }
        })()`)
        samples.push(await sample(page, browser, `after-${start + 512}-messages`))
    }
    await evaluate(page, 'window.fixtureController.close()')
    for (let attempt = 0; attempt < 100; attempt++) {
        if (await evaluate(page, 'window.assembledMetrics?.messageCount === 16384')) break
        await delay(50)
    }
    if (!await evaluate(page, 'window.assembledMetrics?.messageCount === 16384')) throw new Error('Guest assembly did not finish')
    await evaluate(page, 'globalThis.gc?.()')
    samples.push(await sample(page, browser, 'retained-after-gc'))
    const logical = await evaluate(page, `(() => {
        return {
            ...window.logical,
            iframeCharacterCount: window.assembledMetrics.characterCount,
            iframeRetainedJsonBytes: window.assembledMetrics.jsonBytes,
        };
    })()`)
    const peak = (field) => Math.max(...samples.map((entry) =>
        field === 'heap'
            ? entry.webViewHeapUsedBytes
            : entry.processTree.workingSetBytes))
    console.log(JSON.stringify({
        fixture: { characters: 1, conversationsPerCharacter: 1, messagesPerConversation: 16384,
            messageBodyBytes: 2048 },
        implementation: 'Production SandboxHost and iframe assembly, synthetic bounded source (storage reads excluded)',
        logicalDomains: logical,
        peaks: {
            webViewHeapUsedBytes: peak('heap'),
            totalProcessTreeWorkingSetBytes: peak('tree'),
        },
        samples,
    }, null, 2))
} finally {
    await browser.call('Browser.close').catch(() => {})
    fixtureServer.close()
    page.close()
    browser.close()
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
        windowsHide: true,
        stdio: 'ignore',
    })
    if (killer.exitCode === null) await once(killer, 'exit')
    if (child.exitCode === null) await Promise.race([once(child, 'exit'), delay(5_000)])
    if (path.dirname(path.resolve(profile)) !== path.resolve(os.tmpdir())) throw new Error('Unexpected profile directory');
    await rm(profile, { recursive: true, force: true, maxRetries: 20, retryDelay: 500 })
        .catch(() => console.error('Synthetic browser profile remains locked:', profile))
}
process.exit(0)
