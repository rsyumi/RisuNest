import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { access, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'

const CDP_HOST = '127.0.0.1'
const DEFAULT_TIMEOUT_MS = 180_000
const TEMP_PREFIX = 'risunest-lua-worker-pilot-'

export function buildTauriProfileConfig(original, port, runId) {
  const safeRunId = runId.replaceAll(/[^a-zA-Z0-9]/g, '')
  const browserArguments = [
    `--remote-debugging-port=${port}`,
    '--remote-allow-origins=*',
    '--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection',
  ].join(' ')
  return {
    ...structuredClone(original),
    identifier: `RisuNest.luaworkerpilot.${safeRunId}`,
    build: {
      ...original.build,
      beforeBuildCommand: 'pnpm exec vite build --config benchmarks/lua-worker-pilot/vite.config.ts',
      frontendDist: '../dist-lua-worker-pilot',
    },
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
      windows: (original.app?.windows ?? [{}]).map((window, index) => ({
        ...window,
        ...(index === 0 ? {
          title: 'RisuNest Lua Worker Pilot',
          dataDirectory: `lua-worker-pilot-${runId}`,
          additionalBrowserArgs: browserArguments,
        } : {}),
      })),
    },
  }
}

export function summarizePilotGates(pilot, baselineMemory, idleMemory) {
  const requiredParityCases = [
    'chat-reads',
    'ordered-mutations',
    'mutation-reads',
    'variables',
    'stop-chat',
  ]
  const requiredBoundaryCases = ['unsupported', 'contextWindow', 'memory']
  const parityNames = new Set((pilot.parity ?? []).map((entry) => entry.name))
  const missingParityCases = requiredParityCases.filter((name) => !parityNames.has(name))
  const boundaries = pilot.boundaries ?? {}
  const missingBoundaryCases = requiredBoundaryCases.filter((name) => !(name in boundaries))
  const mainP95 = pilot.performance.main.p95Ms
  const workerP95 = pilot.performance.worker.p95Ms
  const mainBusy = pilot.performance.main.busyTimeMs
  const workerBusy = pilot.performance.worker.busyTimeMs
  const rssBaseline = Number.isFinite(baselineMemory.processMemory.workingSetBytes)
    ? baselineMemory.processMemory.workingSetBytes
    : 0
  const rssIdle = Number.isFinite(idleMemory.processMemory.workingSetBytes)
    ? idleMemory.processMemory.workingSetBytes
    : 0
  const rssDelta = Math.max(0, rssIdle - rssBaseline)
  const rssBudget = Math.min(64 * 1024 * 1024, rssBaseline * 0.10)
  const supportedBoundariesPassed = missingBoundaryCases.length === 0
    && requiredBoundaryCases.every((name) => boundaries[name]?.passed === true)
  const hasCompleteProcessSample = (snapshot) => {
    const memory = snapshot?.processMemory ?? {}
    const requested = memory.requestedProcessIds
    const sampled = memory.sampledProcessIds
    return Array.isArray(requested)
      && requested.length > 0
      && Array.isArray(sampled)
      && sampled.length === requested.length
      && requested.every((pid) => Number.isSafeInteger(pid) && sampled.includes(pid))
      && Number.isFinite(memory.workingSetBytes)
      && memory.workingSetBytes > 0
  }
  const completeRssEvidence = hasCompleteProcessSample(baselineMemory)
    && hasCompleteProcessSample(idleMemory)
    && baselineMemory.processMemory.requestedProcessIds.length
      === idleMemory.processMemory.requestedProcessIds.length
    && baselineMemory.processMemory.requestedProcessIds.every((pid) => (
      idleMemory.processMemory.requestedProcessIds.includes(pid)
    ))
  const semanticParity = {
    passed: pilot.parityMismatchCount === 0
      && pilot.globalIsolation.passed
      && pilot.syntheticPromise.passed
      && pilot.atomicFailureComparison.productionSemanticMatch
      && missingParityCases.length === 0,
    mismatchCount: pilot.parityMismatchCount,
    missingCases: missingParityCases,
  }
  const termination = {
    passed: pilot.termination.passed && pilot.termination.p95Ms <= 100,
    p95Ms: pilot.termination.p95Ms,
    budgetMs: 100,
  }
  const uiBusyTime = {
    passed: pilot.performance.uiBusyMeasurement === 'warm-total-timer-lag-v1'
      && mainBusy > 0 && workerBusy <= mainBusy * 0.10,
    mainMs: mainBusy,
    workerMs: workerBusy,
    reduction: mainBusy > 0 ? 1 - workerBusy / mainBusy : 0,
  }
  const integratedP95 = {
    passed: Number.isFinite(mainP95) && mainP95 > 0
      && Number.isFinite(workerP95) && workerP95 > 0
      && workerP95 <= mainP95 * 1.10,
    mainMs: mainP95,
    workerMs: workerP95,
    ratio: mainP95 > 0 ? workerP95 / mainP95 : Number.POSITIVE_INFINITY,
  }
  const idleRss = {
    passed: completeRssEvidence && rssDelta <= rssBudget,
    baselineBytes: rssBaseline,
    idleFourWorkersBytes: rssIdle,
    deltaBytes: rssDelta,
    budgetBytes: rssBudget,
    completeProcessSamples: completeRssEvidence,
  }
  const partialMutations = {
    passed: pilot.atomicFailureComparison.zeroPartialWorkerMutation,
  }
  const windowsPilotPassed = semanticParity.passed
    && supportedBoundariesPassed
    && termination.passed
    && uiBusyTime.passed
    && integratedP95.passed
    && idleRss.passed
    && partialMutations.passed

  const supportedBoundaries = {
    passed: supportedBoundariesPassed,
    missingCases: missingBoundaryCases,
  }
  const productionBlockers = [
    !semanticParity.passed && 'Semantic parity gate failed.',
    !supportedBoundaries.passed && 'Supported boundary evidence is incomplete or failed.',
    !termination.passed && 'Worker termination gate failed.',
    !uiBusyTime.passed && 'UI busy-time gate failed.',
    !integratedP95.passed && 'Integrated P95 latency gate failed.',
    !idleRss.passed && 'Idle Worker RSS gate failed or has incomplete process samples.',
    !partialMutations.passed && 'Partial-mutation gate failed.',
  ].filter(Boolean)

  return {
    semanticParity,
    supportedBoundaries,
    partialMutations,
    termination,
    uiBusyTime,
    integratedP95,
    idleRss,
    windowsPilotPassed,
    productionAdoptionEnabled: false,
    productionBlockers,
  }
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
  if (port === null) throw new Error('Failed to reserve a port')
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

async function waitForHttp(url, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  let lastError
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url)
      if (response.ok) return
    }
    catch (error) {
      lastError = error
    }
    await delay(100)
  }
  throw new Error(`HTTP endpoint did not become ready: ${lastError ?? url}`)
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
        const page = targets.find((target) => (
          target.type === 'page' && typeof target.webSocketDebuggerUrl === 'string'
        ))
        if (page && version.webSocketDebuggerUrl) return { version, page }
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
    throw new Error(response.exceptionDetails.exception?.description
      ?? response.exceptionDetails.text
      ?? 'Runtime evaluation failed')
  }
  return response.result.value
}

