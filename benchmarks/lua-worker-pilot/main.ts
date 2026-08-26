import type { LuaWorkerHarnessClient } from '../../src/ts/process/luaWorkerHarness'
import {
  createLuaWorkerPilotClient,
  type LuaWorkerPilotClientOptions,
} from '../../src/ts/process/luaWorkerPilotClient'
import type {
  LuaWorkerBoundedContext,
  LuaWorkerInvocation,
  LuaWorkerJsonValue,
  LuaWorkerMode,
  LuaWorkerMutation,
  LuaWorkerPolicy,
} from '../../src/ts/process/luaWorkerProtocol'
import { runScripted } from '../../src/ts/process/scriptings'
import { DBState } from '../../src/ts/stores.svelte'

interface PilotChatMessage {
  role: 'user' | 'char'
  data: string
  time?: number
}

interface PilotState {
  chat: PilotChatMessage[]
  variables: Record<string, string>
  version: number
  stopped: boolean
}

interface ParityCase {
  name: string
  passed: boolean
  main: unknown
  worker: unknown
}

interface PilotApi {
  run(): Promise<unknown>
  createIdleWorkers(): Promise<{ count: number }>
  disposeIdleWorkers(): void
}

declare global {
  var __RISUNEST_LUA_WORKER_PILOT__: PilotApi | undefined
}

const status = document.querySelector<HTMLPreElement>('#status')!
const idleClients: LuaWorkerHarnessClient[] = []

function clone<T>(value: T): T {
  return structuredClone(value)
}

function percentile(values: number[], percentileValue: number): number {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.min(sorted.length - 1, Math.ceil(sorted.length * percentileValue) - 1)
  return sorted[Math.max(0, index)] ?? 0
}

async function sourceHash(source: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(source))
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join('')
}

async function createClient(
  ownerChaId: string,
  mode: LuaWorkerMode,
  source: string,
  options: Pick<LuaWorkerPilotClientOptions, 'syntheticLLMMain'> & { policy?: LuaWorkerPolicy } = {},
): Promise<LuaWorkerHarnessClient> {
  return createLuaWorkerPilotClient({
    engine: {
      ownerChaId,
      mode,
      exactSourceHash: await sourceHash(source),
    },
    source,
    policy: options.policy,
    syntheticLLMMain: options.syntheticLLMMain,
  })
}

function stateFromContext(context: LuaWorkerBoundedContext): PilotState {
  return {
    chat: clone(context.messages),
    variables: { ...context.chatVars },
    version: 1,
    stopped: false,
  }
}

function applyMutations(state: PilotState, mutations: LuaWorkerMutation[]): void {
  for (const mutation of mutations) {
    switch (mutation.type) {
      case 'setChatVar':
      case 'setChatVarChanged':
        state.variables[mutation.key] = mutation.value
        break
      case 'setChat': {
        const message = state.chat.at(mutation.index)
        if (message !== undefined) message.data = mutation.value
        break
      }
      case 'setChatRole': {
        const message = state.chat.at(mutation.index)
        if (message !== undefined) message.role = mutation.role
        break
      }
      case 'cutChat':
        state.chat = state.chat.slice(mutation.start, mutation.end)
        break
      case 'removeChat':
        state.chat.splice(mutation.index, 1)
        break
      case 'addChat':
        state.chat.push({ role: mutation.role, data: mutation.value })
        break
      case 'insertChat':
        state.chat.splice(mutation.index, 0, { role: mutation.role, data: mutation.value })
        break
      case 'stopChat':
        state.stopped = true
        break
    }
  }
}

async function invokeWorker(
  client: LuaWorkerHarnessClient,
  invocation: Omit<LuaWorkerInvocation, 'runtime'>,
  state: PilotState,
  timeoutMs?: number,
) {
  return client.invoke(invocation, {
    timeoutMs,
    commitMutations(expectedVersion, mutations) {
      if (expectedVersion !== state.version) return false
      applyMutations(state, mutations)
      return true
    },
  })
}

