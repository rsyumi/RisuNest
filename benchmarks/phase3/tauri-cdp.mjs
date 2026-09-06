import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { once } from 'node:events'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'

const DEFAULT_TIMEOUT_MS = 120_000
const DEFAULT_FIXTURE_BYTES = 64 * 1024 * 1024
const TEMP_PREFIX = 'risunest-phase3-tauri-'
const CDP_HOST = '127.0.0.1'

export function parseSaveLargeFixture(serialized) {
    const database = JSON.parse(serialized.toString('utf8'))
    if (!Array.isArray(database.characters)) {
        throw new Error('Save-large fixture must contain a characters array')
    }
    const conversations = database.characters.flatMap((character) => character.chats ?? [])
    const totalMessages = conversations.reduce(
        (total, conversation) => total + (conversation.message?.length ?? 0),
        0,
    )
    return {
        database,
        description: {
            kind: 'phase3-step5-save-large',
            serializedBytes: serialized.length,
            serializedSha256: createHash('sha256').update(serialized).digest('hex'),
            characters: database.characters.length,
            totalConversations: conversations.length,
            totalMessages,
        },
    }
}

export function parseArguments(argumentsList) {
    const options = {
        output: null,
        keepProfile: false,
        timeoutMs: DEFAULT_TIMEOUT_MS,
        fixtureBytes: DEFAULT_FIXTURE_BYTES,
        saveLargeFixture: null,
    }

    for (let index = 0; index < argumentsList.length; index += 1) {
        const argument = argumentsList[index]
        if (argument === '--keep-profile') {
            options.keepProfile = true
        } else if (argument === '--output') {
            options.output = requiredValue(argumentsList, ++index, argument)
        } else if (argument === '--timeout-ms') {
            options.timeoutMs = positiveInteger(requiredValue(argumentsList, ++index, argument), argument)
        } else if (argument === '--padding-mib') {
            const mebibytes = positiveInteger(requiredValue(argumentsList, ++index, argument), argument)
            options.fixtureBytes = mebibytes * 1024 * 1024
        } else if (argument === '--save-large-fixture') {
            options.saveLargeFixture = requiredValue(argumentsList, ++index, argument)
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

export function buildBenchmarkConfig(original, port, runId, sourceRevision) {
    const safeRunId = runId.replaceAll(/[^a-zA-Z0-9]/g, '')
    if (!/^[0-9a-f]{40}$/.test(sourceRevision)) {
        throw new Error('Tauri benchmark source revision must be a full Git commit SHA')
    }
    const revisionSegment = `r${sourceRevision.slice(0, 12)}`
    const browserArguments = [
        `--remote-debugging-port=${port}`,
        '--remote-allow-origins=*',
        '--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection',
    ].join(' ')
    const windows = (original.app?.windows ?? [{}]).map((window, index) => ({
        ...window,
        ...(index === 0
            ? {
                dataDirectory: `phase3-benchmark-${runId}`,
                additionalBrowserArgs: browserArguments,
            }
            : {}),
    }))

    return {
        ...structuredClone(original),
        identifier: `RisuNest.phase3benchmark.${revisionSegment}.${safeRunId}`,
        bundle: {
            ...original.bundle,
            active: false,
        },
        plugins: {
            ...original.plugins,
            updater: {
                ...original.plugins?.updater,
                endpoints: [],
            },
        },
        app: {
            ...original.app,
            windows,
        },
    }
}

export function summarizeG6(entries, startTime, endTime) {
    const overlapping = entries.filter((entry) => {
        const entryEnd = entry.startTime + entry.duration
        return entry.duration > 50 && entry.startTime < endTime && entryEnd > startTime
    })
    return {
        passed: overlapping.length === 0,
        longTaskCount: overlapping.length,
        longTaskTotalMs: overlapping.reduce((total, entry) => total + entry.duration, 0),
        longestLongTaskMs: overlapping.reduce(
            (longest, entry) => Math.max(longest, entry.duration),
            0,
        ),
    }
}

export function aggregateProcessMemory(processIds, processRows) {
    const uniqueIds = [...new Set(processIds)]
    const processes = uniqueIds
        .map((pid) => processRows.find((row) => row.pid === pid))
        .filter((row) => row)
        .map(({ pid, workingSetBytes, privateBytes }) => ({ pid, workingSetBytes, privateBytes }))
    return {
        processCount: processes.length,
        workingSetBytes: processes.reduce((sum, row) => sum + row.workingSetBytes, 0),
        privateBytes: processes.reduce((sum, row) => sum + row.privateBytes, 0),
        processes,
    }
}

export function tauriBuildInvocation(repositoryRoot, configPath) {
    return {
        command: process.execPath,
        args: [
            path.join(repositoryRoot, 'node_modules', '@tauri-apps', 'cli', 'tauri.js'),
            'build',
            '--no-bundle',
            '--ci',
            '--config',
            configPath,
        ],
    }
}

export function benchmarkAppDataDirectory(snapshotPath, identifier) {
    const snapshotDirectory = path.dirname(snapshotPath)
    const persistentDirectory = path.dirname(snapshotDirectory)
    const appDataDirectory = path.dirname(persistentDirectory)
    if (
        !identifier.startsWith('RisuNest.phase3benchmark.') ||
        path.basename(snapshotDirectory) !== 'snapshots' ||
        path.basename(persistentDirectory) !== 'persistent' ||
        path.basename(appDataDirectory) !== identifier
    ) {
        throw new Error('Snapshot path does not belong to the isolated benchmark identifier')
    }
    return appDataDirectory
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
        if (message.error) {
            pending.reject(new Error(`${pending.method}: ${message.error.message}`))
        } else {
            pending.resolve(message.result)
        }
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
    child.stdout.on('data', (chunk) => process.stderr.write(chunk))
    child.stderr.on('data', (chunk) => process.stderr.write(chunk))
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0) throw new Error(`${command} exited with code ${exitCode}`)
}

async function captureCommand(command, args, cwd) {
    const child = spawn(command, args, {
        cwd,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk) => { stdout += chunk })
    child.stderr.on('data', (chunk) => { stderr += chunk })
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0) throw new Error(`${command} failed: ${stderr.trim()}`)
    return stdout.trim()
}

async function waitForCdp(port, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    let lastError
    while (Date.now() < deadline) {
        try {
            const [versionResponse, listResponse] = await Promise.all([
                fetch(`http://${CDP_HOST}:${port}/json/version`),
                fetch(`http://${CDP_HOST}:${port}/json/list`),
            ])
            if (versionResponse.ok && listResponse.ok) {
                const version = await versionResponse.json()
                const targets = await listResponse.json()
                const page = targets.find((target) =>
                    target.type === 'page' && typeof target.webSocketDebuggerUrl === 'string')
                if (page && version.webSocketDebuggerUrl) return { version, page }
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
        throw new Error(response.exceptionDetails.exception?.description
            ?? response.exceptionDetails.text
            ?? 'Runtime evaluation failed')
    }
    return response.result.value
}

async function waitForInteractive(page, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        const ready = await evaluate(page, `Boolean(
            globalThis.__TAURI_INTERNALS__?.invoke &&
            performance.getEntriesByName('boot:interactive').length
        )`)
        if (ready) return
        await delay(100)
    }
    throw new Error('Tauri page did not reach boot:interactive')
}

async function queryWindowsProcessMemory(processIds) {
    const ids = [...new Set(processIds.filter(Number.isSafeInteger))]
    if (ids.length === 0) return []
    const command = [
        `$ids = @(${ids.join(',')})`,
        '$rows = Get-Process -Id $ids -ErrorAction SilentlyContinue | ForEach-Object {',
        '  [PSCustomObject]@{ pid = $_.Id; workingSetBytes = $_.WorkingSet64; privateBytes = $_.PrivateMemorySize64 }',
        '}',
        '$rows | ConvertTo-Json -Compress',
    ].join('; ')
    const child = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], {
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    child.stdout.on('data', (chunk) => { stdout += chunk })
    child.stderr.on('data', (chunk) => { stderr += chunk })
    const [exitCode] = await once(child, 'exit')
    if (exitCode !== 0) throw new Error(`Process memory query failed: ${stderr.trim()}`)
    if (!stdout.trim()) return []
    const parsed = JSON.parse(stdout)
    return Array.isArray(parsed) ? parsed : [parsed]
}

function metricsByName(metrics) {
    return Object.fromEntries(metrics.map(({ name, value }) => [name, value]))
}

async function captureMemory(label, page, browser, appPid) {
    const [heap, metrics, processInfo] = await Promise.all([
        page.call('Runtime.getHeapUsage'),
        page.call('Performance.getMetrics'),
        browser.call('SystemInfo.getProcessInfo'),
    ])
    const cdpProcessIds = processInfo.processInfo
        .map((entry) => entry.id)
        .filter(Number.isSafeInteger)
    const processIds = [appPid, ...cdpProcessIds]
    const rows = await queryWindowsProcessMemory(processIds)
    return {
        label,
        timestamp: new Date().toISOString(),
        jsHeap: {
            usedBytes: heap.usedSize,
            totalBytes: heap.totalSize,
            embedderHeapUsedBytes: heap.embedderHeapUsedSize,
            backingStorageBytes: heap.backingStorageSize,
        },
        performance: metricsByName(metrics.metrics),
        processMemory: aggregateProcessMemory(processIds, rows),
    }
}

function peakMemory(samples) {
    return samples.reduce((peak, sample) => ({
        jsHeapUsedBytes: Math.max(peak.jsHeapUsedBytes, sample.jsHeap.usedBytes),
        workingSetBytes: Math.max(peak.workingSetBytes, sample.processMemory.workingSetBytes),
        privateBytes: Math.max(peak.privateBytes, sample.processMemory.privateBytes),
    }), { jsHeapUsedBytes: 0, workingSetBytes: 0, privateBytes: 0 })
}

async function observeOperation(operation, sample) {
    let settled = false
    const samples = [await sample('before')]
    const promise = operation().finally(() => { settled = true })
    promise.catch(() => {})
    while (!settled) {
        await delay(50)
        if (!settled) samples.push(await sample(`sample-${samples.length}`))
    }
    const result = await promise
    samples.push(await sample('after'))
    return { result, samples, peak: peakMemory(samples) }
}

function explicitImportExpression(fixtureBytes) {
    return `(async () => {
        const startedAt = performance.now()
        const { stagingId } = await globalThis.__TAURI_INTERNALS__.invoke('pds_replace_begin')
        try {
            const padding = 'r'.repeat(${fixtureBytes})
            await globalThis.__TAURI_INTERNALS__.invoke('pds_replace_put_root', {
                stagingId,
                root: {
                    phase3Benchmark: {
                        format: 'deterministic-root-padding-v1',
                        paddingBytes: ${fixtureBytes},
                        padding,
                    },
                },
            })
            const committed = await globalThis.__TAURI_INTERNALS__.invoke('pds_replace_commit', { stagingId })
            return { startedAt, endedAt: performance.now(), committed }
        } catch (error) {
            await globalThis.__TAURI_INTERNALS__.invoke('pds_replace_abort', { stagingId }).catch(() => {})
            throw error
        }
    })()`
}

async function invokeTauri(page, command, argumentsValue = {}) {
    return evaluate(
        page,
        `globalThis.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(argumentsValue)})`,
    )
}

function characterBatches(characters, maximumBytes = 4 * 1024 * 1024) {
    const batches = []
    let batch = []
    let batchBytes = 2
    for (const character of characters) {
        const characterBytes = Buffer.byteLength(JSON.stringify(character))
        const separatorBytes = batch.length > 0 ? 1 : 0
        if (batch.length > 0 && batchBytes + separatorBytes + characterBytes > maximumBytes) {
            batches.push(batch)
            batch = []
            batchBytes = 2
        }
        batch.push(character)
        batchBytes += (batch.length > 1 ? 1 : 0) + characterBytes
    }
    if (batch.length > 0) batches.push(batch)
    return batches
}

async function stageSaveLargeFixture(page, database) {
    const startedAt = await evaluate(page, 'performance.now()')
    const { stagingId } = await invokeTauri(page, 'pds_replace_begin')
    try {
        const root = structuredClone(database)
        delete root.characters
        await invokeTauri(page, 'pds_replace_put_root', { stagingId, root })
        await invokeTauri(page, 'pds_replace_put_presets', {
            stagingId,
            presets: database.botPresets,
        })
        for (const characters of characterBatches(database.characters)) {
            await invokeTauri(page, 'pds_replace_add_characters', { stagingId, characters })
        }
        const committed = await invokeTauri(page, 'pds_replace_commit', { stagingId })
        return {
            startedAt,
            endedAt: await evaluate(page, 'performance.now()'),
            committed,
        }
    } catch (error) {
        await invokeTauri(page, 'pds_replace_abort', { stagingId }).catch(() => {})
        throw error
    }
}

const INSTALL_LONG_TASK_OBSERVER = `(() => {
    globalThis.__risuPhase3LongTasks = []
    globalThis.__risuPhase3LongTaskSupported = PerformanceObserver.supportedEntryTypes.includes('longtask')
    if (globalThis.__risuPhase3LongTaskSupported) {
        globalThis.__risuPhase3LongTaskObserver?.disconnect()
        globalThis.__risuPhase3LongTaskObserver = new PerformanceObserver((list) => {
            for (const entry of list.getEntries()) {
                globalThis.__risuPhase3LongTasks.push({
                    name: entry.name,
                    startTime: entry.startTime,
                    duration: entry.duration,
                })
            }
        })
        globalThis.__risuPhase3LongTaskObserver.observe({ type: 'longtask', buffered: true })
    }
    return globalThis.__risuPhase3LongTaskSupported
})()`

const SNAPSHOT_EXPRESSION = `(async () => {
    globalThis.__risuPhase3LongTasks.length = 0
    const startedAt = performance.now()
    const snapshot = await globalThis.__TAURI_INTERNALS__.invoke('pds_snapshot_create', {
        reason: 'phase3-g6-cdp',
    })
    const endedAt = performance.now()
    await new Promise((resolve) => requestAnimationFrame(() => setTimeout(resolve, 0)))
    return {
        startedAt,
        endedAt,
        snapshot,
        longTasks: globalThis.__risuPhase3LongTasks,
        longTaskSupported: globalThis.__risuPhase3LongTaskSupported,
    }
})()`

async function collectBootEvidence(page) {
    return evaluate(page, `(() => ({
        marks: performance.getEntriesByType('mark')
            .filter((entry) => entry.name.startsWith('boot:'))
            .map(({ name, startTime, duration }) => ({ name, startTime, duration })),
        navigation: performance.getEntriesByType('navigation').map((entry) => ({
            startTime: entry.startTime,
            domInteractive: entry.domInteractive,
            domContentLoadedEventEnd: entry.domContentLoadedEventEnd,
            loadEventEnd: entry.loadEventEnd,
            duration: entry.duration,
            transferSize: entry.transferSize,
            decodedBodySize: entry.decodedBodySize,
        })),
        url: location.href,
        userAgent: navigator.userAgent,
    }))()`)
}

export function summarizeUiEvidence({ domNodeCount, mountedMessageCount, resourceUrls }) {
    const liveUrls = new Set(resourceUrls.filter((value) => {
        if (typeof value !== 'string' || value.length === 0) return false
        if (value.startsWith('blob:') || value.startsWith('risuasset:')) return true
        try {
            return new URL(value).hostname === 'risuasset.localhost'
        } catch {
            return false
        }
    }))
    return {
        domNodeCount,
        mountedMessageCount,
        liveUrlCount: liveUrls.size,
    }
}

async function collectUiEvidence(page) {
    const raw = await evaluate(page, `(() => ({
        domNodeCount: document.querySelectorAll('*').length,
        mountedMessageCount: document.querySelectorAll('.risu-chat').length,
        resourceUrls: [...document.querySelectorAll('[src], [href], [poster]')]
            .flatMap((element) => ['src', 'href', 'poster']
                .map((attribute) => element.getAttribute(attribute))
                .filter(Boolean)),
    }))()`)
    return summarizeUiEvidence(raw)
}

function delay(milliseconds) {
    return new Promise((resolve) => setTimeout(resolve, milliseconds))
}

async function stopProcess(child) {
    if (!child || child.exitCode !== null) return
    child.kill()
    const exited = await Promise.race([
        once(child, 'exit').then(() => true),
        delay(5_000).then(() => false),
    ])
    if (exited || child.exitCode !== null) return
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
        windowsHide: true,
        stdio: 'ignore',
    })
    await once(killer, 'exit')
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
    const sourceRevision = await captureCommand('git.exe', ['rev-parse', 'HEAD'], repositoryRoot)
    const saveLargeFixture = options.saveLargeFixture
        ? parseSaveLargeFixture(await readFile(path.resolve(options.saveLargeFixture)))
        : null
    const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), TEMP_PREFIX))
    const runId = path.basename(temporaryRoot).slice(TEMP_PREFIX.length)
    const isolatedRoaming = path.join(temporaryRoot, 'roaming')
    const isolatedLocal = path.join(temporaryRoot, 'local')
    const port = await getFreePort()
    let appProcess
    let page
    let browser
    let nativeAppDataDirectory

    try {
        await Promise.all([
            mkdir(isolatedRoaming, { recursive: true }),
            mkdir(isolatedLocal, { recursive: true }),
        ])
        const baseConfig = JSON.parse(await readFile(
            path.join(repositoryRoot, 'src-tauri', 'tauri.conf.json'),
            'utf8',
        ))
        const benchmarkConfig = buildBenchmarkConfig(baseConfig, port, runId, sourceRevision)
        const configPath = path.join(temporaryRoot, 'tauri.phase3-benchmark.json')
        await writeFile(configPath, JSON.stringify(benchmarkConfig), 'utf8')
        const benchmarkEnvironment = {
            ...process.env,
            VITE_RISU_LEGAL_CONFIGURED: 'TRUE',
            APPDATA: isolatedRoaming,
            LOCALAPPDATA: isolatedLocal,
        }

        const invocation = tauriBuildInvocation(repositoryRoot, configPath)
        await runCommand(invocation.command, invocation.args, {
            cwd: repositoryRoot,
            env: benchmarkEnvironment,
        })

        const executable = path.resolve(
            path.join(repositoryRoot, 'src-tauri', 'target', 'release', `${baseConfig.mainBinaryName}.exe`),
        )
        appProcess = spawn(executable, [], {
            cwd: repositoryRoot,
            env: benchmarkEnvironment,
            windowsHide: true,
            stdio: ['ignore', 'pipe', 'pipe'],
        })
        appProcess.stdout.on('data', (chunk) => process.stderr.write(chunk))
        appProcess.stderr.on('data', (chunk) => process.stderr.write(chunk))

        const cdp = await waitForCdp(port, options.timeoutMs)
        page = new CdpClient(cdp.page.webSocketDebuggerUrl)
        browser = new CdpClient(cdp.version.webSocketDebuggerUrl)
        await Promise.all([page.connect(options.timeoutMs), browser.connect(options.timeoutMs)])
        await Promise.all([
            page.call('Runtime.enable'),
            page.call('Performance.enable'),
            page.call('Network.enable'),
        ])
        // 측정하는 번들은 프로덕션과 동일하게 두고, RisuRealm만 브라우저 레벨에서
        // 끊는다. 제3자 카드와 이미지가 이 세션의 화면이나 로그에 들어오지 못한다.
        await page.call('Network.setBlockedURLs', { urls: REALM_BLOCKED_URL_PATTERNS })
        await evaluate(page, INSTALL_LONG_TASK_OBSERVER)
        await waitForInteractive(page, options.timeoutMs)
        // Probe the store IPC so readiness failures surface before any measured operation.
        await evaluate(page, `globalThis.__TAURI_INTERNALS__.invoke('pds_snapshot_list')`)

        const boot = await collectBootEvidence(page)
        const bootMemory = await captureMemory('boot-interactive', page, browser, appProcess.pid)
        const imported = await observeOperation(
            () => saveLargeFixture
                ? stageSaveLargeFixture(page, saveLargeFixture.database)
                : evaluate(page, explicitImportExpression(options.fixtureBytes)),
            (label) => captureMemory(`import-${label}`, page, browser, appProcess.pid),
        )
        await evaluate(page, INSTALL_LONG_TASK_OBSERVER)
        const snapshot = await observeOperation(
            () => evaluate(page, SNAPSHOT_EXPRESSION),
            (label) => captureMemory(`snapshot-${label}`, page, browser, appProcess.pid),
        )
        const g6 = summarizeG6(
            snapshot.result.longTasks,
            snapshot.result.startedAt,
            snapshot.result.endedAt,
        )
        const ui = await collectUiEvidence(page)
        nativeAppDataDirectory = benchmarkAppDataDirectory(
            snapshot.result.snapshot.path,
            benchmarkConfig.identifier,
        )

        return {
            schemaVersion: 1,
            measuredAt: new Date().toISOString(),
            platform: {
                os: `${os.type()} ${os.release()}`,
                arch: os.arch(),
                node: process.version,
                webViewUserAgent: boot.userAgent,
            },
            build: {
                release: true,
                realmDisabled: true,
                sourceRevision,
                identifier: benchmarkConfig.identifier,
                isolatedProfile: true,
            },
            fixture: saveLargeFixture?.description ?? {
                kind: 'explicit-staged-native-root-padding',
                bytes: options.fixtureBytes,
                deterministicByte: 'r',
                legacyWebViewStorageRead: false,
            },
            boot: {
                marks: boot.marks,
                navigation: boot.navigation,
                memory: bootMemory,
            },
            explicitImport: {
                wallTimeMs: imported.result.endedAt - imported.result.startedAt,
                committed: imported.result.committed,
                memorySamples: imported.samples,
                peakMemory: imported.peak,
            },
            snapshot: {
                wallTimeMs: snapshot.result.endedAt - snapshot.result.startedAt,
                nativeResult: snapshot.result.snapshot,
                longTaskSupported: snapshot.result.longTaskSupported,
                longTasks: snapshot.result.longTasks,
                memorySamples: snapshot.samples,
                peakMemory: snapshot.peak,
            },
            ui,
            gates: { g6 },
            limits: [
                'The setup is a benchmark-only explicit staged import through existing Tauri commands, not public file-picker UI automation.',
                saveLargeFixture
                    ? 'The staged import uses the serialized Phase 3 save-large fixture supplied by the Roadmap 14 runner.'
                    : 'The deterministic root padding is not the full save-large domain fixture.',
                'Boot memory is the isolated fresh-install baseline before the explicit import.',
                'G6 covers WebView main-thread long tasks overlapping pds_snapshot_create on Windows release Tauri only.',
                'Windows working-set samples include the Tauri process and CDP-reported WebView processes; sampling can miss very short peaks.',
                'Live RisuRealm and live account compatibility are intentionally not exercised.',
            ],
        }
    } finally {
        page?.close()
        browser?.close()
        await stopProcess(appProcess)
        if (!options.keepProfile) {
            if (nativeAppDataDirectory) {
                await rm(nativeAppDataDirectory, { recursive: true, force: true })
            }
            assertSafeTemporaryDirectory(temporaryRoot)
            await rm(temporaryRoot, { recursive: true, force: true })
        } else {
            process.stderr.write(`Kept isolated benchmark profile: ${temporaryRoot}${os.EOL}`)
        }
    }
}

function usage() {
    return [
        'Usage: node benchmarks/phase3/tauri-cdp.mjs [options]',
        '',
        'Options:',
        '  --padding-mib <integer>   Explicit import padding, default 64.',
        '  --save-large-fixture <path>  Stage an exact serialized Phase 3 save-large fixture.',
        '  --timeout-ms <integer>    CDP and boot timeout, default 120000.',
        '  --output <path>           Also write the JSON result to this path.',
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
    if (!result.gates.g6.passed || !result.snapshot.longTaskSupported) process.exitCode = 1
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
    main().catch((error) => {
        process.stderr.write(`${error.stack ?? error}${os.EOL}`)
        process.exitCode = 1
    })
}
