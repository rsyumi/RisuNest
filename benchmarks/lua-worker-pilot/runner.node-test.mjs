import assert from 'node:assert/strict'
import test from 'node:test'
import {
  buildTauriProfileConfig,
  summarizePilotGates,
} from './runner.mjs'

test('builds an isolated Tauri profile around the pilot frontend', () => {
  const config = buildTauriProfileConfig({
    build: { beforeBuildCommand: 'pnpm tauribuild', frontendDist: '../dist' },
    bundle: { active: true },
    identifier: 'RisuNest',
    app: { windows: [{ title: 'RisuNest' }] },
    plugins: { updater: { endpoints: ['https://example.invalid/latest.json'] } },
  }, 9229, 'abc-123')

  assert.equal(config.build.frontendDist, '../dist-lua-worker-pilot')
  assert.match(config.build.beforeBuildCommand, /lua-worker-pilot\/vite\.config\.ts/)
  assert.equal(config.bundle.active, false)
  assert.equal(config.plugins.updater.endpoints.length, 0)
  assert.equal(config.identifier, 'RisuNest.luaworkerpilot.abc123')
  assert.match(config.app.windows[0].additionalBrowserArgs, /remote-debugging-port=9229/)
})

test('requires parity, termination, busy-time, latency, and RSS gates', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'mutation-reads' },
      { name: 'variables' },
      { name: 'stop-chat' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: {
      unsupported: { passed: true },
      contextWindow: { passed: true },
      memory: { passed: true },
    },
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: true,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 500 * 1024 * 1024,
      },
    },
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 530 * 1024 * 1024,
      },
    },
  )

  assert.equal(gates.semanticParity.passed, true)
  assert.equal(gates.termination.passed, true)
  assert.equal(gates.uiBusyTime.passed, true)
  assert.equal(gates.integratedP95.passed, true)
  assert.equal(gates.idleRss.passed, true)
  assert.equal(gates.windowsPilotPassed, true)
  assert.equal(gates.productionAdoptionEnabled, false)
  assert.deepEqual(gates.productionBlockers, [])
})

test('fails closed when boundary or process RSS evidence is missing', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'mutation-reads' },
      { name: 'variables' },
      { name: 'stop-chat' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: {},
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: true,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [],
        workingSetBytes: 0,
      },
    },
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [],
        workingSetBytes: 0,
      },
    },
  )

  assert.deepEqual(gates.supportedBoundaries.missingCases, [
    'unsupported',
    'contextWindow',
    'memory',
  ])
  assert.equal(gates.supportedBoundaries.passed, false)
  assert.equal(gates.idleRss.passed, false)
  assert.equal(gates.windowsPilotPassed, false)
  assert.deepEqual(gates.productionBlockers, [
    'Supported boundary evidence is incomplete or failed.',
    'Idle Worker RSS gate failed or has incomplete process samples.',
  ])
})

test('fails RSS when baseline and idle process populations differ', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'mutation-reads' },
      { name: 'variables' },
      { name: 'stop-chat' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: {
      unsupported: { passed: true },
      contextWindow: { passed: true },
      memory: { passed: true },
    },
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: true,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 500 * 1024 * 1024,
      },
    },
    {
      processMemory: {
        requestedProcessIds: [1, 3],
        sampledProcessIds: [1, 3],
        workingSetBytes: 530 * 1024 * 1024,
      },
    },
  )

  assert.equal(gates.idleRss.completeProcessSamples, false)
  assert.equal(gates.idleRss.passed, false)
  assert.equal(gates.windowsPilotPassed, false)
})

test('fails semantic parity when atomic error handling differs from production', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'mutation-reads' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: { unsupported: { passed: true } },
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: false,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 500 * 1024 * 1024,
      },
    },
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 530 * 1024 * 1024,
      },
    },
  )

  assert.equal(gates.semanticParity.passed, false)
  assert.equal(gates.windowsPilotPassed, false)
})

test('fails UI busy gate without the warm total-lag measurement', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'mutation-reads' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: { unsupported: { passed: true } },
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: true,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'maximum-timer-lag',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 500 * 1024 * 1024,
      },
    },
    {
      processMemory: {
        requestedProcessIds: [1, 2],
        sampledProcessIds: [1, 2],
        workingSetBytes: 530 * 1024 * 1024,
      },
    },
  )

  assert.equal(gates.uiBusyTime.passed, false)
  assert.equal(gates.windowsPilotPassed, false)
})

test('fails semantic parity when the mutation-read case is missing', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'variables' },
      { name: 'stop-chat' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: { unsupported: { passed: true } },
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: true,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    { processMemory: { workingSetBytes: 500 * 1024 * 1024 } },
    { processMemory: { workingSetBytes: 530 * 1024 * 1024 } },
  )

  assert.deepEqual(gates.semanticParity.missingCases, ['mutation-reads'])
  assert.equal(gates.semanticParity.passed, false)
})

test('requires variable and explicit stop parity cases', () => {
  const pilot = {
    parity: [
      { name: 'chat-reads' },
      { name: 'ordered-mutations' },
      { name: 'mutation-reads' },
    ],
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: {
      unsupported: { passed: true },
      contextWindow: { passed: true },
      memory: { passed: true },
    },
    atomicFailureComparison: {
      zeroPartialWorkerMutation: true,
      productionSemanticMatch: true,
    },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    {
      processMemory: {
        requestedProcessIds: [1],
        sampledProcessIds: [1],
        workingSetBytes: 500 * 1024 * 1024,
      },
    },
    {
      processMemory: {
        requestedProcessIds: [1],
        sampledProcessIds: [1],
        workingSetBytes: 530 * 1024 * 1024,
      },
    },
  )

  assert.deepEqual(gates.semanticParity.missingCases, ['variables', 'stop-chat'])
  assert.equal(gates.semanticParity.passed, false)
  assert.equal(gates.windowsPilotPassed, false)
})