async function invokeMain(
  ownerChaId: string,
  mode: LuaWorkerMode,
  source: string,
  data: LuaWorkerJsonValue,
  context: LuaWorkerBoundedContext,
  lowLevelAccess = false,
) {
  const variables = { ...context.chatVars }
  const chat = { message: clone(context.messages) }
  const result = await runScripted(source, {
    char: { chaId: ownerChaId } as never,
    chat: chat as never,
    data: data as never,
    getVar: (key) => variables[key] ?? 'null',
    setVar: (key, value) => {
      if (variables[key] === value) return false
      variables[key] = value
      return true
    },
    lowLevelAccess,
    meta: {},
    mode,
  })
  return {
    res: result.res ?? null,
    stopSending: result.stopSending,
    chat: result.chat.message,
    variables,
  }
}

function sameValue(left: unknown, right: unknown): boolean {
  return canonicalJson(left) === canonicalJson(right)
}

function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== 'object') return JSON.stringify(value)
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  const record = value as Record<string, unknown>
  return `{${Object.keys(record).sort().map((key) => (
    `${JSON.stringify(key)}:${canonicalJson(record[key])}`
  )).join(',')}}`
}

async function parityCase(
  name: string,
  mode: LuaWorkerMode,
  source: string,
  data: LuaWorkerJsonValue,
  context: LuaWorkerBoundedContext,
  expectedCommittedStop = false,
): Promise<ParityCase> {
  const owner = `pilot-${name}`
  const main = await invokeMain(owner, mode, source, data, context)
  const client = await createClient(owner, mode, source)
  const state = stateFromContext(context)
  try {
    const workerResult = await invokeWorker(client, {
      mode,
      data,
      meta: {},
      contextVersion: state.version,
      boundedContext: context,
    }, state)
    const worker = {
      res: workerResult.res,
      stopSending: workerResult.stopSending,
      chat: state.chat,
      variables: state.variables,
      ...(expectedCommittedStop ? { committedStop: state.stopped } : {}),
    }
    const expectedMain = expectedCommittedStop
      ? { ...main, committedStop: true }
      : main
    return { name, passed: sameValue(expectedMain, worker), main: expectedMain, worker }
  }
  finally {
    client.dispose()
  }
}

async function runParityCorpus(): Promise<ParityCase[]> {
  const emptyContext: LuaWorkerBoundedContext = {
    messages: [],
    startIndex: 0,
    totalMessages: 0,
  }
  const cases: ParityCase[] = []
  cases.push(await parityCase(
    'pure-cpu',
    'editInput',
    `
      function fibonacci(value)
        if value < 2 then return value end
        return fibonacci(value - 1) + fibonacci(value - 2)
      end
      listenEdit('editInput', function(id, value, meta) return fibonacci(10) end)
    `,
    null,
    emptyContext,
  ))
  cases.push(await parityCase(
    'values',
    'editInput',
    `listenEdit('editInput', function(id, value, meta) return value end)`,
    { dense: [1, false, 'value'], object: { empty: '', falseValue: false, zero: 0 }, nullValue: null },
    emptyContext,
  ))

  for (const mode of ['editRequest', 'editInput', 'editOutput', 'editDisplay'] as const) {
    cases.push(await parityCase(
      `listener-${mode}`,
      mode,
      `
        listenEdit('${mode}', function(id, value) table.insert(value, 'first') return value end)
        listenEdit('${mode}', function(id, value) table.insert(value, 'second') return value end)
      `,
      [],
      emptyContext,
    ))
  }

  const chatContext: LuaWorkerBoundedContext = {
    messages: [
      { role: 'user', data: 'zero', time: 10 },
      { role: 'char', data: 'one' },
      { role: 'user', data: 'two', time: 30 },
    ],
    startIndex: 0,
    totalMessages: 3,
  }
  cases.push(await parityCase(
    'chat-reads',
    'editInput',
    `
      listenEdit('editInput', function(id, value)
        return {
          first = getChat(id, 0),
          last = getChat(id, -1),
          recent = getRecentChats(id, 2),
          length = getChatLength(id)
        }
      end)
    `,
    null,
    chatContext,
  ))
  cases.push(await parityCase(
    'ordered-mutations',
    'editInput',
    `
      listenEdit('editInput', function(id, value)
        setChat(id, 0, 'edited')
        setChatRole(id, 0, 'char')
        removeChat(id, 1)
        addChat(id, 'user', 'tail')
        return false
      end)
    `,
    null,
    chatContext,
  ))
  cases.push(await parityCase(
    'mutation-reads',
    'editInput',
    `
      listenEdit('editInput', function(id, value)
        setChat(id, 0, 'edited')
        setChatRole(id, 0, 'char')
        insertChat(id, 1, 'user', 'inserted')
        removeChat(id, 2)
        addChat(id, 'char', 'tail')
        cutChat(id, 1, 4)
        return getRecentChats(id, getChatLength(id))
      end)
    `,
    null,
    chatContext,
  ))
  const previousGlobalVariables = DBState.db.globalChatVariables
  DBState.db.globalChatVariables = {
    ...(previousGlobalVariables ?? {}),
    __risunest_worker_pilot_global: 'global-value',
  }
  try {
    cases.push(await parityCase(
      'variables',
      'editInput',
      `
        listenEdit('editInput', function(id, value)
          local previous = getState(id, 'profile')
          local changed = setStateChanged(id, 'profile', { level = 2 })
          setState(id, 'enabled', true)
          return {
            previous = previous,
            changed = changed,
            current = getState(id, 'profile'),
            enabled = getState(id, 'enabled'),
            global = getGlobalVar(id, '__risunest_worker_pilot_global')
          }
        end)
      `,
      null,
      {
        messages: [],
        startIndex: 0,
        totalMessages: 0,
        chatVars: { __profile: JSON.stringify({ level: 1 }) },
        globalVars: { __risunest_worker_pilot_global: 'global-value' },
      },
    ))
  }
  finally {
    DBState.db.globalChatVariables = previousGlobalVariables
  }
  cases.push(await parityCase(
    'stop-chat',
    'editInput',
    `
      listenEdit('editInput', function(id, value)
        stopChat(id)
        return 'stopped'
      end)
    `,
    null,
    emptyContext,
    true,
  ))
  return cases
}

