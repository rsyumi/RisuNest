import type {
  LuaWorkerHostMessage,
  LuaWorkerBoundedContext,
  LuaWorkerInvocation,
  LuaWorkerInvocationResult,
  LuaWorkerJsonValue,
  LuaWorkerMutation,
  LuaWorkerPolicy,
  LuaWorkerRequest,
} from './luaWorkerProtocol'
import {
  canonicalizeLuaWorkerJson,
  isLuaWorkerMetrics,
  isLuaWorkerMutation,
} from './luaWorkerProtocol'

export { createLuaWorkerEngineKey } from './luaWorkerProtocol'

export type {
  LuaWorkerHostMessage,
  LuaWorkerBoundedContext,
  LuaWorkerInvocation,
  LuaWorkerInvocationResult,
  LuaWorkerJsonValue,
  LuaWorkerMutation,
  LuaWorkerPolicy,
  LuaWorkerRequest,
} from './luaWorkerProtocol'

export const DEFAULT_LUA_WORKER_POLICY: LuaWorkerPolicy = {
  memoryBytes: 64 * 1024 * 1024,
  cpuDeadlineMs: 2_000,
}

export const DEFAULT_LUA_WORKER_TIMEOUT_MS = 2_000
export const MAX_LUA_WORKER_PENDING_INVOCATIONS = 8
export const MAX_LUA_WORKER_SOURCE_BYTES = 256 * 1024
export const MAX_LUA_WORKER_CONTEXT_MESSAGES = 256
export const MAX_LUA_WORKER_CONTEXT_BYTES = 1024 * 1024
export const MAX_LUA_WORKER_RESULT_BYTES = 2 * 1024 * 1024
export const MAX_LUA_WORKER_MUTATIONS = 256
export const MAX_LUA_WORKER_MUTATION_BYTES = 512 * 1024
export const MAX_LUA_WORKER_HOST_CALLS = 16
export const MAX_LUA_WORKER_HOST_RESPONSE_BYTES = 1024 * 1024
const LUA_WORKER_MODES = new Set(['editRequest', 'editInput', 'editOutput', 'editDisplay'])

export class LuaWorkerHarnessError extends Error {
  constructor(
    readonly category: string,
    message: string,
  ) {
    super(message)
    this.name = 'LuaWorkerHarnessError'
  }
}

export interface LuaWorkerLike {
  postMessage(message: LuaWorkerRequest): void
  terminate(): void
  addEventListener(type: 'message' | 'error', listener: EventListener): void
  removeEventListener(type: 'message' | 'error', listener: EventListener): void
}

export interface LuaWorkerHarnessOptions {
  engineKey: string
  source: string
  policy?: LuaWorkerPolicy
  syntheticLLMMain?: (args: LuaWorkerJsonValue) => LuaWorkerJsonValue | Promise<LuaWorkerJsonValue>
  workerFactory: () => LuaWorkerLike
}

export interface LuaWorkerInvokeOptions {
  commitMutations: (
    expectedContextVersion: number,
    orderedMutations: LuaWorkerMutation[],
  ) => boolean | Promise<boolean>
  signal?: AbortSignal
  timeoutMs?: number
}

interface ActiveInvocation {
  id: number
  contextVersion: number
  mode: LuaWorkerInvocation['mode']
  options: LuaWorkerInvokeOptions
  abortListener?: () => void
  hostCallIds: Set<number>
  hostResponseBytes: number
  timeout: ReturnType<typeof setTimeout>
  resolve: (result: LuaWorkerInvocationResult) => void
  reject: (error: unknown) => void
}

interface PendingInvocation {
  invocation: LuaWorkerInvocation
  options: LuaWorkerInvokeOptions
  resolve: (result: LuaWorkerInvocationResult) => void
  reject: (error: unknown) => void
  abortListener?: () => void
}

export class LuaWorkerHarnessClient {
  private worker: LuaWorkerLike | undefined
  private active: ActiveInvocation | undefined
  private readonly pending: PendingInvocation[] = []
  private nextInvocationId = 1
  private workerListeners: {
    worker: LuaWorkerLike
    message: EventListener
    error: EventListener
  } | undefined

  constructor(private readonly options: LuaWorkerHarnessOptions) {}

