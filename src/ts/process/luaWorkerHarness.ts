import type {
  LuaWorkerHostMessage,
  LuaWorkerBoundedContext,
  LuaWorkerEngineDescriptor,
  LuaWorkerInvocation,
  LuaWorkerInvocationResult,
  LuaWorkerJsonValue,
  LuaWorkerMutation,
  LuaWorkerPolicy,
  LuaWorkerProtocolLimits,
  LuaWorkerRequest,
} from './luaWorkerProtocol'
import {
  applyLuaWorkerMutationToContext,
  canonicalizeLuaWorkerJson,
  createLuaWorkerEngineKey,
  isLuaWorkerMetrics,
  isLuaWorkerMutation,
  LuaWorkerContextWindowError,
} from './luaWorkerProtocol'

export {
  createLuaWorkerEngineKey,
  getLuaWorkerContextLength,
  readLuaWorkerContextMessage,
  readLuaWorkerRecentMessages,
} from './luaWorkerProtocol'

export type {
  LuaWorkerHostMessage,
  LuaWorkerBoundedContext,
  LuaWorkerEngineDescriptor,
  LuaWorkerInvocation,
  LuaWorkerInvocationResult,
  LuaWorkerJsonValue,
  LuaWorkerMutation,
  LuaWorkerPolicy,
  LuaWorkerProtocolLimits,
  LuaWorkerRequest,
} from './luaWorkerProtocol'

export const DEFAULT_LUA_WORKER_POLICY: LuaWorkerPolicy = {
  memoryBytes: 64 * 1024 * 1024,
  cpuDeadlineMs: 2_000,
}