async function runGlobalIsolation() {
  const source = `
    counter = 0
    listenEdit('editInput', function(id, value) counter = counter + 1 return counter end)
    listenEdit('editOutput', function(id, value) counter = counter + 1 return counter end)
  `
  const context: LuaWorkerBoundedContext = { messages: [], startIndex: 0, totalMessages: 0 }
  const input = await createClient('pilot-global-a', 'editInput', source)
  const otherOwner = await createClient('pilot-global-b', 'editInput', source)
  const otherMode = await createClient('pilot-global-a', 'editOutput', source)
  try {
    const invoke = (client: LuaWorkerHarnessClient, mode: LuaWorkerMode) => invokeWorker(client, {
      mode,
      data: null,
      meta: {},
      contextVersion: 1,
      boundedContext: context,
    }, stateFromContext(context))
    const values = [
      (await invoke(input, 'editInput')).res,
      (await invoke(input, 'editInput')).res,
      (await invoke(otherOwner, 'editInput')).res,
      (await invoke(otherMode, 'editOutput')).res,
    ]
    return { passed: sameValue(values, [1, 2, 1, 1]), values }
  }
  finally {
    input.dispose()
    otherOwner.dispose()
    otherMode.dispose()
  }
}

async function runSyntheticPromise() {
  const source = `
    listenEdit('editInput', function(id, value)
      local response = LLM(id, {{ role = 'user', content = 'synthetic fixture' }})
      return response.result
    end)
  `
  let hostArgs: LuaWorkerJsonValue | undefined
  const client = await createClient('pilot-promise', 'editInput', source, {
    syntheticLLMMain: async (args) => {
      hostArgs = args
      return { success: true, result: 'synthetic LLM response' }
    },
  })
  const context: LuaWorkerBoundedContext = { messages: [], startIndex: 0, totalMessages: 0 }
  try {
    const result = await invokeWorker(client, {
      mode: 'editInput',
      lowLevelAccess: true,
      data: null,
      meta: {},
      contextVersion: 1,
      boundedContext: context,
    }, stateFromContext(context))
    const expectedArgs = {
      prompt: [{ role: 'user', content: 'synthetic fixture' }],
      useMultimodal: false,
      options: [],
    }
    return {
      passed: result.res === 'synthetic LLM response' && sameValue(hostArgs, expectedArgs),
      result: result.res,
      hostArgs,
      oracle: 'scriptings.test.ts deterministic mocked LLM golden',
    }
  }
  finally {
    client.dispose()
  }
}

