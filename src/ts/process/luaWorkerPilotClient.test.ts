import { expect, it, vi } from 'vitest'
import type {
  LuaWorkerHostMessage,
  LuaWorkerRequest,
} from './luaWorkerProtocol'
import { createLuaWorkerPilotClient } from './luaWorkerPilotClient'

class CapturedModuleWorker {
  static instances: CapturedModuleWorker[] = []

  readonly requests: LuaWorkerRequest[] = []
  readonly listeners = new Map<string, Set<EventListener>>()
  terminated = false

  constructor(
    readonly url: URL,
    readonly options: WorkerOptions,
  ) {
    CapturedModuleWorker.instances.push(this)
  }

  postMessage(message: LuaWorkerRequest): void {
    this.requests.push(message)
  }

  terminate(): void {
    this.terminated = true
  }

  addEventListener(type: string, listener: EventListener): void {
    const listeners = this.listeners.get(type) ?? new Set<EventListener>()
    listeners.add(listener)
    this.listeners.set(type, listeners)
  }

  removeEventListener(type: string, listener: EventListener): void {
    this.listeners.get(type)?.delete(listener)
  }

  respond(message: LuaWorkerHostMessage): void {
    for (const listener of this.listeners.get('message') ?? []) {
      listener({ data: message } as MessageEvent)
    }
  }
}

it('creates one Vite module Worker for one immutable engine descriptor', async () => {
  CapturedModuleWorker.instances = []
  vi.stubGlobal('Worker', CapturedModuleWorker)
  const client = createLuaWorkerPilotClient({
    engine: {
      ownerChaId: 'module-worker-owner',
      mode: 'editInput',
      exactSourceHash: 'module-worker-source',
    },
    source: `listenEdit('editInput', function(id, value) return value end)`,
  })

  const result = client.invoke({
    mode: 'editInput',
    data: 'fixture',
    meta: {},
    contextVersion: 1,
    boundedContext: {
      messages: [],
      startIndex: 0,
      totalMessages: 0,
    },
  }, {
    commitMutations: async () => true,
  })
  const worker = CapturedModuleWorker.instances[0]

  expect(worker.options).toEqual({ type: 'module' })
  expect(worker.url.pathname).toMatch(/luaWorker\.ts$/)
  expect(worker.requests.map((request) => request.type)).toEqual(['register', 'invoke'])
  worker.respond({
    type: 'result',
    id: 1,
    res: 'fixture',
    stopSending: false,
    orderedMutations: [],
    metrics: { wallMs: 1, luaMemoryBytes: 1024 },
  })

  await expect(result).resolves.toMatchObject({ res: 'fixture' })
  client.dispose()
  expect(worker.terminated).toBe(true)
  vi.unstubAllGlobals()
})
