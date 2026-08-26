import { describe, expect, it, vi } from 'vitest'
import {
  createLuaWorkerEngineKey,
  DEFAULT_LUA_WORKER_TIMEOUT_MS,
  DEFAULT_LUA_WORKER_POLICY,
  MAX_LUA_WORKER_CONTEXT_BYTES,
  MAX_LUA_WORKER_CONTEXT_MESSAGES,
  MAX_LUA_WORKER_RESULT_BYTES,
  MAX_LUA_WORKER_MUTATIONS,
  MAX_LUA_WORKER_MUTATION_BYTES,
  MAX_LUA_WORKER_HOST_CALLS,
  MAX_LUA_WORKER_HOST_RESPONSE_BYTES,
  MAX_LUA_WORKER_SOURCE_BYTES,
  LuaWorkerHarnessError,
  LuaWorkerHarnessClient,
  type LuaWorkerHostMessage,
  type LuaWorkerLike,
  type LuaWorkerRequest,
} from './luaWorkerHarness'

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
    boundedContext: { messages: [] },
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
})