async function expectWorkerError(
  name: string,
  source: string,
  context: LuaWorkerBoundedContext,
  category: string,
  policy?: LuaWorkerPolicy,
  timeoutMs?: number,
) {
  const client = await createClient(`pilot-${name}`, 'editInput', source, { policy })
  const startedAt = performance.now()
  try {
    await invokeWorker(client, {
      mode: 'editInput',
      data: null,
      meta: {},
      contextVersion: 1,
      boundedContext: context,
    }, stateFromContext(context), timeoutMs)
    return { name, passed: false, category: 'success', elapsedMs: performance.now() - startedAt }
  }
  catch (error) {
    const actualCategory = (error as { category?: string }).category ?? 'unknown'
    return {
      name,
      passed: actualCategory === category,
      category: actualCategory,
      elapsedMs: performance.now() - startedAt,
    }
  }
  finally {
    client.dispose()
  }
}

async function runBoundaries() {
  const empty: LuaWorkerBoundedContext = { messages: [], startIndex: 0, totalMessages: 0 }
  const unsupported = await expectWorkerError(
    'unsupported',
    `listenEdit('editInput', function(id) alertNormal(id, 'blocked') end)`,
    empty,
    'lua_worker_unsupported_callback',
  )
  const contextWindow = await expectWorkerError(
    'context-window',
    `listenEdit('editInput', function(id) return getChat(id, 0) end)`,
    {
      messages: [{ role: 'char', data: 'tail' }],
      startIndex: 1,
      totalMessages: 2,
    },
    'lua_worker_context_window',
  )
  const memory = await expectWorkerError(
    'memory',
    `
      listenEdit('editInput', function(id)
        local allocations = {}
        for index = 1, 200000 do allocations[index] = string.rep('x', 64) end
        return #allocations
      end)
    `,
    empty,
    'lua_worker_memory',
    { memoryBytes: 512 * 1024, cpuDeadlineMs: 2_000 },
  )
  return { unsupported, contextWindow, memory }
}

async function runAtomicFailureComparison() {
  const source = `
    listenEdit('editInput', function(id)
      setChat(id, 0, 'mutated-before-error')
      error('synthetic handler failure')
    end)
  `
  const context: LuaWorkerBoundedContext = {
    messages: [{ role: 'user', data: 'before-error' }],
    startIndex: 0,
    totalMessages: 1,
  }
  const main = await invokeMain('pilot-handler-error', 'editInput', source, null, context)
  const client = await createClient('pilot-handler-error', 'editInput', source)
  const workerState = stateFromContext(context)
  let workerCategory = 'success'
  try {
    await invokeWorker(client, {
      mode: 'editInput',
      data: null,
      meta: {},
      contextVersion: 1,
      boundedContext: context,
    }, workerState)
  }
  catch (error) {
    workerCategory = (error as { category?: string }).category ?? 'unknown'
  }
  finally {
    client.dispose()
  }
  return {
    mainChat: main.chat,
    workerChat: workerState.chat,
    workerCategory,
    zeroPartialWorkerMutation: sameValue(workerState.chat, context.messages),
    productionSemanticMatch: sameValue(main.chat, workerState.chat),
  }
}

async function measureTimerBusyTime(operation: () => Promise<unknown>) {
  const intervalMs = 10
  let previous = performance.now()
  let maximumLagMs = 0
  let totalLagMs = 0
  const timer = setInterval(() => {
    const current = performance.now()
    const lag = Math.max(0, current - previous - intervalMs)
    maximumLagMs = Math.max(maximumLagMs, lag)
    totalLagMs += lag
    previous = current
  }, intervalMs)
  await new Promise((resolve) => setTimeout(resolve, 25))
  const startedAt = performance.now()
  await operation()
  const elapsedMs = performance.now() - startedAt
  await new Promise((resolve) => setTimeout(resolve, 25))
  clearInterval(timer)
  return { elapsedMs, maximumLagMs, totalLagMs }
}