async function waitForPilot(page, timeoutMs) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const ready = await evaluate(page, `Boolean(globalThis.__RISUNEST_LUA_WORKER_PILOT__)`)
    if (ready) return
    await delay(100)
  }
  throw new Error('Lua Worker pilot page did not become ready')
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

function aggregateProcessMemory(processIds, rows) {
  const requestedProcessIds = [...new Set(processIds)]
  const processes = requestedProcessIds
    .map((pid) => rows.find((row) => row.pid === pid))
    .filter(Boolean)
  return {
    requestedProcessIds,
    sampledProcessIds: processes.map((row) => row.pid),
    requestedProcessCount: requestedProcessIds.length,
    processCount: processes.length,
    workingSetBytes: processes.reduce((sum, row) => sum + row.workingSetBytes, 0),
    privateBytes: processes.reduce((sum, row) => sum + row.privateBytes, 0),
    processes,
  }
}

async function captureMemory(label, page, browser, rootPid) {
  const [heap, processInfo] = await Promise.all([
    page.call('Runtime.getHeapUsage'),
    browser.call('SystemInfo.getProcessInfo'),
  ])
  const processIds = [
    rootPid,
    ...processInfo.processInfo.map((entry) => entry.id).filter(Number.isSafeInteger),
  ]
  const rows = await queryWindowsProcessMemory(processIds)
  return {
    label,
    jsHeap: {
      usedBytes: heap.usedSize,
      totalBytes: heap.totalSize,
      embedderHeapUsedBytes: heap.embedderHeapUsedSize,
      backingStorageBytes: heap.backingStorageSize,
    },
    processMemory: aggregateProcessMemory(processIds, rows),
  }
}

