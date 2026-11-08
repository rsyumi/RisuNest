import { spawn, execFileSync } from 'node:child_process'
import { once } from 'node:events'
import { mkdtemp, rm, readFile } from 'node:fs/promises'
import net from 'node:net'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { createRequire } from 'node:module'
import { createHash } from 'node:crypto'
import ts from 'typescript'
import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'
import { evaluationErrorText } from '../support/cdpException.mjs'

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds))

const args = new Map(process.argv.slice(2).map(arg => {
    const [key, ...value] = arg.replace(/^--/, '').split('=')
    return [key, value.join('=') || 'true']
}))
function positiveOption(name, fallback) {
    const value = Number(args.get(name) ?? fallback)
    if (!Number.isSafeInteger(value) || value < 1) throw new Error(`Invalid --${name}`)
    return value
}
const fixture = {
    characters: positiveOption('characters', 4),
    messagesPerConversation: positiveOption('messages', 4096),
    messageBodyBytes: positiveOption('body-bytes', 2048),
    nonMessageBytes: positiveOption('field-bytes', 8192),
}
const iterations = positiveOption('iterations', 3)
const hostOnly = args.has('host-only')
const baselineRef = args.get('baseline-ref')
const require = createRequire(import.meta.url)
const { build } = createRequire(require.resolve('vite'))('esbuild')
const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
async function productSource(relative) {
    return baselineRef
        ? execFileSync('git', ['show', `${baselineRef}:${relative}`], { cwd: repository, encoding: 'utf8', windowsHide: true })
        : readFile(path.join(repository, relative), 'utf8')
}
function declaration(source, name) {
    const file = ts.createSourceFile('fixture.ts', source, ts.ScriptTarget.Latest, true)
    const node = file.statements.find(node => ts.isFunctionDeclaration(node) && node.name?.text === name)
    if (!node) throw new Error(`Production function unavailable: ${name}`)
    return node.getText(file)
}

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
        throw new Error(await evaluationErrorText(
            (method, params) => page.call(method, params),
            result.exceptionDetails,
            result.exceptionDetails.exception?.description ?? 'evaluation failed',
        ))
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