export const DEFAULT_LUA_WORKER_TIMEOUT_MS = 2_000
export const DEFAULT_LUA_WORKER_REGISTRATION_TIMEOUT_MS = 10_000
export const MAX_LUA_WORKER_PENDING_INVOCATIONS = 8
export const MAX_LUA_WORKER_SOURCE_BYTES = 256 * 1024
export const MAX_LUA_WORKER_CONTEXT_MESSAGES = 256
export const MAX_LUA_WORKER_CONTEXT_BYTES = 1024 * 1024
export const MAX_LUA_WORKER_RESULT_BYTES = 2 * 1024 * 1024
export const MAX_LUA_WORKER_RESULT_ENVELOPE_BYTES = 2 * 1024 * 1024
export const MAX_LUA_WORKER_ERROR_MESSAGE_BYTES = 8 * 1024
export const MAX_LUA_WORKER_ERROR_BYTES = 16 * 1024
export const MAX_LUA_WORKER_MUTATIONS = 256
export const MAX_LUA_WORKER_MUTATION_BYTES = 512 * 1024
export const MAX_LUA_WORKER_METRICS = 64
export const MAX_LUA_WORKER_METRIC_KEY_BYTES = 256
export const MAX_LUA_WORKER_METRICS_BYTES = 4 * 1024
export const MAX_LUA_WORKER_HOST_CALLS = 16
export const MAX_LUA_WORKER_HOST_ARGUMENT_BYTES = 1024 * 1024
export const MAX_LUA_WORKER_HOST_CALL_ENVELOPE_BYTES = 1024 * 1024
export const MAX_LUA_WORKER_HOST_RESPONSE_BYTES = 1024 * 1024
export const LUA_WORKER_PROTOCOL_LIMITS = Object.freeze({
  sourceBytes: MAX_LUA_WORKER_SOURCE_BYTES,
  contextMessages: MAX_LUA_WORKER_CONTEXT_MESSAGES,
  invocationContextBytes: MAX_LUA_WORKER_CONTEXT_BYTES,
  resultValueBytes: MAX_LUA_WORKER_RESULT_BYTES,
  resultEnvelopeBytes: MAX_LUA_WORKER_RESULT_ENVELOPE_BYTES,
  errorMessageBytes: MAX_LUA_WORKER_ERROR_MESSAGE_BYTES,
  errorEnvelopeBytes: MAX_LUA_WORKER_ERROR_BYTES,
  mutationCount: MAX_LUA_WORKER_MUTATIONS,
  mutationBytes: MAX_LUA_WORKER_MUTATION_BYTES,
  metricCount: MAX_LUA_WORKER_METRICS,
  metricKeyBytes: MAX_LUA_WORKER_METRIC_KEY_BYTES,
  metricsBytes: MAX_LUA_WORKER_METRICS_BYTES,
  hostCallCount: MAX_LUA_WORKER_HOST_CALLS,
  hostArgumentBytes: MAX_LUA_WORKER_HOST_ARGUMENT_BYTES,
  hostCallEnvelopeBytes: MAX_LUA_WORKER_HOST_CALL_ENVELOPE_BYTES,
  hostResponseBytes: MAX_LUA_WORKER_HOST_RESPONSE_BYTES,
}) satisfies LuaWorkerProtocolLimits
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
  engine: LuaWorkerEngineDescriptor
  source: string
  policy?: LuaWorkerPolicy
  syntheticLLMMain?: (args: LuaWorkerJsonValue) => LuaWorkerJsonValue | Promise<LuaWorkerJsonValue>
  workerFactory: () => LuaWorkerLike
  waitForReady?: boolean
  registrationTimeoutMs?: number
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
  boundedContext: LuaWorkerBoundedContext
  lowLevelAccess: boolean
  phase: 'running' | 'committing'
  postCommitFailure?: unknown
  options: LuaWorkerInvokeOptions
  abortListener?: () => void
  hostCallIds: Set<number>
  hostResponseBytes: number
  timeout?: ReturnType<typeof setTimeout>
  timeoutMs: number
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
  private readonly options: LuaWorkerHarnessOptions
  private worker: LuaWorkerLike | undefined
  private active: ActiveInvocation | undefined
  private readonly pending: PendingInvocation[] = []
  private nextInvocationId = 1
  private disposed = false
  private workerReady = false
  private registrationTimeout: ReturnType<typeof setTimeout> | undefined
  private workerListeners: {
    worker: LuaWorkerLike
    message: EventListener
    error: EventListener
  } | undefined

  constructor(options: LuaWorkerHarnessOptions) {
    this.options = {
      ...options,
      engine: Object.freeze({ ...options.engine }),
      policy: options.policy === undefined ? undefined : Object.freeze({ ...options.policy }),
    }
  }

  dispose(): void {
    if (this.disposed) {
      return
    }
    this.disposed = true
    this.failWorker(new LuaWorkerHarnessError(
      'lua_worker_disposed',
      'Lua Worker harness client was disposed',
    ))
  }

  terminate(): void {
    this.dispose()
  }

  invoke(
    invocation: LuaWorkerInvocation,
    options: LuaWorkerInvokeOptions,
  ): Promise<LuaWorkerInvocationResult> {
    if (this.disposed) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_disposed',
        'Lua Worker harness client is disposed',
      ))
    }
    if (this.active?.postCommitFailure !== undefined) {
      return Promise.reject(this.active.postCommitFailure)
    }
    if (options.signal?.aborted) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_abort',
        'Lua Worker invocation was aborted',
      ))
    }
    try {
      invocation = deepFreeze(structuredClone(invocation))
    }
    catch (error) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_malformed_input',
        error instanceof Error ? error.message : String(error),
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
    if (invocation.mode !== this.options.engine.mode) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_mode',
        `Lua Worker invocation mode ${invocation.mode} does not match engine mode ${this.options.engine.mode}`,
      ))
    }
    if (hasLuaWorkerLlmAccess(invocation) && this.options.syntheticLLMMain === undefined) {
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
    if (!Number.isSafeInteger(context.startIndex) || context.startIndex < 0
      || !Number.isSafeInteger(context.totalMessages) || context.totalMessages < 0
      || context.startIndex > context.totalMessages
      || context.startIndex + context.messages.length > context.totalMessages) {
      return Promise.reject(new LuaWorkerHarnessError(
        'lua_worker_malformed_input',
        'Lua Worker bounded context has invalid absolute window metadata',
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
      const pending: PendingInvocation = {
        invocation,
        options,
        resolve,
        reject,
      }
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
    const active: ActiveInvocation = {
      id,
      boundedContext: pending.invocation.boundedContext,
      contextVersion: pending.invocation.contextVersion,
      hostCallIds: new Set(),
      hostResponseBytes: 0,
      mode: pending.invocation.mode,
      lowLevelAccess: hasLuaWorkerLlmAccess(pending.invocation),
      phase: 'running',
      options: pending.options,
      timeoutMs,
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
    if (this.options.waitForReady !== true || this.workerReady) {
      this.armActiveTimeout(active)
    }
    try {
      const { mode, data, meta, contextVersion, boundedContext } = pending.invocation
      worker.postMessage({
        type: 'invoke',
        id,
        mode,
        lowLevelAccess: hasLuaWorkerLlmAccess(pending.invocation),
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
      if (this.worker === worker) {
        void this.handleMessage(event.data)
      }
    }) as EventListener
    const errorListener = ((event: ErrorEvent) => {
      if (this.worker === worker) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_crash',
          event.message || 'Lua Worker crashed',
        ))
      }
    }) as EventListener
    worker.addEventListener('message', messageListener)
    worker.addEventListener('error', errorListener)
    this.worker = worker
    this.workerListeners = { worker, message: messageListener, error: errorListener }
    this.workerReady = this.options.waitForReady !== true
    if (!this.workerReady) {
      this.registrationTimeout = setTimeout(() => {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_registration',
          'Lua Worker registration timed out',
        ))
      }, this.options.registrationTimeoutMs ?? DEFAULT_LUA_WORKER_REGISTRATION_TIMEOUT_MS)
    }
    try {
      worker.postMessage({
        type: 'register',
        engineKey: createLuaWorkerEngineKey(
          this.options.engine.ownerChaId,
          this.options.engine.mode,
          this.options.engine.exactSourceHash,
        ),
        limits: LUA_WORKER_PROTOCOL_LIMITS,
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
    if (message === null || Array.isArray(message) || typeof message !== 'object') {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker posted a non-object message',
      ))
      return
    }
    if (message.type === 'registered') {
      if (this.options.waitForReady !== true) {
        return
      }
      if (this.workerReady) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_malformed_result',
          'Lua Worker registered more than once',
        ))
        return
      }
      this.workerReady = true
      this.clearRegistrationTimeout()
      if (this.active?.phase === 'running') {
        this.armActiveTimeout(this.active)
      }
      return
    }
    const active = this.active
    if (active === undefined) {
      return
    }
    if (this.options.waitForReady === true && !this.workerReady && message.type !== 'error') {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker posted an invocation message before registration completed',
      ))
      return
    }
    if (message.id !== active.id) {
      return
    }
    if (active.phase === 'committing') {
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
      let errorBytes: number
      try {
        errorBytes = canonicalByteLength(message)
      }
      catch (error) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_malformed_result',
          error instanceof Error ? error.message : String(error),
        ))
        return
      }
      if (errorBytes > MAX_LUA_WORKER_ERROR_BYTES) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_output_limit',
          'Lua Worker error envelope exceeds 16 KiB',
        ))
        return
      }
      if (utf8ByteLength(message.message) > MAX_LUA_WORKER_ERROR_MESSAGE_BYTES) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_output_limit',
          'Lua Worker error message exceeds 8 KiB',
        ))
        return
      }
      const runtimeError = new LuaWorkerHarnessError(message.category, message.message)
      if (!this.workerReady || message.category === 'lua_worker_timeout'
        || message.category === 'lua_worker_memory') {
        this.failWorker(runtimeError)
        return
      }
      this.active = undefined
      this.cleanupActive(active)
      active.reject(runtimeError)
      this.startNextInvocation()
      return
    }
    if (message.type === 'hostCall') {
      await this.handleHostCall(message, active)
      return
    }
    if (message.type !== 'result') {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        'Lua Worker posted an unknown message type',
      ))
      return
    }
    let resultEnvelopeBytes: number
    try {
      resultEnvelopeBytes = canonicalByteLength(message)
    }
    catch (error) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        error instanceof Error ? error.message : String(error),
      ))
      return
    }
    if (resultEnvelopeBytes > MAX_LUA_WORKER_RESULT_ENVELOPE_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_output_limit',
        'Lua Worker result envelope exceeds 2 MiB',
      ))
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
    const metricKeys = Object.keys(message.metrics)
    if (metricKeys.length > MAX_LUA_WORKER_METRICS
      || metricKeys.some((key) => utf8ByteLength(key) > MAX_LUA_WORKER_METRIC_KEY_BYTES)
      || canonicalByteLength(message.metrics) > MAX_LUA_WORKER_METRICS_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_output_limit',
        'Lua Worker result metrics exceed their bounded envelope',
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
    if (utf8ByteLength(canonicalResult) > MAX_LUA_WORKER_RESULT_BYTES) {
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
    try {
      const mutationContext = cloneBoundedContext(active.boundedContext)
      for (const mutation of message.orderedMutations) {
        applyLuaWorkerMutationToContext(mutationContext, mutation)
      }
    }
    catch (error) {
      if (error instanceof LuaWorkerContextWindowError) {
        this.failWorker(new LuaWorkerHarnessError(error.category, error.message))
        return
      }
      throw error
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
    if (utf8ByteLength(canonicalMutations) > MAX_LUA_WORKER_MUTATION_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_mutation_limit',
        'Lua Worker mutation batch exceeds 512 KiB',
      ))
      return
    }

    active.phase = 'committing'
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
        if (active.postCommitFailure !== undefined) {
          this.rejectPending(active.postCommitFailure)
        }
        else {
          this.startNextInvocation()
        }
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
    let argumentBytes: number
    let envelopeBytes: number
    try {
      argumentBytes = canonicalByteLength(message.args)
      envelopeBytes = canonicalByteLength(message)
    }
    catch (error) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_malformed_result',
        error instanceof Error ? error.message : String(error),
      ))
      return
    }
    if (argumentBytes > MAX_LUA_WORKER_HOST_ARGUMENT_BYTES
      || envelopeBytes > MAX_LUA_WORKER_HOST_CALL_ENVELOPE_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_host_limit',
        'Lua Worker host-call envelope exceeds 1 MiB',
      ))
      return
    }
    if (!active.lowLevelAccess) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_capability',
        'Lua Worker invocation did not grant low-level host access',
      ))
      return
    }
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
      if (this.active !== active || this.worker === undefined || active.phase !== 'running') {
        return
      }
      const hostResult = {
        type: 'hostResult' as const,
        id: message.id,
        callId: message.callId,
        result,
      }
      try {
        if (!this.reserveHostResponse(active, hostResult)) {
          return
        }
      }
      catch (error) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_host_limit',
          error instanceof Error ? error.message : String(error),
        ))
        return
      }
      this.postHostResult(hostResult)
    }
    catch (error) {
      if (this.active !== active || this.worker === undefined || active.phase !== 'running') {
        return
      }
      const hostError = {
        category: 'lua_worker_host_error',
        message: error instanceof Error ? error.message : String(error),
      }
      if (utf8ByteLength(hostError.message) > MAX_LUA_WORKER_ERROR_MESSAGE_BYTES) {
        this.failWorker(new LuaWorkerHarnessError(
          'lua_worker_host_limit',
          'Lua Worker synthetic host error exceeds 8 KiB',
        ))
        return
      }
      const hostResult = {
        type: 'hostResult',
        id: message.id,
        callId: message.callId,
        error: hostError,
      } as const
      if (!this.reserveHostResponse(active, hostResult)) {
        return
      }
      this.postHostResult(hostResult)
    }
  }

  private reserveHostResponse(
    active: ActiveInvocation,
    message: Extract<LuaWorkerRequest, { type: 'hostResult' }>,
  ): boolean {
    const responseBytes = canonicalByteLength(message)
    active.hostResponseBytes += responseBytes
    if (active.hostResponseBytes > MAX_LUA_WORKER_HOST_RESPONSE_BYTES) {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_host_limit',
        'Lua Worker synthetic host response envelopes exceed 1 MiB',
      ))
      return false
    }
    return true
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
    this.workerReady = false
    this.clearRegistrationTimeout()
    if (listeners !== undefined) {
      listeners.worker.removeEventListener('message', listeners.message)
      listeners.worker.removeEventListener('error', listeners.error)
      listeners.worker.terminate()
    }

    const active = this.active
    if (active?.phase === 'committing') {
      active.postCommitFailure ??= error
      this.rejectPending(error)
      return
    }
    this.active = undefined
    if (active !== undefined) {
      this.cleanupActive(active)
      active.reject(error)
    }
    this.rejectPending(error)
  }

  private rejectPending(error: unknown): void {
    for (const pending of this.pending.splice(0)) {
      this.cleanupPending(pending)
      pending.reject(error)
    }
  }

  private cleanupActive(active: ActiveInvocation): void {
    if (active.timeout !== undefined) {
      clearTimeout(active.timeout)
    }
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

  private armActiveTimeout(active: ActiveInvocation): void {
    if (active.timeout !== undefined || active.phase !== 'running') {
      return
    }
    active.timeout = setTimeout(() => {
      this.failWorker(new LuaWorkerHarnessError(
        'lua_worker_timeout',
        `Lua Worker invocation ${active.id} timed out`,
      ))
    }, active.timeoutMs)
  }

  private clearRegistrationTimeout(): void {
    if (this.registrationTimeout !== undefined) {
      clearTimeout(this.registrationTimeout)
      this.registrationTimeout = undefined
    }
  }
}

function deepFreeze<T>(value: T): T {
  if (value !== null && typeof value === 'object' && !Object.isFrozen(value)) {
    Object.freeze(value)
    for (const nested of Object.values(value)) {
      deepFreeze(nested)
    }
  }
  return value
}

function canonicalByteLength(value: unknown): number {
  return utf8ByteLength(canonicalizeLuaWorkerJson(value))
}

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength
}

function hasLuaWorkerLlmAccess(invocation: LuaWorkerInvocation): boolean {
  return invocation.lowLevelAccess === true && invocation.mode !== 'editDisplay'
}

function cloneBoundedContext(context: LuaWorkerBoundedContext): LuaWorkerBoundedContext {
  return {
    messages: context.messages.map((message) => ({ ...message })),
    startIndex: context.startIndex,
    totalMessages: context.totalMessages,
    chatVars: context.chatVars === undefined ? undefined : { ...context.chatVars },
    globalVars: context.globalVars === undefined ? undefined : { ...context.globalVars },
  }
}