  invoke(
    invocation: LuaWorkerInvocation,
    options: LuaWorkerInvokeOptions,
  ): Promise<LuaWorkerInvocationResult> {
    if (options.signal?.aborted) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_abort',
        'Lua Worker invocation was aborted',
      ))
    }
    const policy = this.options.policy ?? DEFAULT_LUA_WORKER_POLICY
    if (!Number.isFinite(policy.memoryBytes) || policy.memoryBytes <= 0
      || policy.memoryBytes > DEFAULT_LUA_WORKER_POLICY.memoryBytes) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_memory',
        'Lua Worker memory policy must be within the 64 MiB cap',
      ))
    }
    if (!Number.isFinite(policy.cpuDeadlineMs) || policy.cpuDeadlineMs <= 0
      || policy.cpuDeadlineMs > DEFAULT_LUA_WORKER_POLICY.cpuDeadlineMs) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_timeout',
        'Lua Worker CPU deadline policy must be within 2,000 ms',
      ))
    }
    if (invocation.runtime !== undefined && invocation.runtime !== 'lua') {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_runtime',
        'Lua Worker harness accepts only Lua invocations',
      ))
    }
    if (!LUA_WORKER_MODES.has(invocation.mode)) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_mode',
        `Lua Worker mode is unsupported: ${invocation.mode}`,
      ))
    }
    if (invocation.lowLevelAccess === true && this.options.syntheticLLMMain === undefined) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_capability',
        'Lua Worker low-level access requires the synthetic LLM capability',
      ))
    }
    if (new TextEncoder().encode(this.options.source).byteLength > MAX_LUA_WORKER_SOURCE_BYTES) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_source_limit',
        'Lua Worker source exceeds 256 KiB',
      ))
    }
    if (!Number.isSafeInteger(invocation.contextVersion) || invocation.contextVersion < 0) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_malformed_input',
        'Lua Worker context version must be a non-negative safe integer',
      ))
    }
    const context = invocation.boundedContext
    if (context === null || Array.isArray(context) || typeof context !== 'object'
      || !Array.isArray(context.messages)) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_malformed_input',
        'Lua Worker bounded context must contain a messages array',
      ))
    }
    if (context.messages.length > MAX_LUA_WORKER_CONTEXT_MESSAGES) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_context_limit',
        'Lua Worker context exceeds 256 messages',
      ))
    }
    let canonicalContext: string
    try {
      canonicalContext = canonicalizeLuaWorkerJson({
        boundedContext: invocation.boundedContext,
        data: invocation.data,
        meta: invocation.meta,
      })
    }
    catch (error) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_malformed_input',
        error instanceof Error ? error.message : String(error),
      ))
    }
    if (new TextEncoder().encode(canonicalContext).byteLength > MAX_LUA_WORKER_CONTEXT_BYTES) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_context_limit',
        'Lua Worker canonical invocation context exceeds 1 MiB',
      ))
    }
    if (this.active !== undefined && this.pending.length >= MAX_LUA_WORKER_PENDING_INVOCATIONS) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_queue_limit',
        'Lua Worker pending invocation limit exceeded',
      ))
    }

    return new Promise((resolve, reject) => {
      const pending: PendingInvocation = { invocation, options, resolve, reject }
      if (this.active === undefined) {
        this.startInvocation(pending)
      }
      else {
        this.pending.push(pending)
        if (options.signal !== undefined) {
          pending.abortListener = () => {
            const index = this.pending.indexOf(pending)
            if (index === -1) {
              return
            }
            this.pending.splice(index, 1)
            this.cleanupPending(pending)
            reject(new LuaWorkerHarnessError(
              'lua_worker_abort',
              'Queued Lua Worker invocation was aborted',
            ))
          }
          options.signal.addEventListener('abort', pending.abortListener, { once: true })
        }
      }
    })
  }

  private startInvocation(pending: PendingInvocation): void {
    this.cleanupPending(pending)
    let worker: LuaWorkerLike
    try {
      worker = this.ensureWorker()
    }
    catch (error) {
      pending.reject(error)
      return
    }
    const id = this.nextInvocationId++
    const policy = this.options.policy ?? DEFAULT_LUA_WORKER_POLICY
    const timeoutMs = Math.min(
      pending.options.timeoutMs ?? policy.cpuDeadlineMs,
      policy.cpuDeadlineMs,
      DEFAULT_LUA_WORKER_TIMEOUT_MS,
    )
    const timeout = setTimeout(() => {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_timeout',
        `Lua Worker invocation ${id} timed out`,
      ))
    }, timeoutMs)
    const active: ActiveInvocation = {
      id,
      contextVersion: pending.invocation.contextVersion,
      hostCallIds: new Set(),
      hostResponseBytes: 0,
      mode: pending.invocation.mode,
      options: pending.options,
      timeout,
      resolve: pending.resolve,
      reject: pending.reject,
    }
    if (pending.options.signal !== undefined) {
      active.abortListener = () => {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_abort',
          'Lua Worker invocation was aborted',
        ))
      }
      pending.options.signal.addEventListener('abort', active.abortListener, { once: true })
    }
    this.active = active
    try {
      const { mode, data, meta, contextVersion, boundedContext } = pending.invocation
      worker.postMessage({
        type: 'invoke',
        id,
        mode,
        data,
        meta,
        contextVersion,
        boundedContext,
      })
    }
    catch (error) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_crash',
        error instanceof Error ? error.message : String(error),
      ))
    }
  }

  private ensureWorker(): LuaWorkerLike {
    if (this.worker !== undefined) {
      return this.worker
    }

    const worker = this.options.workerFactory()
    const messageListener = ((event: MessageEvent<LuaWorkerHostMessage>) => {
      this.handleMessage(event.data)
    }) as EventListener
    const errorListener = ((event: ErrorEvent) => {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_crash',
        event.message || 'Lua Worker crashed',
      ))
    }) as EventListener
    worker.addEventListener('message', messageListener)
    worker.addEventListener('error', errorListener)
    this.worker = worker
    this.workerListeners = { worker, message: messageListener, error: errorListener }
    try {
      worker.postMessage({
        type: 'register',
        engineKey: this.options.engineKey,
        source: this.options.source,
        policy: this.options.policy ?? DEFAULT_LUA_WORKER_POLICY,
      })
    }
    catch (error) {
      const crash = new LuaWorkerHarnessError(
        'lua_worker_crash',
        error instanceof Error ? error.message : String(error),
      )
      this.failWorker(crash)
      throw crash
    }
    return worker
  }

  private async handleMessage(message: LuaWorkerHostMessage): Promise<void> {
    const active = this.active
    if (active === undefined) {
      return
    }
    if (message === null || Array.isArray(message) || typeof message !== 'object') {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker posted a non-object message',
      ))
      return
    }
    if (message.id !== active.id) {
      return
    }
    if (message.type === 'error') {
      if (typeof message.category !== 'string' || !message.category.startsWith('lua_worker_')
        || typeof message.message !== 'string') {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_malformed_result',
          'Lua Worker returned a malformed invocation error',
        ))
        return
      }
      this.active = undefined
      this.cleanupActive(active)
      active.reject(new LuaWorkerHarnessError(message.category, message.message))
      this.startNextInvocation()
      return
    }
    if (message.type === 'hostCall') {
      await this.handleHostCall(message, active)
      return
    }
    if (message.type !== 'result') {
      return
    }
    if (typeof message.stopSending !== 'boolean') {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker result has an invalid stopSending value',
      ))
      return
    }
    if (!isLuaWorkerMetrics(message.metrics)) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker result has invalid metrics',
      ))
      return
    }
    let canonicalResult: string
    try {
      canonicalResult = canonicalizeLuaWorkerJson(message.res)
    }
    catch (error) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        error instanceof Error ? error.message : String(error),
      ))
      return
    }
    if (new TextEncoder().encode(canonicalResult).byteLength > MAX_LUA_WORKER_RESULT_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_output_limit',
        'Lua Worker result exceeds 2 MiB',
      ))
      return
    }
    if (!Array.isArray(message.orderedMutations)) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker result has an invalid mutation batch',
      ))
      return
    }
    if (message.orderedMutations.length > MAX_LUA_WORKER_MUTATIONS) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_mutation_limit',
        'Lua Worker result exceeds 256 mutations',
      ))
      return
    }
    if (!message.orderedMutations.every(isLuaWorkerMutation)) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker result contains an invalid mutation',
      ))
      return
    }
    if (active.mode === 'editDisplay' && message.orderedMutations.some((mutation) => (
      mutation.type !== 'setChatVar' && mutation.type !== 'setChatVarChanged'
    ))) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_capability',
        'Lua Worker editDisplay mode permits only variable mutations',
      ))
      return
    }
    let canonicalMutations: string
    try {
      canonicalMutations = canonicalizeLuaWorkerJson(message.orderedMutations)
    }
    catch (error) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        error instanceof Error ? error.message : String(error),
      ))
      return
    }
    if (new TextEncoder().encode(canonicalMutations).byteLength > MAX_LUA_WORKER_MUTATION_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_mutation_limit',
        'Lua Worker mutation batch exceeds 512 KiB',
      ))
      return
    }

    this.cleanupActive(active)
    try {
      const committed = await active.options.commitMutations(
        active.contextVersion,
        message.orderedMutations,
      )
      if (!committed) {
        throw new LuaWorkerHarnessError(
          'lua_worker_stale_context',
          `Lua Worker context version ${active.contextVersion} is stale`,
        )
      }
      active.resolve({
        metrics: message.metrics,
        res: message.res,
        stopSending: message.stopSending,
      })
    }
    catch (error) {
      active.reject(error)
    }
    finally {
      if (this.active === active) {
        this.active = undefined
        this.startNextInvocation()
      }
    }
  }

  private startNextInvocation(): void {
    const next = this.pending.shift()
    if (next !== undefined) {
      this.startInvocation(next)
    }
  }

  private async handleHostCall(
    message: Extract<LuaWorkerHostMessage, { type: 'hostCall' }>,
    active: ActiveInvocation,
  ): Promise<void> {
    if (message.name !== 'LLMMain' || this.options.syntheticLLMMain === undefined) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_unsupported_callback',
        `Lua Worker callback is unsupported: ${message.name}`,
      ))
      return
    }
    if (!Number.isInteger(message.callId) || message.callId < 0 || active.hostCallIds.has(message.callId)) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker host call has an invalid or duplicate call ID',
      ))
      return
    }
    active.hostCallIds.add(message.callId)
    if (active.hostCallIds.size > MAX_LUA_WORKER_HOST_CALLS) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_host_limit',
        'Lua Worker invocation exceeds 16 synthetic host calls',
      ))
      return
    }

    try {
      const result = await this.options.syntheticLLMMain(message.args)
      if (this.active !== active || this.worker === undefined) {
        return
      }
      let responseBytes: number
      try {
        responseBytes = new TextEncoder().encode(canonicalizeLuaWorkerJson(result)).byteLength
      }
      catch (error) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_host_limit',
          error instanceof Error ? error.message : String(error),
        ))
        return
      }
      active.hostResponseBytes += responseBytes
      if (active.hostResponseBytes > MAX_LUA_WORKER_HOST_RESPONSE_BYTES) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_host_limit',
          'Lua Worker synthetic host responses exceed 1 MiB',
        ))
        return
      }
      this.postHostResult({
        type: 'hostResult',
        id: message.id,
        callId: message.callId,
        result,
      })
    }
    catch (error) {
      if (this.active !== active || this.worker === undefined) {
        return
      }
      const hostError = {
        category: 'lua_worker_host_error',
        message: error instanceof Error ? error.message : String(error),
      }
      const responseBytes = new TextEncoder().encode(
        canonicalizeLuaWorkerJson(hostError),
      ).byteLength
      active.hostResponseBytes += responseBytes
      if (active.hostResponseBytes > MAX_LUA_WORKER_HOST_RESPONSE_BYTES) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_host_limit',
          'Lua Worker synthetic host responses exceed 1 MiB',
        ))
        return
      }
      this.postHostResult({
        type: 'hostResult',
        id: message.id,
        callId: message.callId,
        error: hostError,
      })
    }
  }

  private postHostResult(message: Extract<LuaWorkerRequest, { type: 'hostResult' }>): void {
    try {
      this.worker?.postMessage(message)
    }
    catch (error) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_crash',
        error instanceof Error ? error.message : String(error),
      ))
    }
  }

  private failWorker(error: unknown): void {
    const listeners = this.workerListeners
    this.worker = undefined
    this.workerListeners = undefined
    if (listeners !== undefined) {
      listeners.worker.removeEventListener('message', listeners.message)
      listeners.worker.removeEventListener('error', listeners.error)
      listeners.worker.terminate()
    }

    const active = this.active
    this.active = undefined
    if (active !== undefined) {
      this.cleanupActive(active)
      active.reject(error)
    }
    for (const pending of this.pending.splice(0)) {
      this.cleanupPending(pending)
      pending.reject(error)
    }
  }

  private cleanupActive(active: ActiveInvocation): void {
    clearTimeout(active.timeout)
    if (active.options.signal !== undefined && active.abortListener !== undefined) {
      active.options.signal.removeEventListener('abort', active.abortListener)
    }
  }

  private cleanupPending(pending: PendingInvocation): void {
    if (pending.options.signal !== undefined && pending.abortListener !== undefined) {
      pending.options.signal.removeEventListener('abort', pending.abortListener)
      pending.abortListener = undefined
    }
  }
}
