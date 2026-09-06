import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'

const CDP_HOST = '127.0.0.1'
const TEMP_PREFIX = 'risunest-regex-native-pilot-'

export function buildBenchmarkConfig(original, port, runId) {
    const safeRunId = runId.replaceAll(/[^a-zA-Z0-9]/g, '')
    const browserArguments = [
        `--remote-debugging-port=${port}`,
        '--remote-allow-origins=*',
        '--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection',
    ].join(' ')
    return {
        ...structuredClone(original),
        identifier: `RisuNest.regexnativepilot.${safeRunId}`,
        build: {
            ...original.build,
            beforeBuildCommand: 'pnpm exec vite build --config benchmarks/regex-native-pilot/vite.config.ts',
            frontendDist: '../node_modules/.cache/regex-native-pilot',
        },
        bundle: { ...original.bundle, active: false },
        plugins: {
            ...original.plugins,
            updater: { ...original.plugins?.updater, endpoints: [] },
        },
        app: {
            ...original.app,
            windows: (original.app?.windows ?? [{}]).map((window, index) => ({
                ...window,
                ...(index === 0 ? {
                    title: 'RisuNest Regex Native Pilot',
                    dataDirectory: `regex-native-pilot-${runId}`,
                    additionalBrowserArgs: browserArguments,
                } : {}),
            })),
        },
    }
}

export function summarizeGates(cells) {
    const passingCells = cells.filter((cell) => cell.gate.passed)
    const productionShaped = cells.filter((cell) => cell.rules >= 100 && cell.inputBytes >= 32 * 1024)
    return {
        passingCells: passingCells.map((cell) => ({
            rules: cell.rules,
            inputBytes: cell.inputBytes,
        })),
        productionShapedPassed: productionShaped.length > 0
            && productionShaped.every((cell) => cell.gate.passed),
        productionShapedCells: productionShaped.length,
        productionShapedPassingCells: productionShaped.filter((cell) => cell.gate.passed).length,
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
        }
        finally {
            clearTimeout(timeout)
        }
        socket.addEventListener('message', (event) => this.onMessage(event.data))
        socket.addEventListener('close', () => this.rejectPending(new Error('CDP WebSocket closed')))
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

    onMessage(data) {
        const message = JSON.parse(typeof data === 'string' ? data : Buffer.from(data).toString())
        if (!message.id) return
        const pending = this.pending.get(message.id)
        if (!pending) return
        this.pending.delete(message.id)
        if (message.error) pending.reject(new Error(`${pending.method}: ${message.error.message}`))
        else pending.resolve(message.result)
    }

    rejectPending(error) {
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

async function runCommand(command, argumentsList, options) {
    const child = spawn(command, argumentsList, {
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
        }
        catch (error) {
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
            response.exceptionDetails.exception?.description
                ?? response.exceptionDetails.text
                ?? 'Runtime evaluation failed',
        )
    }
    return response.result.value
}

async function waitForPilot(page, timeoutMs) {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
        if (await evaluate(page, 'Boolean(globalThis.__RISUNEST_REGEX_NATIVE_PILOT__)')) return
        await delay(100)
    }
    throw new Error('Regex native pilot page did not become ready')
}

async function stopProcess(child) {
    if (!child || child.exitCode !== null) return
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
        windowsHide: true,
        stdio: 'ignore',
    })
    await once(killer, 'exit')
    if (child.exitCode === null) {
        await Promise.race([once(child, 'exit'), delay(5_000)])
    }
}

function assertSafeTemporaryDirectory(directory) {
    const temporaryRoot = path.resolve(os.tmpdir()) + path.sep
    const resolved = path.resolve(directory)
    if (!resolved.startsWith(temporaryRoot) || !path.basename(resolved).startsWith(TEMP_PREFIX)) {
        throw new Error(`Refusing to remove unexpected directory: ${resolved}`)
    }
}

async function runPilot(options) {
    if (process.platform !== 'win32') throw new Error('This pilot supports Windows only')
    const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
    const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), TEMP_PREFIX))
    const runId = path.basename(temporaryRoot).slice(TEMP_PREFIX.length)
    const port = await getFreePort()
    let appProcess
    let page
    try {
        const baseConfig = JSON.parse(
            await readFile(path.join(repositoryRoot, 'src-tauri', 'tauri.conf.json'), 'utf8'),
        )
        const config = buildBenchmarkConfig(baseConfig, port, runId)
        const configPath = path.join(temporaryRoot, 'tauri.regex-native-pilot.json')
        await writeFile(configPath, JSON.stringify(config), 'utf8')
        const environment = {
            ...process.env,
            VITE_RISU_LEGAL_CONFIGURED: 'TRUE',
            CARGO_BUILD_JOBS: '1',
            APPDATA: path.join(temporaryRoot, 'roaming'),
            LOCALAPPDATA: path.join(temporaryRoot, 'local'),
        }
        await Promise.all([
            mkdir(environment.APPDATA, { recursive: true }),
            mkdir(environment.LOCALAPPDATA, { recursive: true }),
        ])
        await runCommand(process.execPath, [
            path.join(repositoryRoot, 'node_modules', '@tauri-apps', 'cli', 'tauri.js'),
            'build',
            '--no-bundle',
            '--ci',
            '--config',
            configPath,
        ], { cwd: repositoryRoot, env: environment })

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
        // RisuRealm만 브라우저 레벨에서 끊는다. 측정 번들은 프로덕션과 동일하다.
        await page.call('Network.setBlockedURLs', { urls: REALM_BLOCKED_URL_PATTERNS })
        await waitForPilot(page, options.timeoutMs)
        const pilot = await evaluate(page, 'globalThis.__RISUNEST_REGEX_NATIVE_PILOT__.run()')
        return {
            schemaVersion: 1,
            measuredAt: new Date().toISOString(),
            platform: {
                os: `${os.type()} ${os.release()}`,
                arch: os.arch(),
                node: process.version,
                userAgent: pilot.userAgent,
            },
            build: {
                realmDisabled: true,
                releaseTauri: true,
                cargoBuildJobs: 1,
                isolatedProfile: true,
            },
            cells: pilot.cells,
            gates: summarizeGates(pilot.cells),
        }
    }
    finally {
        page?.close()
        await stopProcess(appProcess)
        assertSafeTemporaryDirectory(temporaryRoot)
        await rm(temporaryRoot, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 })
    }
}

function parseArguments(argumentsList) {
    const options = { output: null, timeoutMs: 300_000 }
    for (let index = 0; index < argumentsList.length; index++) {
        const argument = argumentsList[index]
        if (argument === '--output') options.output = argumentsList[++index]
        else if (argument === '--timeout-ms') options.timeoutMs = Number(argumentsList[++index])
        else if (argument === '--help' || argument === '-h') options.help = true
        else throw new Error(`Unknown argument: ${argument}`)
    }
    return options
}

function delay(milliseconds) {
    return new Promise((resolve) => setTimeout(resolve, milliseconds))
}

async function main() {
    const options = parseArguments(process.argv.slice(2))
    if (options.help) {
        process.stdout.write('Usage: node benchmarks/regex-native-pilot/runner.mjs [--output path] [--timeout-ms number]\n')
        return
    }
    const result = await runPilot(options)
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
