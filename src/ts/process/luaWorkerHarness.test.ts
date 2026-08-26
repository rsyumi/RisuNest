import { describe, expect, it, vi } from 'vitest'
import {
  createLuaWorkerEngineKey,
  DEFAULT_LUA_WORKER_TIMEOUT_MS,
  DEFAULT_LUA_WORKER_POLICY,
  MAX_LUA_WORKER_CONTEXT_BYTES,
  MAX_LUA_WORKER_CONTEXT_MESSAGES,
  MAX_LUA_WORKER_ERROR_BYTES,
  MAX_LUA_WORKER_ERROR_MESSAGE_BYTES,
  MAX_LUA_WORKER_HOST_ARGUMENT_BYTES,
  MAX_LUA_WORKER_HOST_CALL_ENVELOPE_BYTES,
  MAX_LUA_WORKER_RESULT_BYTES,
  MAX_LUA_WORKER_RESULT_ENVELOPE_BYTES,
  MAX_LUA_WORKER_METRIC_KEY_BYTES,
  MAX_LUA_WORKER_METRICS,
  MAX_LUA_WORKER_METRICS_BYTES,
  MAX_LUA_WORKER_MUTATIONS,
  MAX_LUA_WORKER_MUTATION_BYTES,
  MAX_LUA_WORKER_HOST_CALLS,
  MAX_LUA_WORKER_HOST_RESPONSE_BYTES,
  MAX_LUA_WORKER_SOURCE_BYTES,
  LUA_WORKER_PROTOCOL_LIMITS,
  getLuaWorkerContextLength,
  readLuaWorkerContextMessage,
  readLuaWorkerRecentMessages,
  LuaWorkerHarnessError,
  LuaWorkerHarnessClient as ProductionLuaWorkerHarnessClient,
  type LuaWorkerHarnessOptions,
  type LuaWorkerHostMessage,
  type LuaWorkerLike,
  type LuaWorkerRequest,
} from './luaWorkerHarness'

type TestEngineMode = 'editRequest' | 'editInput' | 'editOutput' | 'editDisplay'
type TestHarnessOptions = Omit<LuaWorkerHarnessOptions, 'engine'> & {
  engineKey: string
  engineMode?: TestEngineMode
}

class LuaWorkerHarnessClient extends ProductionLuaWorkerHarnessClient {
  constructor({ engineKey, engineMode = 'editInput', ...options }: TestHarnessOptions) {
    let engine = {
      ownerChaId: 'fixture-owner',
      mode: engineMode,
      exactSourceHash: engineKey,
    }
    try {
      const parsed = JSON.parse(engineKey) as unknown
      if (Array.isArray(parsed) && parsed.length === 3
        && typeof parsed[0] === 'string'
        && (parsed[1] === 'editRequest' || parsed[1] === 'editInput'
          || parsed[1] === 'editOutput' || parsed[1] === 'editDisplay')
        && typeof parsed[2] === 'string') {
        engine = {
          ownerChaId: parsed[0],
          mode: parsed[1],
          exactSourceHash: parsed[2],
        }
      }
    }
    catch {
      // Short fixture labels become deterministic exact-source hashes.
    }
    super({ ...options, engine })
  }
}

class FakeLuaWorker implements LuaWorkerLike {
  readonly requests: LuaWorkerRequest[] = []
  terminated = false
  private readonly messageListeners = new Set<(event: MessageEvent<LuaWorkerHostMessage>) => void>()
  private readonly errorListeners = new Set<(event: ErrorEvent) => void>()

  postMessage(message: LuaWorkerRequest): void {
    this.requests.push(message)
  }

  terminate(): void {
    this.terminated = true
  }

  addEventListener(type: 'message' | 'error', listener: EventListener): void {
    if (type === 'message') {
      this.messageListeners.add(listener as (event: MessageEvent<LuaWorkerHostMessage>) => void)
    }
    else {
      this.errorListeners.add(listener as (event: ErrorEvent) => void)
    }
  }

  removeEventListener(type: 'message' | 'error', listener: EventListener): void {
    if (type === 'message') {
      this.messageListeners.delete(listener as (event: MessageEvent<LuaWorkerHostMessage>) => void)
    }
    else {
      this.errorListeners.delete(listener as (event: ErrorEvent) => void)
    }
  }

  respond(message: LuaWorkerHostMessage): void {
    for (const listener of this.messageListeners) {
      listener({ data: message } as MessageEvent<LuaWorkerHostMessage>)
    }
  }

  crash(message = 'synthetic Worker crash'): void {
    for (const listener of this.errorListeners) {
      listener({ message } as ErrorEvent)
    }
  }

  captureMessageDispatcher(): (message: LuaWorkerHostMessage) => void {
    const listeners = [...this.messageListeners]
    return (message) => {
      for (const listener of listeners) {
        listener({ data: message } as MessageEvent<LuaWorkerHostMessage>)
      }
    }
  }

  get listenerCount(): number {
    return this.messageListeners.size + this.errorListeners.size
  }
}

class RegisterFailureLuaWorker extends FakeLuaWorker {
  override postMessage(message: LuaWorkerRequest): void {
    super.postMessage(message)
    if (message.type === 'register') {
      throw new Error('synthetic registration failure')
    }
  }
}

class InvokeFailureLuaWorker extends FakeLuaWorker {
  override postMessage(message: LuaWorkerRequest): void {
    super.postMessage(message)
    if (message.type === 'invoke') {
      throw new Error('synthetic invoke failure')
    }
  }
}

class HostResultFailureLuaWorker extends FakeLuaWorker {
  override postMessage(message: LuaWorkerRequest): void {
    super.postMessage(message)
    if (message.type === 'hostResult') {
      throw new Error('synthetic host result failure')
    }
  }
}

function invocation() {
  return {
    boundedContext: {
      messages: [{ role: 'user' as const, data: 'existing' }],
      startIndex: 0,
      totalMessages: 1,
    },
    contextVersion: 4,
    data: 'input',
    meta: { source: 'fixture' },
    mode: 'editInput' as const,
  }
}

