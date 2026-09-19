import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdtemp, rm } from 'node:fs/promises'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'

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
const child = spawn(edge, [
    '--headless=new',
    '--disable-gpu',
    '--no-first-run',
    '--js-flags=--expose-gc',
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    'about:blank',
], { windowsHide: true, stdio: 'ignore' })

const page = new Cdp()
const browser = new Cdp()
try {
    const version = await endpoint(port, '/json/version')
    const targets = await endpoint(port, '/json/list')
    const target = targets.find(({ type }) => type === 'page')
    if (!target) throw new Error('browser page target is unavailable')
    await page.connect(target.webSocketDebuggerUrl)
    await browser.connect(version.webSocketDebuggerUrl)
    await page.call('Runtime.enable')
    await evaluate(page, `(() => {
        const iframe = document.createElement('iframe');
        document.body.append(iframe);
        const target = iframe.contentWindow;
        target.snapshot = { characters: [] };
        target.addEventListener('message', (event) => {
            if (event.data?.type !== 'character') return;
            target.snapshot.characters.push(event.data.value);
            event.ports[0].postMessage('accepted');
        });
        window.transferCharacter = (value) => new Promise((resolve) => {
            const channel = new MessageChannel();
            channel.port1.onmessage = resolve;
            target.postMessage({ type: 'character', value }, '*', [channel.port2]);
        });
        window.logical = { parentRetainedBytes: 0, maximumParentChunkBytes: 0 };
    })()`)
    await evaluate(page, 'globalThis.gc?.()')
    const samples = [await sample(page, browser, 'baseline')]
    for (let start = 0; start < 64; start += 8) {
        await evaluate(page, `(async () => {
            for (let index = ${start}; index < ${start + 8}; index += 1) {
                let character = {
                    type: 'character', chaId: 'character-' + index,
                    name: 'Character ' + index,
                    chats: Array.from({ length: 8 }, (_, conversation) => ({
                        id: 'conversation-' + index + '-' + conversation,
                        name: 'Conversation',
                        message: Array.from({ length: 32 }, (_, message) => ({
                            role: message % 2 ? 'char' : 'user',
                            data: String(index).padStart(4, '0') + 'x'.repeat(2044),
                            chatId: 'message-' + index + '-' + conversation + '-' + message,
                        })),
                    })),
                };
                const bytes = new TextEncoder().encode(JSON.stringify(character)).byteLength;
                window.logical.parentRetainedBytes = bytes;
                window.logical.maximumParentChunkBytes = Math.max(
                    window.logical.maximumParentChunkBytes, bytes,
                );
                await window.transferCharacter(character);
                character = null;
                window.logical.parentRetainedBytes = 0;
            }
        })()`)
        samples.push(await sample(page, browser, `after-${start + 8}-characters`))
    }
    await evaluate(page, 'globalThis.gc?.()')
    samples.push(await sample(page, browser, 'retained-after-gc'))
    const logical = await evaluate(page, `(() => {
        const target = document.querySelector('iframe').contentWindow;
        return {
            ...window.logical,
            iframeCharacterCount: target.snapshot.characters.length,
            iframeRetainedJsonBytes: new TextEncoder().encode(
                JSON.stringify(target.snapshot),
            ).byteLength,
        };
    })()`)
    const peak = (field) => Math.max(...samples.map((entry) =>
        field === 'heap'
            ? entry.webViewHeapUsedBytes
            : entry.processTree.workingSetBytes))
    console.log(JSON.stringify({
        fixture: { characters: 64, conversationsPerCharacter: 8, messagesPerConversation: 32,
            messageBodyBytes: 2048 },
        logicalDomains: logical,
        peaks: {
            webViewHeapUsedBytes: peak('heap'),
            totalProcessTreeWorkingSetBytes: peak('tree'),
        },
        samples,
    }, null, 2))
} finally {
    page.close()
    browser.close()
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
        windowsHide: true,
        stdio: 'ignore',
    })
    await once(killer, 'exit')
    await Promise.race([once(child, 'exit'), delay(5_000)])
    await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 })
}
process.exit(0)