async function sample(page, browser, label, includeProcessTree) {
    const heap = await page.call('Runtime.getHeapUsage')
    return {
        label,
        webViewHeapUsedBytes: heap.usedSize,
        webViewHeapTotalBytes: heap.totalSize,
        processTree: includeProcessTree ? await processTree(browser) : null,
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
    const factory = await productSource('src/ts/plugins/apiV3/factory.ts')
    const baselines = await productSource('src/ts/plugins/pluginReadBaselines.ts')
    const incremental = baselines.includes('beginSnapshot(')
    let implementation = baselines
    if (incremental) {
        implementation += '\n' + await productSource('src/ts/plugins/pluginSnapshotStream.ts')
    } else {
        const access = await productSource('src/ts/plugins/pluginDatabaseAccess.ts')
        implementation += '\n' + await productSource('src/ts/storage/ownEnumerableProperty.ts')
        implementation += '\nexport ' + declaration(access, 'accumulatePluginSnapshotChunk')
    }
    const compiledHashes = {}
    for (const [source, name] of [[factory, 'SandboxModule'], [implementation, 'BaselineModule']]) {
        const compiled = await build({
            stdin: { contents: source, loader: 'ts', resolveDir: path.join(repository, name === 'SandboxModule' ? 'src/ts/plugins/apiV3' : 'src/ts/plugins') },
            bundle: true, write: false, platform: 'browser', conditions: ['browser'],
            format: 'iife', globalName: name, define: { 'import.meta.env.DEV': 'false' },
            plugins: [{ name: 'source-revision', setup(build) {
                if (baselineRef) build.onLoad({ filter: /\.ts$/ }, async ({ path: filename }) => {
                    const relative = path.relative(repository, filename).replaceAll('\\', '/')
                    if (!relative.startsWith('src/')) return
                    return { contents: await productSource(relative), loader: 'ts' }
                })
            } }],
        })
        compiledHashes[name] = createHash('sha256').update(compiled.outputFiles[0].text).digest('hex')
        await evaluate(page, compiled.outputFiles[0].text)
    }
    await evaluate(page, `(() => {
        window.fixture = ${JSON.stringify(fixture)};
        window.longTasks = [];
        new PerformanceObserver(list => window.longTasks.push(...list.getEntries().map(e => ({ start: e.startTime, duration: e.duration }))))
            .observe({ type: 'longtask', buffered: true });
        window.runFixture = async () => {
            const start = performance.now();
            window.pausedAt = 0;
            window.releaseSource = undefined;
            window.completed = undefined;
            window.longTasks.length = 0;
            const metrics = { frames: 0, reads: 0, logicalChunks: 0, maximumFrameChunks: 0, maximumSourcePauseMs: 0 };
            window.baselines = new BaselineModule.PluginReadBaselines('synthetic', () => 1);
            async function* source() {
                yield { type: 'set', key: 'username', value: 'Synthetic' };
                yield { type: 'arrayStart', key: 'characters' };
                for (let character = 0; character < fixture.characters; character++) {
                    yield { type: 'characterStart', key: 'characters', value: { chaId: 'character-' + character, name: 'Synthetic', desc: 'd'.repeat(fixture.nonMessageBytes) } };
                    yield { type: 'conversationStart', key: 'characters', value: { id: 'conversation-' + character, name: 'Synthetic' } };
                    for (let i = 0; i < fixture.messagesPerConversation; i++) {
                        yield { type: 'message', key: 'characters', value: {
                            role: i % 2 ? 'char' : 'user', data: (String(i) + ':').padEnd(fixture.messageBodyBytes, 'x'), chatId: character + ':' + i,
                        } };
                    }
                }
                window.pausedAt = performance.now();
                await new Promise(resolve => { window.releaseSource = resolve; });
            }
            let stream;
            if (${incremental}) {
                stream = BaselineModule.createPluginSnapshotStream(source(), baselines.beginSnapshot('database', 'database'), async () => {},
                    { maxChunks: ${positiveOption('max-chunks', 128)}, maxBytes: ${positiveOption('max-bytes', 256 * 1024)} });
            } else {
                const iterator = source();
                let copy = {};
                stream = new ReadableStream({ async pull(controller) {
                    const next = await iterator.next();
                    if (next.done) {
                        baselines.track(copy, 'database', 'database', 1);
                        controller.enqueue({ type: 'provenance', entries: BaselineModule.collectPluginReadProvenance(copy) });
                        copy = undefined;
                        controller.close();
                    } else {
                        BaselineModule.accumulatePluginSnapshotChunk(copy, next.value);
                        controller.enqueue(next.value);
                    }
                } });
            }
            const reader = stream.getReader();
            const measured = new ReadableStream({ async pull(controller) {
                metrics.reads++;
                const before = performance.now();
                const next = await reader.read();
                const duration = performance.now() - before;
                if (!window.pausedAt) metrics.maximumSourcePauseMs = Math.max(metrics.maximumSourcePauseMs, duration);
                if (next.done) controller.close();
                else {
                    const count = next.value.type === 'batch' ? next.value.chunks.length : 1;
                    metrics.frames++;
                    metrics.logicalChunks += count;
                    metrics.maximumFrameChunks = Math.max(metrics.maximumFrameChunks, count);
                    controller.enqueue(next.value);
                }
            }, cancel: reason => reader.cancel(reason) }, { highWaterMark: 0 });
            const finish = assembled => {
                window.completed = { ...metrics, ...assembled,
                    callMs: (assembled.publicCallMs ?? performance.now() - start) - window.measurementPauseMs,
                    pauseMs: window.measurementPauseMs,
                    baselines: baselines.retainedBaselineCount,
                    maxLongTaskMs: Math.max(0, ...window.longTasks.map(e => e.duration)),
                    longTaskCount: window.longTasks.length,
                };
            };
            if (${hostOnly}) {
                const drain = measured.getReader();
                while (!(await drain.read()).done) {}
                finish({});
            } else {
                const frame = document.createElement('iframe');
                window.fixtureFrame = frame;
                document.body.append(frame);
                window.fixtureHost = new SandboxModule.SandboxHost({
                    _getPropertiesForInitialization: () => ({ apiVersion: '3.0', list: ['apiVersion'] }),
                    _getAliases: () => ({}),
                    snapshot: () => ({ __type: 'IFRAME_OBJECT_STREAM', value: measured }),
                    assembled: finish,
                });
                fixtureHost.run(frame, 'const started = performance.now(); risuai.snapshot().then(value => { const publicCallMs = performance.now() - started; window.snapshot = value; return risuai.assembled({ publicCallMs, messageCount: value.characters.reduce((n, c) => n + c.chats[0].message.length, 0), characterCount: value.characters.length }); });', 'synthetic-memory');
            }
        };
    })()`)
    const runs = []
    async function waitFor(expression) {
        for (let attempt = 0; attempt < 2400; attempt++) {
            if (await evaluate(page, expression)) return
            await delay(25)
        }
        throw new Error(`Fixture timed out: ${expression}`)
    }
    for (let iteration = 0; iteration < iterations; iteration++) {
        await evaluate(page, 'window.fixtureHost?.terminate(); window.fixtureFrame?.remove(); window.fixtureFrame = undefined; window.fixtureHost = undefined; window.baselines = undefined; globalThis.gc?.()')
        const before = await sample(page, browser, 'before', iteration === 0)
        await evaluate(page, 'void runFixture().catch(error => { window.fixtureError = String(error); })')
        await waitFor('Boolean(window.releaseSource || window.fixtureError)')
        const error = await evaluate(page, 'window.fixtureError')
        if (error) throw new Error(error)
        await evaluate(page, 'globalThis.gc?.()')
        const beforePublication = await sample(page, browser, 'before-publication-after-gc', iteration === 0)
        await evaluate(page, 'window.measurementPauseMs = performance.now() - window.pausedAt; window.releaseSource(); window.releaseSource = undefined;')
        await waitFor('Boolean(window.completed || window.fixtureError)')
        const metrics = await evaluate(page, 'window.completed')
        if (!metrics || (!hostOnly && metrics.messageCount !== fixture.characters * fixture.messagesPerConversation)) {
            throw new Error('Guest assembly returned an incorrect message count')
        }
        await evaluate(page, 'globalThis.gc?.()')
        const retained = await sample(page, browser, 'retained-after-gc', iteration === 0)
        runs.push({ cache: iteration === 0 ? 'cold-runtime' : 'warm-runtime', metrics, samples: [before, beforePublication, retained] })
    }
    console.log(JSON.stringify({
        source: baselineRef ?? 'working-tree', compiledHashes, browser: version.Browser, fixture, hostOnly, incremental,
        implementation: 'Production SandboxHost, assembly and baseline algorithms; synthetic source, storage reads excluded. Heap measurements belong to the parent CDP target; a separate guest process heap is not sampled. Process-tree samples include guest processes and are collected on cold runs only. Host-only runs measure baseline retention without iframe delivery. Long-task observation reports tasks of at least 50 ms.',
        runs,
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