async function runPerformance() {
  const source = `
    listenEdit('editInput', function(id, value)
      local total = 0
      for index = 1, 8000000 do total = total + index end
      return total
    end)
  `
  const context: LuaWorkerBoundedContext = { messages: [], startIndex: 0, totalMessages: 0 }
  const client = await createClient('pilot-performance', 'editInput', source)
  const workerSamples: number[] = []
  const mainSamples: number[] = []
  try {
    for (let index = 0; index < 11; index++) {
      const mainStarted = performance.now()
      await invokeMain('pilot-performance-main', 'editInput', source, null, context)
      mainSamples.push(performance.now() - mainStarted)
      const workerStarted = performance.now()
      await invokeWorker(client, {
        mode: 'editInput',
        data: null,
        meta: {},
        contextVersion: 1,
        boundedContext: context,
      }, stateFromContext(context))
      workerSamples.push(performance.now() - workerStarted)
    }
    const mainBusy = await measureTimerBusyTime(() => invokeMain(
      'pilot-performance-main',
      'editInput',
      source,
      null,
      context,
    ))
    const workerBusy = await measureTimerBusyTime(() => invokeWorker(client, {
      mode: 'editInput',
      data: null,
      meta: {},
      contextVersion: 1,
      boundedContext: context,
    }, stateFromContext(context)))
    const mainWarm = mainSamples.slice(1)
    const workerWarm = workerSamples.slice(1)
    return {
      samples: 10,
      uiBusyMeasurement: 'warm-total-timer-lag-v1',
      main: {
        p50Ms: percentile(mainWarm, 0.5),
        p95Ms: percentile(mainWarm, 0.95),
        busyTimeMs: mainBusy.totalLagMs,
      },
      worker: {
        p50Ms: percentile(workerWarm, 0.5),
        p95Ms: percentile(workerWarm, 0.95),
        busyTimeMs: workerBusy.totalLagMs,
      },
    }
  }
  finally {
    client.dispose()
  }
}

async function runTermination() {
  const source = `listenEdit('editInput', function(id) while true do end end)`
  const context: LuaWorkerBoundedContext = { messages: [], startIndex: 0, totalMessages: 0 }
  const samples: number[] = []
  const categories: string[] = []
  for (let index = 0; index < 11; index++) {
    const result = await expectWorkerError(
      `termination-${index}`,
      source,
      context,
      'lua_worker_timeout',
      { memoryBytes: 64 * 1024 * 1024, cpuDeadlineMs: 100 },
      100,
    )
    samples.push(Math.max(0, result.elapsedMs - 100))
    categories.push(result.category)
  }
  return {
    categories,
    p50Ms: percentile(samples.slice(1), 0.5),
    p95Ms: percentile(samples.slice(1), 0.95),
    passed: categories.every((category) => category === 'lua_worker_timeout')
      && percentile(samples.slice(1), 0.95) <= 100,
  }
}

async function runPilot() {
  status.textContent = 'running'
  const parity = await runParityCorpus()
  const result = {
    schemaVersion: 1,
    userAgent: navigator.userAgent,
    parity,
    parityMismatchCount: parity.filter((entry) => !entry.passed).length,
    globalIsolation: await runGlobalIsolation(),
    syntheticPromise: await runSyntheticPromise(),
    boundaries: await runBoundaries(),
    atomicFailureComparison: await runAtomicFailureComparison(),
    termination: await runTermination(),
    performance: await runPerformance(),
  }
  status.textContent = JSON.stringify(result, null, 2)
  return result
}

async function createIdleWorkers() {
  const source = `listenEdit('editInput', function(id, value) return value end)`
  const context: LuaWorkerBoundedContext = { messages: [], startIndex: 0, totalMessages: 0 }
  for (let index = 0; index < 4; index++) {
    const client = await createClient(`pilot-idle-${index}`, 'editInput', source)
    await invokeWorker(client, {
      mode: 'editInput',
      data: null,
      meta: {},
      contextVersion: 1,
      boundedContext: context,
    }, stateFromContext(context))
    idleClients.push(client)
  }
  return { count: idleClients.length }
}

function disposeIdleWorkers() {
  for (const client of idleClients.splice(0)) client.dispose()
}

globalThis.__RISUNEST_LUA_WORKER_PILOT__ = {
  run: runPilot,
  createIdleWorkers,
  disposeIdleWorkers,
}
status.textContent = 'ready'