async function findBrowserExecutable() {
  const candidates = [
    path.join(process.env['ProgramFiles(x86)'] ?? '', 'Microsoft', 'Edge', 'Application', 'msedge.exe'),
    path.join(process.env.ProgramFiles ?? '', 'Microsoft', 'Edge', 'Application', 'msedge.exe'),
    path.join(process.env.ProgramFiles ?? '', 'Google', 'Chrome', 'Application', 'chrome.exe'),
  ]
  for (const candidate of candidates) {
    if (!candidate.startsWith(path.parse(candidate).root)) continue
    try {
      await access(candidate)
      return candidate
    }
    catch {}
  }
  throw new Error('Microsoft Edge or Google Chrome was not found')
}

async function startBrowser(repositoryRoot, temporaryRoot, port, timeoutMs) {
  const previewPort = await getFreePort()
  const preview = spawn(process.execPath, [
    path.join(repositoryRoot, 'node_modules', 'vite', 'bin', 'vite.js'),
    'preview',
    '--config',
    'benchmarks/lua-worker-pilot/vite.config.ts',
    '--host',
    CDP_HOST,
    '--port',
    String(previewPort),
  ], {
    cwd: repositoryRoot,
    env: { ...process.env },
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  preview.stdout.on('data', (chunk) => process.stderr.write(chunk))
  preview.stderr.on('data', (chunk) => process.stderr.write(chunk))
  try {
    const url = `http://${CDP_HOST}:${previewPort}`
    await waitForHttp(url, timeoutMs)
    const executable = await findBrowserExecutable()
    const browser = spawn(executable, [
      '--headless=new',
      `--remote-debugging-port=${port}`,
      '--remote-allow-origins=*',
      `--user-data-dir=${path.join(temporaryRoot, 'browser-profile')}`,
      '--no-first-run',
      '--disable-background-networking',
      url,
    ], {
      cwd: repositoryRoot,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    return { appProcess: browser, helperProcess: preview }
  }
  catch (error) {
    await stopProcess(preview)
    throw error
  }
}

async function startTauri(repositoryRoot, temporaryRoot, port) {
  const baseConfig = JSON.parse(await readFile(
    path.join(repositoryRoot, 'src-tauri', 'tauri.conf.json'),
    'utf8',
  ))
  const runId = path.basename(temporaryRoot).slice(TEMP_PREFIX.length)
  const config = buildTauriProfileConfig(baseConfig, port, runId)
  const configPath = path.join(temporaryRoot, 'tauri.lua-worker-pilot.json')
  await writeFile(configPath, JSON.stringify(config), 'utf8')
  const environment = {
    ...process.env,
    VITE_RISU_LEGAL_CONFIGURED: 'TRUE',
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
  const executable = path.join(
    repositoryRoot,
    'src-tauri',
    'target',
    'release',
    `${baseConfig.mainBinaryName}.exe`,
  )
  const appProcess = spawn(executable, [], {
    cwd: repositoryRoot,
    env: environment,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  appProcess.stdout.on('data', (chunk) => process.stderr.write(chunk))
  appProcess.stderr.on('data', (chunk) => process.stderr.write(chunk))
  return { appProcess, helperProcess: undefined }
}

async function stopProcess(child) {
  if (!child || child.exitCode !== null) return
  if (process.platform === 'win32') {
    const killer = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], {
      windowsHide: true,
      stdio: 'ignore',
    })
    await once(killer, 'exit')
  }
  else {
    child.kill()
  }
  if (child.exitCode === null) {
    await Promise.race([
      once(child, 'exit'),
      delay(5_000),
    ])
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
  if (process.platform !== 'win32') throw new Error('The Lua Worker pilot supports Windows only')
  const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
  const temporaryRoot = await mkdtemp(path.join(os.tmpdir(), TEMP_PREFIX))
  const port = await getFreePort()
  let appProcess
  let helperProcess
  let page
  let browser
  try {
    if (options.mode === 'browser') {
      await runCommand(process.execPath, [
        path.join(repositoryRoot, 'node_modules', 'vite', 'bin', 'vite.js'),
        'build',
        '--config',
        'benchmarks/lua-worker-pilot/vite.config.ts',
      ], {
        cwd: repositoryRoot,
        env: { ...process.env },
      })
    }
    const started = options.mode === 'browser'
      ? await startBrowser(repositoryRoot, temporaryRoot, port, options.timeoutMs)
      : await startTauri(repositoryRoot, temporaryRoot, port)
    appProcess = started.appProcess
    helperProcess = started.helperProcess
    const cdp = await waitForCdp(port, options.timeoutMs)
    page = new CdpClient(cdp.page.webSocketDebuggerUrl)
    browser = new CdpClient(cdp.version.webSocketDebuggerUrl)
    await Promise.all([page.connect(options.timeoutMs), browser.connect(options.timeoutMs)])
    await page.call('Runtime.enable')
    await page.call('Network.enable')
    // Cut RisuRealm at the browser level only. The measured bundle stays identical to production.
    await page.call('Network.setBlockedURLs', { urls: REALM_BLOCKED_URL_PATTERNS })
    await waitForPilot(page, options.timeoutMs)
    const baselineMemory = await captureMemory('baseline', page, browser, appProcess.pid)
    await evaluate(page, `globalThis.__RISUNEST_LUA_WORKER_PILOT__.createIdleWorkers()`)
    const idleMemory = await captureMemory('four-idle-workers', page, browser, appProcess.pid)
    await evaluate(page, `globalThis.__RISUNEST_LUA_WORKER_PILOT__.disposeIdleWorkers()`)
    const pilot = await evaluate(page, `globalThis.__RISUNEST_LUA_WORKER_PILOT__.run()`)
    const finalMemory = await captureMemory('after-pilot', page, browser, appProcess.pid)
    return {
      schemaVersion: 1,
      measuredAt: new Date().toISOString(),
      mode: options.mode,
      platform: {
        os: `${os.type()} ${os.release()}`,
        arch: os.arch(),
        node: process.version,
        userAgent: pilot.userAgent,
      },
      build: {
        realmDisabled: true,
        releaseTauri: options.mode === 'tauri',
        isolatedProfile: true,
      },
      pilot,
      memory: { baseline: baselineMemory, idleFourWorkers: idleMemory, final: finalMemory },
      gates: summarizePilotGates(pilot, baselineMemory, idleMemory),
      limitations: [
        'All fixtures are local and synthetic.',
        'Live RisuRealm and live provider access are disabled.',
        'Physical Android evidence is unavailable.',
        'Process RSS sampling includes the root and CDP-reported browser processes.',
      ],
    }
  }
  finally {
    page?.close()
    browser?.close()
    await stopProcess(appProcess)
    await stopProcess(helperProcess)
    assertSafeTemporaryDirectory(temporaryRoot)
    await rm(temporaryRoot, {
      recursive: true,
      force: true,
      maxRetries: 10,
      retryDelay: 200,
    })
  }
}

function parseArguments(argumentsList) {
  const options = { mode: 'browser', output: null, timeoutMs: DEFAULT_TIMEOUT_MS }
  for (let index = 0; index < argumentsList.length; index++) {
    const argument = argumentsList[index]
    if (argument === '--mode') {
      const mode = argumentsList[++index]
      if (mode !== 'browser' && mode !== 'tauri') throw new Error('--mode requires browser or tauri')
      options.mode = mode
    }
    else if (argument === '--output') options.output = argumentsList[++index]
    else if (argument === '--timeout-ms') options.timeoutMs = Number(argumentsList[++index])
    else if (argument === '--help' || argument === '-h') options.help = true
    else throw new Error(`Unknown argument: ${argument}`)
  }
  return options
}

function usage() {
  return [
    'Usage: node benchmarks/lua-worker-pilot/runner.mjs [options]',
    '',
    '  --mode browser|tauri   Pilot host, default browser.',
    '  --output <path>        Write the JSON evidence file.',
    '  --timeout-ms <number>  Build and CDP timeout, default 180000.',
  ].join(os.EOL)
}

function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds))
}

async function main() {
  const options = parseArguments(process.argv.slice(2))
  if (options.help) {
    process.stdout.write(`${usage()}${os.EOL}`)
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
  if (!result.gates.windowsPilotPassed) process.exitCode = 1
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}${os.EOL}`)
    process.exitCode = 1
  })
}