describe('LuaWorkerHarnessClient', () => {
  it('builds engine keys from owner, mode, and exact source hash', () => {
    expect(createLuaWorkerEngineKey('owner', 'editInput', 'sha256:abc')).toBe(
      '["owner","editInput","sha256:abc"]',
    )
  })

  it('exposes absolute bounded-window reads and rejects unavailable chat access', () => {
    const middleWindow = {
      messages: [
        { role: 'user' as const, data: 'absolute eight' },
        { role: 'char' as const, data: 'absolute nine', time: 9 },
      ],
      startIndex: 8,
      totalMessages: 12,
    }

    expect(getLuaWorkerContextLength(middleWindow)).toBe(12)
    expect(readLuaWorkerContextMessage(middleWindow, 8)).toEqual({
      role: 'user',
      data: 'absolute eight',
      time: 0,
    })
    expect(() => readLuaWorkerContextMessage(middleWindow, 0)).toThrowError(
      expect.objectContaining({ category: 'lua_worker_context_window' }),
    )
    expect(() => readLuaWorkerContextMessage(middleWindow, -1)).toThrowError(
      expect.objectContaining({ category: 'lua_worker_context_window' }),
    )
    expect(readLuaWorkerRecentMessages(middleWindow, 0)).toEqual([])
    expect(() => readLuaWorkerRecentMessages(middleWindow, 2)).toThrowError(
      expect.objectContaining({ category: 'lua_worker_context_window' }),
    )

    const tailWindow = {
      ...middleWindow,
      startIndex: 10,
    }
    expect(readLuaWorkerRecentMessages(tailWindow, 2)).toEqual([
      { role: 'user', data: 'absolute eight', time: 0 },
      { role: 'char', data: 'absolute nine', time: 9 },
    ])
    expect(readLuaWorkerContextMessage(tailWindow, -1)).toEqual({
      role: 'char',
      data: 'absolute nine',
      time: 9,
    })
  })

  it('rejects invocation mode that differs from the registered engine descriptor', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'mode-bound-engine',
      engineMode: 'editInput',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      mode: 'editOutput',
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_mode' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('snapshots the exact engine descriptor when the client is created', async () => {
    const worker = new FakeLuaWorker()
    const engine: LuaWorkerHarnessOptions['engine'] = {
      ownerChaId: 'original-owner',
      mode: 'editInput',
      exactSourceHash: 'original-hash',
    }
    const client = new ProductionLuaWorkerHarnessClient({
      engine,
      source: 'fixture source',
      workerFactory: () => worker,
    })
    engine.ownerChaId = 'mutated-owner'
    engine.mode = 'editOutput'
    engine.exactSourceHash = 'mutated-hash'

    const pending = client.invoke(invocation(), { commitMutations: () => true })
    expect(worker.requests[0]).toMatchObject({
      type: 'register',
      engineKey: '["original-owner","editInput","original-hash"]',
    })
    const request = worker.requests.find((message) => message.type === 'invoke')!
    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'complete',
      stopSending: false,
    })
    await expect(pending).resolves.toMatchObject({ res: 'complete' })
  })

  it('registers one engine and routes its active invocation result', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: '["owner","editInput","source-hash"]',
      source: 'listenEdit("editInput", function(id, value) return value end)',
      workerFactory: () => worker,
    })

    const pending = client.invoke(invocation(), { commitMutations })

    expect(worker.requests.map((message) => message.type)).toEqual(['register', 'invoke'])
    expect(worker.requests[0]).toEqual({
      type: 'register',
      engineKey: '["owner","editInput","source-hash"]',
      limits: LUA_WORKER_PROTOCOL_LIMITS,
      source: 'listenEdit("editInput", function(id, value) return value end)',
      policy: DEFAULT_LUA_WORKER_POLICY,
    })
    const request = worker.requests[1]
    if (request.type !== 'invoke') {
      throw new Error('Expected an invoke request')
    }
    worker.respond({
      type: 'result',
      id: request.id,
      metrics: { handlerCpuMs: 2 },
      orderedMutations: [
        { type: 'setChatVar', key: 'first', value: 'one' },
        { type: 'addChat', role: 'char', value: 'second' },
      ],
      res: 'output',
      stopSending: false,
    })

    await expect(pending).resolves.toEqual({
      metrics: { handlerCpuMs: 2 },
      res: 'output',
      stopSending: false,
    })
    expect(commitMutations).toHaveBeenCalledWith(4, [
      { type: 'setChatVar', key: 'first', value: 'one' },
      { type: 'addChat', role: 'char', value: 'second' },
    ])
  })

  it('keeps one active invocation and starts pending work in FIFO order', async () => {
    const worker = new FakeLuaWorker()
    const client = new LuaWorkerHarnessClient({
      engineKey: 'fifo-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const commitMutations = () => true
    const first = client.invoke(invocation(), { commitMutations })
    const second = client.invoke({ ...invocation(), data: 'second' }, { commitMutations })
    const third = client.invoke({ ...invocation(), data: 'third' }, { commitMutations })

    expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(1)
    const respondToActive = (res: string) => {
      const request = worker.requests.at(-1)
      if (request?.type !== 'invoke') {
        throw new Error('Expected an invoke request')
      }
      worker.respond({
        type: 'result',
        id: request.id,
        metrics: {},
        orderedMutations: [],
        res,
        stopSending: false,
      })
    }

    respondToActive('first-result')
    await expect(first).resolves.toMatchObject({ res: 'first-result' })
    await vi.waitFor(() => {
      expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(2)
    })
    respondToActive('second-result')
    await expect(second).resolves.toMatchObject({ res: 'second-result' })
    await vi.waitFor(() => {
      expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(3)
    })
    respondToActive('third-result')
    await expect(third).resolves.toMatchObject({ res: 'third-result' })

    expect(worker.requests
      .filter((message) => message.type === 'invoke')
      .map((message) => message.data)).toEqual(['input', 'second', 'third'])
  })

  it('rejects work beyond the active invocation and eight pending entries', async () => {
    const worker = new FakeLuaWorker()
    const client = new LuaWorkerHarnessClient({
      engineKey: 'bounded-queue-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const accepted = Array.from({ length: 9 }, (_, index) => client.invoke(
      { ...invocation(), data: `accepted-${index}` },
      { commitMutations: () => true },
    ))

    const overflow = client.invoke(
      { ...invocation(), data: 'overflow' },
      { commitMutations: () => true },
    )

    await expect(overflow).rejects.toEqual(expect.objectContaining({
      category: 'lua_worker_queue_limit',
    }))
    await expect(overflow).rejects.toBeInstanceOf(LuaWorkerHarnessError)

    for (let index = 0; index < accepted.length; index++) {
      const requests = worker.requests.filter((message) => message.type === 'invoke')
      const request = requests[index]
      worker.respond({
        type: 'result',
        id: request.id,
        metrics: {},
        orderedMutations: [],
        res: `accepted-${index}`,
        stopSending: false,
      })
      await accepted[index]
      if (index < accepted.length - 1) {
        await vi.waitFor(() => {
          expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(index + 2)
        })
      }
    }
  })

  it('rejects oversized Lua source before creating a Worker', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'oversized-source-engine',
      source: 'a'.repeat(MAX_LUA_WORKER_SOURCE_BYTES + 1),
      workerFactory,
    })

    await expect(client.invoke(invocation(), {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_source_limit' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('rejects non-Lua runtime invocations before creating a Worker', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'python-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      runtime: 'py',
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_runtime' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('rejects modes outside the four edit listener modes', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'unsupported-mode-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      mode: 'start',
    } as never, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_mode' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('rejects low-level access without the synthetic LLM capability', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'low-level-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_capability' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('rejects bounded contexts beyond 256 messages before creating a Worker', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'message-limit-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      boundedContext: {
        messages: Array.from({ length: MAX_LUA_WORKER_CONTEXT_MESSAGES + 1 }, () => ({
          data: 'fixture',
          role: 'user',
        })),
        startIndex: 0,
        totalMessages: MAX_LUA_WORKER_CONTEXT_MESSAGES + 1,
      },
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_context_limit' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('rejects oversized canonical data, meta, and context before creating a Worker', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'context-byte-limit-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      data: 'x'.repeat(MAX_LUA_WORKER_CONTEXT_BYTES),
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_context_limit' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('terminates on timeout and rejects active and queued work without mutations or retry', async () => {
    vi.useFakeTimers()
    try {
      const worker = new FakeLuaWorker()
      const commitMutations = vi.fn(() => true)
      const client = new LuaWorkerHarnessClient({
        engineKey: 'timeout-engine',
        source: 'while true do end',
        workerFactory: () => worker,
      })
      const active = client.invoke(invocation(), { commitMutations })
      const queued = client.invoke({ ...invocation(), data: 'queued' }, { commitMutations })
      const activeOutcome = active.catch((error) => error)
      const queuedOutcome = queued.catch((error) => error)

      await vi.advanceTimersByTimeAsync(DEFAULT_LUA_WORKER_TIMEOUT_MS - 1)
      expect(worker.terminated).toBe(false)
      await vi.advanceTimersByTimeAsync(1)

      await expect(activeOutcome).resolves.toEqual(expect.objectContaining({
        category: 'lua_worker_timeout',
      }))
      await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
        category: 'lua_worker_timeout',
      }))
      expect(worker.terminated).toBe(true)
      expect(worker.listenerCount).toBe(0)
      expect(commitMutations).not.toHaveBeenCalled()
      expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(1)
    }
    finally {
      vi.useRealTimers()
    }
  })

  it('terminates active work on abort and removes every abort and Worker listener', async () => {
    const worker = new FakeLuaWorker()
    const controller = new AbortController()
    const removeAbortListener = vi.spyOn(controller.signal, 'removeEventListener')
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'abort-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const active = client.invoke(invocation(), {
      commitMutations,
      signal: controller.signal,
    })
    const queued = client.invoke({ ...invocation(), data: 'queued' }, { commitMutations })
    const activeOutcome = active.catch((error) => error)
    const queuedOutcome = queued.catch((error) => error)

    controller.abort()

    await expect(activeOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_abort',
    }))
    await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_abort',
    }))
    expect(removeAbortListener).toHaveBeenCalledWith('abort', expect.any(Function))
    expect(worker.terminated).toBe(true)
    expect(worker.listenerCount).toBe(0)
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('removes an aborted queued invocation without terminating active work', async () => {
    const worker = new FakeLuaWorker()
    const controller = new AbortController()
    const removeAbortListener = vi.spyOn(controller.signal, 'removeEventListener')
    const client = new LuaWorkerHarnessClient({
      engineKey: 'queued-abort-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const options = { commitMutations: () => true }
    const first = client.invoke(invocation(), options)
    const aborted = client.invoke({ ...invocation(), data: 'aborted' }, {
      ...options,
      signal: controller.signal,
    })
    const third = client.invoke({ ...invocation(), data: 'third' }, options)
    const abortedOutcome = aborted.catch((error) => error)

    controller.abort()

    await expect(abortedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_abort',
    }))
    expect(worker.terminated).toBe(false)
    expect(removeAbortListener).toHaveBeenCalledWith('abort', expect.any(Function))

    const firstRequest = worker.requests.find((message) => message.type === 'invoke')!
    worker.respond({
      type: 'result',
      id: firstRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'first',
      stopSending: false,
    })
    await first
    await vi.waitFor(() => {
      expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(2)
    })
    const thirdRequest = worker.requests.at(-1)!
    if (thirdRequest.type !== 'invoke') {
      throw new Error('Expected the third invocation')
    }
    expect(thirdRequest.data).toBe('third')
    worker.respond({
      type: 'result',
      id: thirdRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'third',
      stopSending: false,
    })
    await expect(third).resolves.toMatchObject({ res: 'third' })
  })

  it('rejects stale context without applying any mutation from the result batch', async () => {
    const worker = new FakeLuaWorker()
    const applied: unknown[] = []
    const commitMutations = vi.fn((expectedVersion, mutations) => {
      const currentVersion = 5
      if (currentVersion !== expectedVersion) {
        return false
      }
      applied.push(...mutations)
      return true
    })
    const client = new LuaWorkerHarnessClient({
      engineKey: 'stale-context-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{ type: 'setChat', index: 0, value: 'changed' }],
      res: 'ignored',
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_stale_context',
    }))
    expect(commitMutations).toHaveBeenCalledWith(4, [
      { type: 'setChat', index: 0, value: 'changed' },
    ])
    expect(applied).toEqual([])
  })

  it('rejects mutation indices outside the admitted absolute context window', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'mutation-window-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      boundedContext: {
        messages: [
          { role: 'user', data: 'five' },
          { role: 'char', data: 'six' },
        ],
        startIndex: 5,
        totalMessages: 10,
      },
    }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{ type: 'setChat', index: 2, value: 'outside' }],
      res: null,
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_context_window',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('terminates on a malformed result and applies zero mutations', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'malformed-result-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{ type: 'setChat', index: 0, value: 'must-not-apply' }],
      res: 'ignored',
      stopSending: 'false',
    } as never)

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_malformed_result',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('discards a result beyond the 2 MiB output limit', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'output-limit-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'x'.repeat(MAX_LUA_WORKER_RESULT_BYTES),
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_output_limit',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('bounds the complete result envelope even when each field is below its own limit', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'result-envelope-limit-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{
        type: 'setChatVar',
        key: 'bounded',
        value: 'm'.repeat(400 * 1024),
      }],
      res: 'r'.repeat(MAX_LUA_WORKER_RESULT_ENVELOPE_BYTES - 400 * 1024),
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_output_limit',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('bounds metric count, key bytes, and canonical metric bytes before mutation CAS', async () => {
    const cases = [
      Object.fromEntries(Array.from({ length: MAX_LUA_WORKER_METRICS + 1 }, (_, index) => (
        [`metric-${index}`, index]
      ))),
      { ['k'.repeat(MAX_LUA_WORKER_METRIC_KEY_BYTES + 1)]: 1 },
      Object.fromEntries(Array.from({ length: 20 }, (_, index) => (
        [`${index}-${'k'.repeat(240)}`, index]
      ))),
    ]
    expect(new TextEncoder().encode(JSON.stringify(cases[2])).byteLength).toBeGreaterThan(
      MAX_LUA_WORKER_METRICS_BYTES,
    )

    for (const metrics of cases) {
      const worker = new FakeLuaWorker()
      const commitMutations = vi.fn(() => true)
      const client = new LuaWorkerHarnessClient({
        engineKey: `metrics-envelope-${cases.indexOf(metrics)}`,
        source: 'fixture source',
        workerFactory: () => worker,
      })
      const pending = client.invoke(invocation(), { commitMutations })
      const outcome = pending.catch((error) => error)
      const request = worker.requests.find((message) => message.type === 'invoke')!
      worker.respond({
        type: 'result',
        id: request.id,
        metrics,
        orderedMutations: [],
        res: null,
        stopSending: false,
      })

      await expect(outcome).resolves.toEqual(expect.objectContaining({
        category: 'lua_worker_output_limit',
      }))
      expect(commitMutations).not.toHaveBeenCalled()
      expect(worker.terminated).toBe(true)
    }
  })

  it('discards a batch beyond the 256 mutation limit', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'mutation-count-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: Array.from({ length: MAX_LUA_WORKER_MUTATIONS + 1 }, (_, index) => ({
        type: 'setChatVar',
        key: `key-${index}`,
        value: 'value',
      })),
      res: null,
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_mutation_limit',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('discards a mutation batch beyond 512 KiB', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'mutation-byte-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{
        type: 'setChat',
        index: 0,
        value: 'x'.repeat(MAX_LUA_WORKER_MUTATION_BYTES),
      }],
      res: null,
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_mutation_limit',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('rejects an unknown mutation type as a malformed result', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'unknown-mutation-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{ type: 'request', url: 'https://invalid.example' }] as never,
      res: null,
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_malformed_result',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('routes an invocation error without applying a mutation batch', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'handler-error-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'error',
      id: request.id,
      category: 'lua_worker_handler',
      message: 'synthetic handler failure',
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_handler',
      message: 'synthetic handler failure',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(false)
  })

  it('validates an ordered mutation batch against its evolving context', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'ordered-mutation-context-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [
        { type: 'insertChat', index: 1, role: 'char', value: 'inserted' },
        { type: 'setChat', index: 1, value: 'edited after insert' },
      ],
      res: 'complete',
      stopSending: false,
    })

    await expect(pending).resolves.toMatchObject({ res: 'complete' })
    expect(commitMutations).toHaveBeenCalledWith(4, [
      { type: 'insertChat', index: 1, role: 'char', value: 'inserted' },
      { type: 'setChat', index: 1, value: 'edited after insert' },
    ])
  })

  it.each(['lua_worker_timeout', 'lua_worker_memory'])(
    'terminates and replaces a Worker after fatal runtime error %s',
    async (category) => {
      const workers = [new FakeLuaWorker(), new FakeLuaWorker()]
      let workerIndex = 0
      const workerFactory = vi.fn(() => workers[workerIndex++])
      const client = new LuaWorkerHarnessClient({
        engineKey: `fatal-${category}`,
        source: 'fixture source',
        workerFactory,
      })
      const first = client.invoke(invocation(), { commitMutations: () => true })
      const firstOutcome = first.catch((error) => error)
      const firstRequest = workers[0].requests.find((message) => message.type === 'invoke')!

      workers[0].respond({
        type: 'error',
        id: firstRequest.id,
        category,
        message: 'fatal runtime state',
      })

      await expect(firstOutcome).resolves.toEqual(expect.objectContaining({ category }))
      expect(workers[0].terminated).toBe(true)

      const second = client.invoke({ ...invocation(), data: 'fresh VM' }, {
        commitMutations: () => true,
      })
      const secondRequest = workers[1].requests.find((message) => message.type === 'invoke')!
      workers[1].respond({
        type: 'result',
        id: secondRequest.id,
        metrics: {},
        orderedMutations: [],
        res: 'fresh VM',
        stopSending: false,
      })

      await expect(second).resolves.toMatchObject({ res: 'fresh VM' })
      expect(workerFactory).toHaveBeenCalledTimes(2)
    },
  )

  it('bounds invocation error messages and their complete envelope', async () => {
    const errors = [
      {
        category: 'lua_worker_handler',
        message: 'e'.repeat(MAX_LUA_WORKER_ERROR_MESSAGE_BYTES + 1),
      },
      {
        category: `lua_worker_${'c'.repeat(MAX_LUA_WORKER_ERROR_BYTES)}`,
        message: 'bounded',
      },
    ]

    for (const error of errors) {
      const worker = new FakeLuaWorker()
      const commitMutations = vi.fn(() => true)
      const client = new LuaWorkerHarnessClient({
        engineKey: `error-envelope-${errors.indexOf(error)}`,
        source: 'fixture source',
        workerFactory: () => worker,
      })
      const pending = client.invoke(invocation(), { commitMutations })
      const outcome = pending.catch((reason) => reason)
      const request = worker.requests.find((message) => message.type === 'invoke')!
      worker.respond({
        type: 'error',
        id: request.id,
        ...error,
      })

      await expect(outcome).resolves.toEqual(expect.objectContaining({
        category: 'lua_worker_output_limit',
      }))
      expect(commitMutations).not.toHaveBeenCalled()
      expect(worker.terminated).toBe(true)
    }
  })

  it('routes a bounded synthetic LLM host call and result to the active invocation', async () => {
    const worker = new FakeLuaWorker()
    const syntheticLLMMain = vi.fn(async () => ({ success: true, result: 'synthetic' }))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'synthetic-host-engine',
      source: 'fixture source',
      syntheticLLMMain,
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, {
      commitMutations: () => true,
    })
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 7,
      name: 'LLMMain',
      args: { prompt: [{ role: 'user', content: 'fixture' }] },
    })

    await vi.waitFor(() => {
      expect(worker.requests.filter((message) => message.type === 'hostResult')).toHaveLength(1)
    })
    expect(syntheticLLMMain).toHaveBeenCalledWith({
      prompt: [{ role: 'user', content: 'fixture' }],
    })
    expect(worker.requests.at(-1)).toEqual({
      type: 'hostResult',
      id: request.id,
      callId: 7,
      result: { success: true, result: 'synthetic' },
    })

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: { hostWaitMs: 1 },
      orderedMutations: [],
      res: 'complete',
      stopSending: false,
    })
    await expect(pending).resolves.toMatchObject({ res: 'complete' })
  })

  it('rejects LLMMain unless the active invocation granted low-level access', async () => {
    const worker = new FakeLuaWorker()
    const syntheticLLMMain = vi.fn(async () => 'must not run')
    const client = new LuaWorkerHarnessClient({
      engineKey: 'per-invocation-host-capability-engine',
      source: 'fixture source',
      syntheticLLMMain,
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations: () => true })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'LLMMain',
      args: null,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_capability',
    }))
    expect(syntheticLLMMain).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('applies the per-invocation LLMMain gate to editDisplay', async () => {
    const worker = new FakeLuaWorker()
    const syntheticLLMMain = vi.fn(async () => 'must not run')
    const client = new LuaWorkerHarnessClient({
      engineKey: 'edit-display-host-capability-engine',
      engineMode: 'editDisplay',
      source: 'fixture source',
      syntheticLLMMain,
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
      mode: 'editDisplay',
    }, {
      commitMutations: () => true,
    })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!
    expect(request).toMatchObject({ lowLevelAccess: false, mode: 'editDisplay' })
    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'LLMMain',
      args: null,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_capability',
    }))
    expect(syntheticLLMMain).not.toHaveBeenCalled()
  })

  it('bounds canonical host arguments and the full host-call envelope before host work', async () => {
    const argumentCases = [
      'a'.repeat(MAX_LUA_WORKER_HOST_ARGUMENT_BYTES),
      'a'.repeat(MAX_LUA_WORKER_HOST_CALL_ENVELOPE_BYTES - 2),
    ]
    for (const args of argumentCases) {
      const worker = new FakeLuaWorker()
      const syntheticLLMMain = vi.fn(async () => 'must not run')
      const client = new LuaWorkerHarnessClient({
        engineKey: `host-argument-envelope-${argumentCases.indexOf(args)}`,
        source: 'fixture source',
        syntheticLLMMain,
        workerFactory: () => worker,
      })
      const pending = client.invoke({ ...invocation(), lowLevelAccess: true }, {
        commitMutations: () => true,
      })
      const outcome = pending.catch((error) => error)
      const request = worker.requests.find((message) => message.type === 'invoke')!
      worker.respond({
        type: 'hostCall',
        id: request.id,
        callId: 1,
        name: 'LLMMain',
        args,
      })

      await expect(outcome).resolves.toEqual(expect.objectContaining({
        category: 'lua_worker_host_limit',
      }))
      expect(syntheticLLMMain).not.toHaveBeenCalled()
      expect(worker.terminated).toBe(true)
    }
  })

  it('rejects unsupported callback names explicitly and applies zero mutations', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'unsupported-callback-engine',
      source: 'fixture source',
      syntheticLLMMain: async () => null,
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'request',
      args: { url: 'https://invalid.example' },
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_unsupported_callback',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('terminates after more than 16 synthetic host calls', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const syntheticLLMMain = vi.fn(async () => 'synthetic')
    const client = new LuaWorkerHarnessClient({
      engineKey: 'host-call-limit-engine',
      source: 'fixture source',
      syntheticLLMMain,
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    for (let callId = 1; callId <= MAX_LUA_WORKER_HOST_CALLS + 1; callId++) {
      worker.respond({
        type: 'hostCall',
        id: request.id,
        callId,
        name: 'LLMMain',
        args: null,
      })
    }

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_host_limit',
    }))
    expect(syntheticLLMMain).toHaveBeenCalledTimes(MAX_LUA_WORKER_HOST_CALLS)
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('terminates when synthetic host responses exceed 1 MiB in total', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'host-response-limit-engine',
      source: 'fixture source',
      syntheticLLMMain: async () => 'x'.repeat(MAX_LUA_WORKER_HOST_RESPONSE_BYTES),
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'LLMMain',
      args: null,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_host_limit',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.requests.filter((message) => message.type === 'hostResult')).toHaveLength(0)
    expect(worker.terminated).toBe(true)
  })

  it('discards all mutations and queued work when the Worker crashes', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'crash-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const active = client.invoke(invocation(), { commitMutations })
    const queued = client.invoke({ ...invocation(), data: 'queued' }, { commitMutations })
    const activeOutcome = active.catch((error) => error)
    const queuedOutcome = queued.catch((error) => error)

    worker.crash()

    await expect(activeOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_crash',
    }))
    await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_crash',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
    expect(worker.listenerCount).toBe(0)
  })

  it('ignores a stale result ID and accepts only the active invocation result', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'stale-result-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id + 100,
      metrics: {},
      orderedMutations: [{ type: 'setChat', index: 0, value: 'stale' }],
      res: 'stale',
      stopSending: false,
    })
    expect(commitMutations).not.toHaveBeenCalled()

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'active',
      stopSending: false,
    })
    await expect(pending).resolves.toMatchObject({ res: 'active' })
    expect(commitMutations).toHaveBeenCalledTimes(1)
  })

  it('rejects chat mutations from editDisplay while allowing variable writes', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'edit-display-capability-engine',
      engineMode: 'editDisplay',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke({ ...invocation(), mode: 'editDisplay' }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [
        { type: 'setChatVar', key: 'allowed-variable', value: 'value' },
        { type: 'setChat', index: 0, value: 'forbidden-chat-write' },
      ],
      res: null,
      stopSending: false,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_capability',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('keeps later invocations queued until the active mutation CAS finishes', async () => {
    const worker = new FakeLuaWorker()
    let finishCommit!: (committed: boolean) => void
    const commitMutations = vi.fn(() => new Promise<boolean>((resolve) => {
      finishCommit = resolve
    }))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'cas-queue-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const first = client.invoke(invocation(), { commitMutations })
    const firstRequest = worker.requests.find((message) => message.type === 'invoke')!
    worker.respond({
      type: 'result',
      id: firstRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'first',
      stopSending: false,
    })
    await vi.waitFor(() => expect(commitMutations).toHaveBeenCalledTimes(1))

    const second = client.invoke({ ...invocation(), data: 'second' }, {
      commitMutations: () => true,
    })
    expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(1)

    finishCommit(true)
    await expect(first).resolves.toMatchObject({ res: 'first' })
    await vi.waitFor(() => {
      expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(2)
    })
    const secondRequest = worker.requests.at(-1)!
    if (secondRequest.type !== 'invoke') {
      throw new Error('Expected second invocation')
    }
    worker.respond({
      type: 'result',
      id: secondRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'second',
      stopSending: false,
    })
    await expect(second).resolves.toMatchObject({ res: 'second' })
  })

  it('claims a validated result once while mutation CAS is pending', async () => {
    const worker = new FakeLuaWorker()
    let finishCommit!: (committed: boolean) => void
    const commitMutations = vi.fn(() => new Promise<boolean>((resolve) => {
      finishCommit = resolve
    }))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'single-result-claim-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const request = worker.requests.find((message) => message.type === 'invoke')!
    const result: LuaWorkerHostMessage = {
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [{ type: 'setChatVar', key: 'fixture', value: 'value' }],
      res: 'complete',
      stopSending: false,
    }

    worker.respond(result)
    worker.respond(result)

    await vi.waitFor(() => expect(commitMutations).toHaveBeenCalledTimes(1))
    finishCommit(true)
    await expect(pending).resolves.toMatchObject({ res: 'complete' })
    expect(commitMutations).toHaveBeenCalledTimes(1)
  })

  it('ignores a late host completion after a result claimed settlement', async () => {
    const worker = new FakeLuaWorker()
    let finishHost!: (value: string) => void
    let finishCommit!: (committed: boolean) => void
    const client = new LuaWorkerHarnessClient({
      engineKey: 'claimed-result-host-engine',
      source: 'fixture source',
      syntheticLLMMain: () => new Promise((resolve) => {
        finishHost = resolve
      }),
      workerFactory: () => worker,
    })
    const pending = client.invoke({ ...invocation(), lowLevelAccess: true }, {
      commitMutations: () => new Promise((resolve) => {
        finishCommit = resolve
      }),
    })
    const request = worker.requests.find((message) => message.type === 'invoke')!
    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'LLMMain',
      args: null,
    })
    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'claimed',
      stopSending: false,
    })
    await vi.waitFor(() => expect(finishCommit).toBeTypeOf('function'))

    finishHost('late')
    await Promise.resolve()
    expect(worker.requests.filter((message) => message.type === 'hostResult')).toHaveLength(0)
    finishCommit(true)
    await expect(pending).resolves.toMatchObject({ res: 'claimed' })
  })

  it('lets a claimed result finish atomically when the Worker crashes during CAS', async () => {
    const worker = new FakeLuaWorker()
    let finishCommit!: (committed: boolean) => void
    const commitMutations = vi.fn(() => new Promise<boolean>((resolve) => {
      finishCommit = resolve
    }))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'commit-crash-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const active = client.invoke(invocation(), { commitMutations })
    const queued = client.invoke({ ...invocation(), data: 'queued' }, {
      commitMutations: () => true,
    })
    const queuedOutcome = queued.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'claimed',
      stopSending: false,
    })
    await vi.waitFor(() => expect(commitMutations).toHaveBeenCalledTimes(1))
    worker.crash('crash after validated result')
    finishCommit(true)

    await expect(active).resolves.toMatchObject({ res: 'claimed' })
    await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_crash',
    }))
    expect(commitMutations).toHaveBeenCalledTimes(1)
    expect(worker.terminated).toBe(true)
  })

  it('rejects queued work immediately when a Worker crashes during a never-settling CAS', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => new Promise<boolean>(() => undefined))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'never-settling-commit-crash-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const active = client.invoke(invocation(), { commitMutations })
    const queued = client.invoke({ ...invocation(), data: 'queued' }, {
      commitMutations: () => true,
    })
    const queuedOutcome = queued.catch((error) => error)
    let activeSettled = false
    void active.then(
      () => { activeSettled = true },
      () => { activeSettled = true },
    )
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'claimed',
      stopSending: false,
    })
    await vi.waitFor(() => expect(commitMutations).toHaveBeenCalledTimes(1))
    worker.crash('crash while CAS never settles')

    await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_crash',
    }))
    await expect(client.invoke({ ...invocation(), data: 'after-crash' }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_crash' }))
    expect(activeSettled).toBe(false)
    expect(worker.terminated).toBe(true)
  })

  it('rejects queued work immediately when disposed during a never-settling CAS', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => new Promise<boolean>(() => undefined))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'never-settling-commit-dispose-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const active = client.invoke(invocation(), { commitMutations })
    const queued = client.invoke({ ...invocation(), data: 'queued' }, {
      commitMutations: () => true,
    })
    const queuedOutcome = queued.catch((error) => error)
    let activeSettled = false
    void active.then(
      () => { activeSettled = true },
      () => { activeSettled = true },
    )
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'claimed',
      stopSending: false,
    })
    await vi.waitFor(() => expect(commitMutations).toHaveBeenCalledTimes(1))
    client.dispose()

    await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_disposed',
    }))
    expect(activeSettled).toBe(false)
    expect(worker.terminated).toBe(true)
  })

  it('ignores abort after a validated result owns settlement', async () => {
    const worker = new FakeLuaWorker()
    const controller = new AbortController()
    let finishCommit!: (committed: boolean) => void
    const commitMutations = vi.fn(() => new Promise<boolean>((resolve) => {
      finishCommit = resolve
    }))
    const client = new LuaWorkerHarnessClient({
      engineKey: 'commit-abort-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), {
      commitMutations,
      signal: controller.signal,
    })
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'claimed',
      stopSending: false,
    })
    await vi.waitFor(() => expect(commitMutations).toHaveBeenCalledTimes(1))
    controller.abort()
    finishCommit(true)

    await expect(pending).resolves.toMatchObject({ res: 'claimed' })
    expect(commitMutations).toHaveBeenCalledTimes(1)
    expect(worker.terminated).toBe(false)
  })

  it('deeply snapshots queued invocation data at admission', async () => {
    const worker = new FakeLuaWorker()
    const client = new LuaWorkerHarnessClient({
      engineKey: 'snapshot-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const first = client.invoke(invocation(), { commitMutations: () => true })
    const mutable = {
      ...invocation(),
      boundedContext: {
        messages: [{ role: 'user' as const, data: 'original message' }],
        startIndex: 0,
        totalMessages: 1,
      },
      data: { nested: ['original'] },
      meta: { nested: { value: 'original' } },
    }
    const second = client.invoke(mutable, { commitMutations: () => true })

    mutable.contextVersion = 99
    const mutableMode = mutable as unknown as { mode: TestEngineMode }
    mutableMode.mode = 'editOutput'
    mutable.data.nested[0] = 'x'.repeat(MAX_LUA_WORKER_CONTEXT_BYTES)
    mutable.meta.nested.value = 'mutated'
    mutable.boundedContext.messages[0].data = 'mutated message'

    const firstRequest = worker.requests.find((message) => message.type === 'invoke')!
    worker.respond({
      type: 'result',
      id: firstRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'first',
      stopSending: false,
    })
    await first
    await vi.waitFor(() => {
      expect(worker.requests.filter((message) => message.type === 'invoke')).toHaveLength(2)
    })
    const secondRequest = worker.requests.at(-1)!
    expect(secondRequest).toMatchObject({
      type: 'invoke',
      mode: 'editInput',
      contextVersion: 4,
      data: { nested: ['original'] },
      meta: { nested: { value: 'original' } },
      boundedContext: {
        messages: [{ role: 'user', data: 'original message' }],
        startIndex: 0,
        totalMessages: 1,
      },
    })
    if (secondRequest.type !== 'invoke') {
      throw new Error('Expected second invocation')
    }
    expect(Object.isFrozen(secondRequest.data)).toBe(true)
    expect(Object.isFrozen(secondRequest.boundedContext.messages[0])).toBe(true)
    worker.respond({
      type: 'result',
      id: secondRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'second',
      stopSending: false,
    })
    await expect(second).resolves.toMatchObject({ res: 'second' })
  })

  it('rejects malformed bounded context before creating a Worker', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'malformed-context-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      boundedContext: { messages: 'not-an-array' } as never,
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_malformed_input' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('cleans up a synchronous Worker registration failure as a crash', async () => {
    const worker = new RegisterFailureLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'register-failure-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })

    await expect(client.invoke(invocation(), { commitMutations })).rejects.toEqual(
      expect.objectContaining({ category: 'lua_worker_crash' }),
    )
    expect(worker.terminated).toBe(true)
    expect(worker.listenerCount).toBe(0)
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('cleans up a synchronous Worker invoke failure as a crash', async () => {
    const worker = new InvokeFailureLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'invoke-failure-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })

    await expect(client.invoke(invocation(), { commitMutations })).rejects.toEqual(
      expect.objectContaining({ category: 'lua_worker_crash' }),
    )
    expect(worker.terminated).toBe(true)
    expect(worker.listenerCount).toBe(0)
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('rejects malformed result metrics before mutation CAS', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'malformed-metrics-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'result',
      id: request.id,
      metrics: { handlerCpuMs: 'invalid' },
      orderedMutations: [],
      res: null,
      stopSending: false,
    } as never)

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_malformed_result',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('rejects a registration policy above the 64 MiB memory cap', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'memory-policy-engine',
      source: 'fixture source',
      policy: {
        ...DEFAULT_LUA_WORKER_POLICY,
        memoryBytes: DEFAULT_LUA_WORKER_POLICY.memoryBytes + 1,
      },
      workerFactory,
    })

    await expect(client.invoke(invocation(), {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_memory' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('does not allow an invocation timeout above the 2,000 ms CPU deadline', async () => {
    vi.useFakeTimers()
    const controller = new AbortController()
    try {
      const worker = new FakeLuaWorker()
      const client = new LuaWorkerHarnessClient({
        engineKey: 'timeout-cap-engine',
        source: 'fixture source',
        workerFactory: () => worker,
      })
      const pending = client.invoke(invocation(), {
        commitMutations: () => true,
        signal: controller.signal,
        timeoutMs: DEFAULT_LUA_WORKER_TIMEOUT_MS * 5,
      })
      const outcome = pending.catch((error) => error)

      await vi.advanceTimersByTimeAsync(DEFAULT_LUA_WORKER_TIMEOUT_MS)

      expect(worker.terminated).toBe(true)
      await expect(outcome).resolves.toEqual(expect.objectContaining({
        category: 'lua_worker_timeout',
      }))
    }
    finally {
      controller.abort()
      vi.useRealTimers()
    }
  })

  it('bounds synthetic host error responses before posting them to the Worker', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'host-error-limit-engine',
      source: 'fixture source',
      syntheticLLMMain: async () => {
        throw new Error('x'.repeat(MAX_LUA_WORKER_HOST_RESPONSE_BYTES))
      },
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'LLMMain',
      args: null,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_host_limit',
    }))
    expect(worker.requests.filter((message) => message.type === 'hostResult')).toHaveLength(0)
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('cleans up a synchronous host result post failure as a Worker crash', async () => {
    const worker = new HostResultFailureLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'host-result-failure-engine',
      source: 'fixture source',
      syntheticLLMMain: async () => 'synthetic',
      workerFactory: () => worker,
    })
    const pending = client.invoke({
      ...invocation(),
      lowLevelAccess: true,
    }, { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'hostCall',
      id: request.id,
      callId: 1,
      name: 'LLMMain',
      args: null,
    })

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_crash',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
    expect(worker.listenerCount).toBe(0)
  })

  it('rejects a malformed invocation error message and terminates the Worker', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'malformed-error-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)
    const request = worker.requests.find((message) => message.type === 'invoke')!

    worker.respond({
      type: 'error',
      id: request.id,
      category: 4,
      message: null,
    } as never)

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_malformed_result',
    }))
    expect(worker.terminated).toBe(true)
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('rejects a non-integer context version before creating a Worker', async () => {
    const workerFactory = vi.fn(() => new FakeLuaWorker())
    const client = new LuaWorkerHarnessClient({
      engineKey: 'context-version-engine',
      source: 'fixture source',
      workerFactory,
    })

    await expect(client.invoke({
      ...invocation(),
      contextVersion: Number.NaN,
    }, {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_malformed_input' }))
    expect(workerFactory).not.toHaveBeenCalled()
  })

  it('rejects a non-object Worker message as malformed without mutation CAS', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'malformed-message-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const pending = client.invoke(invocation(), { commitMutations })
    const outcome = pending.catch((error) => error)

    worker.respond(null as never)

    await expect(outcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_malformed_result',
    }))
    expect(worker.terminated).toBe(true)
    expect(commitMutations).not.toHaveBeenCalled()
  })

  it('disposes an idle Worker and rejects later invocations', async () => {
    const worker = new FakeLuaWorker()
    const workerFactory = vi.fn(() => worker)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'dispose-idle-engine',
      source: 'fixture source',
      workerFactory,
    })
    const first = client.invoke(invocation(), { commitMutations: () => true })
    const request = worker.requests.find((message) => message.type === 'invoke')!
    worker.respond({
      type: 'result',
      id: request.id,
      metrics: {},
      orderedMutations: [],
      res: 'complete',
      stopSending: false,
    })
    await first

    client.dispose()

    expect(worker.terminated).toBe(true)
    expect(worker.listenerCount).toBe(0)
    await expect(client.invoke(invocation(), {
      commitMutations: () => true,
    })).rejects.toEqual(expect.objectContaining({ category: 'lua_worker_disposed' }))
    expect(workerFactory).toHaveBeenCalledTimes(1)
  })

  it('rejects active and queued work when terminated before result claim', async () => {
    const worker = new FakeLuaWorker()
    const commitMutations = vi.fn(() => true)
    const client = new LuaWorkerHarnessClient({
      engineKey: 'terminate-active-engine',
      source: 'fixture source',
      workerFactory: () => worker,
    })
    const active = client.invoke(invocation(), { commitMutations })
    const queued = client.invoke({ ...invocation(), data: 'queued' }, { commitMutations })
    const activeOutcome = active.catch((error) => error)
    const queuedOutcome = queued.catch((error) => error)

    client.terminate()

    await expect(activeOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_disposed',
    }))
    await expect(queuedOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_disposed',
    }))
    expect(commitMutations).not.toHaveBeenCalled()
    expect(worker.terminated).toBe(true)
  })

  it('ignores captured messages from a replaced Worker identity', async () => {
    const firstWorker = new FakeLuaWorker()
    const secondWorker = new FakeLuaWorker()
    const workers = [firstWorker, secondWorker]
    const client = new LuaWorkerHarnessClient({
      engineKey: 'worker-identity-engine',
      source: 'fixture source',
      workerFactory: () => workers.shift()!,
    })
    const first = client.invoke(invocation(), { commitMutations: () => true })
    const firstOutcome = first.catch((error) => error)
    const staleDispatch = firstWorker.captureMessageDispatcher()
    firstWorker.crash('replace first Worker')
    await expect(firstOutcome).resolves.toEqual(expect.objectContaining({
      category: 'lua_worker_crash',
    }))

    const commitMutations = vi.fn(() => true)
    const second = client.invoke({ ...invocation(), data: 'second' }, { commitMutations })
    const secondRequest = secondWorker.requests.find((message) => message.type === 'invoke')!
    staleDispatch({
      type: 'result',
      id: secondRequest.id,
      metrics: {},
      orderedMutations: [{ type: 'setChatVar', key: 'stale', value: 'must not commit' }],
      res: 'stale',
      stopSending: false,
    })
    expect(commitMutations).not.toHaveBeenCalled()

    secondWorker.respond({
      type: 'result',
      id: secondRequest.id,
      metrics: {},
      orderedMutations: [],
      res: 'current',
      stopSending: false,
    })
    await expect(second).resolves.toMatchObject({ res: 'current' })
    expect(commitMutations).toHaveBeenCalledTimes(1)
  })
})
