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
    identifier: 'co.aiclient.risu',
    app: { windows: [{ title: 'RisuAI' }] },
    plugins: { updater: { endpoints: ['https://example.invalid/latest.json'] } },
  }, 9229, 'abc-123')

  assert.equal(config.build.frontendDist, '../dist-lua-worker-pilot')
  assert.match(config.build.beforeBuildCommand, /lua-worker-pilot\/vite\.config\.ts/)
  assert.equal(config.bundle.active, false)
  assert.equal(config.plugins.updater.endpoints.length, 0)
  assert.equal(config.identifier, 'co.aiclient.risu.luaworkerpilot.abc123')
  assert.match(config.app.windows[0].additionalBrowserArgs, /remote-debugging-port=9229/)
})

test('requires parity, termination, busy-time, latency, and RSS gates', () => {
  const pilot = {
    parityMismatchCount: 0,
    globalIsolation: { passed: true },
    syntheticPromise: { passed: true },
    boundaries: {
      unsupported: { passed: true },
      contextWindow: { passed: true },
      memory: { passed: true },
    },
    atomicFailureComparison: { zeroPartialWorkerMutation: true },
    termination: { passed: true, p95Ms: 10 },
    performance: {
      main: { p95Ms: 100, busyTimeMs: 80 },
      worker: { p95Ms: 105, busyTimeMs: 4 },
    },
  }
  const gates = summarizePilotGates(
    pilot,
    { processMemory: { workingSetBytes: 500 * 1024 * 1024 } },
    { processMemory: { workingSetBytes: 530 * 1024 * 1024 } },
  )

  assert.equal(gates.semanticParity.passed, true)
  assert.equal(gates.termination.passed, true)
  assert.equal(gates.uiBusyTime.passed, true)
  assert.equal(gates.integratedP95.passed, true)
  assert.equal(gates.idleRss.passed, true)
  assert.equal(gates.windowsPilotPassed, true)
  assert.equal(gates.productionAdoptionEnabled, false)
})
