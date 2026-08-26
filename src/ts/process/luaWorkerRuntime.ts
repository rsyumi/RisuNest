import { LuaFactory } from 'wasmoon'
import type { LuaEngine } from 'wasmoon'
import type {
  LuaWorkerBoundedContext,
  LuaWorkerHostMessage,
  LuaWorkerJsonValue,
  LuaWorkerMutation,
  LuaWorkerProtocolLimits,
  LuaWorkerRequest,
} from './luaWorkerProtocol'
import {
  applyLuaWorkerMutationToContext,
  canonicalizeLuaWorkerJson,
  getLuaWorkerContextLength,
  readLuaWorkerContextMessage,
  readLuaWorkerRecentMessages,
} from './luaWorkerProtocol'

export interface WasmoonLuaWorkerRuntimeOptions {
  loadJsonLua: () => Promise<string>
  postMessage: (message: LuaWorkerHostMessage) => void
  now?: () => number
}

export interface WasmoonLuaWorkerRuntime {
  handleMessage(message: LuaWorkerRequest): void
  dispose(): void
}

interface RegisteredEngine {
  engine: LuaEngine
  mode: string
  limits: LuaWorkerProtocolLimits
}

interface QueuedInvocation {
  request: Extract<LuaWorkerRequest, { type: 'invoke' }>
}

interface ActiveExecution {
  accessId: string
  hostCallCount: number
  hostResponseBytes: number
  limits: LuaWorkerProtocolLimits
  mutations: LuaWorkerMutation[]
  request: Extract<LuaWorkerRequest, { type: 'invoke' }>
  stopSending: boolean
  variables: Record<string, string>
  workingContext: LuaWorkerBoundedContext
}

interface PendingHostCall {
  invocationId: number
  reject: (error: unknown) => void
  resolve: (value: string) => void
}

const RUNTIME_ERROR_MARKER = '__RISUNEST_LUA_WORKER__'
const UNSUPPORTED_CALLBACKS = [
  'alertError',
  'alertNormal',
  'alertInput',
  'alertSelect',
  'alertConfirm',
  'getTokens',
  'getFullChatMain',
  'setFullChatMain',
  'sleep',
  'cbs',
  'reloadDisplay',
  'reloadChat',
  'similarity',
  'request',
  'generateImage',
  'getCharacterImageMain',
  'getPersonaImageMain',
  'hash',
  'simpleLLM',
  'getName',
  'setName',
  'getDescription',
  'setDescription',
  'getCharacterFirstMessage',
  'setCharacterFirstMessage',
  'getPersonaName',
  'getPersonaDescription',
  'getAuthorsNote',
  'getBackgroundEmbedding',
  'setBackgroundEmbedding',
  'getLoreBooksMain',
  'upsertLocalLoreBook',
  'loadLoreBooksMain',
  'axLLMMain',
  'getCharacterLastMessage',
  'getUserLastMessage',
] as const

const LUA_WORKER_WRAPPER = `
json = require 'json'

function getChat(id, index)
    return json.decode(getChatMain(id, index))
end

function getRecentChats(id, count)
    return json.decode(getRecentChatsMain(id, count))
end

function getFullChat(id)
    return json.decode(getFullChatMain(id))
end

function setFullChat(id, value)
    setFullChatMain(id, json.encode(value))
end

function log(value)
    logMain(json.encode(value))
end

function getLoreBooks(id, search)
    return json.decode(getLoreBooksMain(id, search))
end

function loadLoreBooks(id)
    return json.decode(loadLoreBooksMain(id):await())
end

function LLM(id, prompt, useMultimodal, options)
    useMultimodal = useMultimodal or false
    options = options or {}
    return json.decode(LLMMain(
        id,
        json.encode(prompt),
        useMultimodal,
        json.encode(options)
    ):await())
end

function axLLM(id, prompt, useMultimodal, options)
    useMultimodal = useMultimodal or false
    options = options or {}
    return json.decode(axLLMMain(
        id,
        json.encode(prompt),
        useMultimodal,
        json.encode(options)
    ):await())
end

function getCharacterImage(id)
    return getCharacterImageMain(id):await()
end

function getPersonaImage(id)
    return getPersonaImageMain(id):await()
end

function getState(id, name)
    return json.decode(getChatVar(id, '__'..name))
end

function setState(id, name, value)
    setChatVar(id, '__'..name, json.encode(value))
end

function setStateChanged(id, name, value)
    return setChatVarChanged(id, '__'..name, json.encode(value))
end

local editRequestFuncs = {}
local editDisplayFuncs = {}
local editInputFuncs = {}
local editOutputFuncs = {}

function listenEdit(type, func)
    if type == 'editRequest' then
        editRequestFuncs[#editRequestFuncs + 1] = func
        return
    end
    if type == 'editDisplay' then
        editDisplayFuncs[#editDisplayFuncs + 1] = func
        return
    end
    if type == 'editInput' then
        editInputFuncs[#editInputFuncs + 1] = func
        return
    end
    if type == 'editOutput' then
        editOutputFuncs[#editOutputFuncs + 1] = func
        return
    end
    error('Invalid edit listener type')
end

function async(callback)
    return function(...)
        local co = coroutine.create(callback)
        local safe, result = coroutine.resume(co, ...)

        return Promise.create(function(resolve, reject)
            local checkresult
            local step = function()
                if coroutine.status(co) == 'dead' then
                    local send = safe and resolve or reject
                    return send(result)
                end

                safe, result = coroutine.resume(co)
                checkresult()
            end

            checkresult = function()
                if safe and result == Promise.resolve(result) then
                    result:finally(step)
                else
                    step()
                end
            end

            checkresult()
        end)
    end
end

callListenMain = async(function(type, id, value, meta)
    local realValue = json.decode(value)
    local realMeta = json.decode(meta)
    local funcs
    if type == 'editRequest' then funcs = editRequestFuncs end
    if type == 'editDisplay' then funcs = editDisplayFuncs end
    if type == 'editInput' then funcs = editInputFuncs end
    if type == 'editOutput' then funcs = editOutputFuncs end
    if funcs == nil then error('Invalid edit listener type') end

    for _, func in ipairs(funcs) do
        realValue = func(id, realValue, realMeta)
    end

    return json.encode(realValue)
end)
`

export function createWasmoonLuaWorkerRuntime(
  options: WasmoonLuaWorkerRuntimeOptions,
): WasmoonLuaWorkerRuntime {
  let registration: Promise<RegisteredEngine> | undefined
  let disposed = false
  let active = false
  let execution: ActiveExecution | undefined
  let nextHostCallId = 1
  const queue: QueuedInvocation[] = []
  const pendingHostCalls = new Map<number, PendingHostCall>()
  const now = options.now ?? (() => performance.now())

  const requireExecution = (accessId: unknown): ActiveExecution => {
    if (execution === undefined || accessId !== execution.accessId) {
      throw new WorkerRuntimeError('lua_worker_capability', 'Lua Worker access ID is invalid')
    }
    return execution
  }

  const recordMutation = (accessId: unknown, mutation: LuaWorkerMutation) => {
    const current = requireExecution(accessId)
    if (current.request.mode === 'editDisplay'
      && mutation.type !== 'setChatVar' && mutation.type !== 'setChatVarChanged') {
      throw new WorkerRuntimeError(
        'lua_worker_capability',
        'Lua Worker editDisplay mode permits only variable mutations',
      )
    }
    if (current.mutations.length >= current.limits.mutationCount) {
      throw new WorkerRuntimeError('lua_worker_mutation_limit', 'Lua Worker mutation count exceeds its limit')
    }
    const candidate = [...current.mutations, mutation]
    if (canonicalByteLength(candidate) > current.limits.mutationBytes) {
      throw new WorkerRuntimeError('lua_worker_mutation_limit', 'Lua Worker mutation bytes exceed their limit')
    }
    try {
      applyLuaWorkerMutationToContext(current.workingContext, mutation)
    }
    catch (error) {
      throw new WorkerRuntimeError(
        'lua_worker_context_window',
        error instanceof Error ? error.message : String(error),
      )
    }
    current.mutations.push(mutation)
  }

  const readMessage = (accessId: unknown, index: number) => {
    const current = requireExecution(accessId)
    try {
      return readLuaWorkerContextMessage(current.workingContext, index)
    }
    catch (error) {
      throw new WorkerRuntimeError(
        'lua_worker_context_window',
        error instanceof Error ? error.message : String(error),
      )
    }
  }

  const initialize = async (
    request: Extract<LuaWorkerRequest, { type: 'register' }>,
  ): Promise<RegisteredEngine> => {
    const descriptor = JSON.parse(request.engineKey) as unknown
    if (!Array.isArray(descriptor) || descriptor.length !== 3
      || typeof descriptor[1] !== 'string') {
      throw new Error('Lua Worker engine key is malformed')
    }
    if (utf8ByteLength(request.source) > request.limits.sourceBytes) {
      throw new WorkerRuntimeError('lua_worker_source_limit', 'Lua Worker source exceeds its limit')
    }

    const factory = new LuaFactory()
    await factory.mountFile('json.lua', await options.loadJsonLua())
    const engine = await factory.createEngine({
      injectObjects: true,
      traceAllocations: true,
      functionTimeout: request.policy.cpuDeadlineMs,
    })
    engine.global.setMemoryMax(request.policy.memoryBytes)
    engine.global.set('getChatVar', (accessId: unknown, key: string) => {
      const current = requireExecution(accessId)
      return current.variables[key] ?? 'null'
    })
    engine.global.set('getGlobalVar', (accessId: unknown, key: string) => {
      const current = requireExecution(accessId)
      return current.request.boundedContext.globalVars?.[key] ?? 'null'
    })
    engine.global.set('setChatVar', (accessId: unknown, key: string, value: string) => {
      const current = requireExecution(accessId)
      recordMutation(accessId, { type: 'setChatVar', key, value })
      current.variables[key] = value
    })
    engine.global.set('setChatVarChanged', (accessId: unknown, key: string, value: string) => {
      const current = requireExecution(accessId)
      if (current.variables[key] === value) {
        return undefined
      }
      recordMutation(accessId, { type: 'setChatVarChanged', key, value })
      current.variables[key] = value
      return true
    })
    engine.global.set('getChatMain', (accessId: unknown, index: number) => (
      JSON.stringify(readMessage(accessId, index))
    ))
    engine.global.set('getChatData', (accessId: unknown, index: number) => (
      readMessage(accessId, index).data
    ))
    engine.global.set('getChatRole', (accessId: unknown, index: number) => (
      readMessage(accessId, index).role
    ))
    engine.global.set('getRecentChatsMain', (accessId: unknown, count: number) => {
      const current = requireExecution(accessId)
      try {
        return JSON.stringify(readLuaWorkerRecentMessages(current.workingContext, count))
      }
      catch (error) {
        throw new WorkerRuntimeError(
          'lua_worker_context_window',
          error instanceof Error ? error.message : String(error),
        )
      }
    })
    engine.global.set('getChatLength', (accessId: unknown) => {
      const current = requireExecution(accessId)
      return getLuaWorkerContextLength(current.workingContext)
    })
    engine.global.set('setChat', (accessId: unknown, index: number, value: string) => {
      recordMutation(accessId, { type: 'setChat', index, value: value ?? '' })
    })
    engine.global.set('setChatRole', (accessId: unknown, index: number, role: string) => {
      recordMutation(accessId, {
        type: 'setChatRole',
        index,
        role: role === 'user' ? 'user' : 'char',
      })
    })
    engine.global.set('cutChat', (accessId: unknown, start: number, end: number) => {
      recordMutation(accessId, { type: 'cutChat', start, end })
    })
    engine.global.set('removeChat', (accessId: unknown, index: number) => {
      recordMutation(accessId, { type: 'removeChat', index })
    })
    engine.global.set('addChat', (accessId: unknown, role: string, value: string) => {
      recordMutation(accessId, {
        type: 'addChat',
        role: role === 'user' ? 'user' : 'char',
        value: value ?? '',
      })
    })
    engine.global.set(
      'insertChat',
      (accessId: unknown, index: number, role: string, value: string) => {
        recordMutation(accessId, {
          type: 'insertChat',
          index,
          role: role === 'user' ? 'user' : 'char',
          value: value ?? '',
        })
      },
    )
    engine.global.set('stopChat', (accessId: unknown) => {
      const current = requireExecution(accessId)
      recordMutation(accessId, { type: 'stopChat' })
      current.stopSending = true
    })
    engine.global.set('logMain', (value: string) => {
      console.log(JSON.parse(value))
    })
    engine.global.set(
      'LLMMain',
      (
        accessId: unknown,
        promptJson: string,
        useMultimodal = false,
        optionsJson = '{}',
      ) => {
        const current = requireExecution(accessId)
        if (!current.request.lowLevelAccess || current.request.mode === 'editDisplay') {
          throw new WorkerRuntimeError(
            'lua_worker_capability',
            'Lua Worker invocation did not grant synthetic LLM access',
          )
        }
        if (current.hostCallCount >= current.limits.hostCallCount) {
          throw new WorkerRuntimeError('lua_worker_host_limit', 'Lua Worker host-call count exceeds its limit')
        }
        const args = {
          prompt: JSON.parse(promptJson) as LuaWorkerJsonValue,
          useMultimodal: useMultimodal === true,
          options: JSON.parse(optionsJson) as LuaWorkerJsonValue,
        }
        const callId = nextHostCallId++
        const hostCall = {
          type: 'hostCall' as const,
          id: current.request.id,
          callId,
          name: 'LLMMain',
          args,
        }
        if (canonicalByteLength(args) > current.limits.hostArgumentBytes
          || canonicalByteLength(hostCall) > current.limits.hostCallEnvelopeBytes) {
          throw new WorkerRuntimeError('lua_worker_host_limit', 'Lua Worker host-call bytes exceed their limit')
        }
        current.hostCallCount++
        return new Promise<string>((resolve, reject) => {
          pendingHostCalls.set(callId, {
            invocationId: current.request.id,
            reject,
            resolve,
          })
          options.postMessage(hostCall)
        })
      },
    )
    for (const callback of UNSUPPORTED_CALLBACKS) {
      engine.global.set(callback, () => {
        throw new WorkerRuntimeError(
          'lua_worker_unsupported_callback',
          `Lua Worker callback is unsupported: ${callback}`,
        )
      })
    }
    await engine.doString(`${LUA_WORKER_WRAPPER}\n${request.source}`)
    options.postMessage({ type: 'registered' })
    return {
      engine,
      mode: descriptor[1],
      limits: request.limits,
    }
  }

  const postError = (id: number, error: unknown) => {
    const classified = classifyRuntimeError(error)
    let message = classified.publicMessage
    let category = classified.category
    let response = {
      type: 'error',
      id,
      category,
      message,
    } as const
    void registration?.then(({ limits }) => {
      if (utf8ByteLength(message) > limits.errorMessageBytes
        || canonicalByteLength(response) > limits.errorEnvelopeBytes) {
        category = 'lua_worker_output_limit'
        message = 'Lua Worker error exceeds its bounded envelope'
        response = { type: 'error', id, category, message }
      }
      options.postMessage(response)
    }, () => {
      options.postMessage(response)
    })
  }

  const drain = async () => {
    if (active || disposed) {
      return
    }
    const next = queue.shift()
    if (next === undefined) {
      return
    }
    active = true
    const startedAt = now()
    try {
      if (registration === undefined) {
        throw new WorkerRuntimeError('lua_worker_registration', 'Lua Worker is not registered')
      }
      const registered = await registration
      const { request } = next
      if (request.mode !== registered.mode) {
        throw new WorkerRuntimeError(
          'lua_worker_mode',
          `Lua Worker invocation mode ${request.mode} does not match ${registered.mode}`,
        )
      }
      if (request.boundedContext.messages.length > registered.limits.contextMessages) {
        throw new WorkerRuntimeError(
          'lua_worker_context_limit',
          'Lua Worker bounded context message count exceeds its limit',
        )
      }
      let invocationBytes: number
      try {
        invocationBytes = canonicalByteLength({
          boundedContext: request.boundedContext,
          data: request.data,
          meta: request.meta,
        })
      }
      catch (error) {
        throw new WorkerRuntimeError(
          'lua_worker_malformed_input',
          error instanceof Error ? error.message : String(error),
        )
      }
      if (invocationBytes > registered.limits.invocationContextBytes) {
        throw new WorkerRuntimeError(
          'lua_worker_context_limit',
          'Lua Worker canonical invocation context exceeds its limit',
        )
      }
      const func = registered.engine.global.get('callListenMain')
      if (typeof func !== 'function') {
        throw new WorkerRuntimeError('lua_worker_source', 'Lua Worker listener dispatcher is unavailable')
      }
      const currentExecution: ActiveExecution = {
        accessId: `${request.id}`,
        hostCallCount: 0,
        hostResponseBytes: 0,
        limits: registered.limits,
        mutations: [],
        request,
        stopSending: false,
        variables: { ...request.boundedContext.chatVars },
        workingContext: cloneBoundedContext(request.boundedContext),
      }
      execution = currentExecution
      const rawResult = await func(
        request.mode,
        currentExecution.accessId,
        JSON.stringify(request.data),
        JSON.stringify(request.meta),
      )
      const res = JSON.parse(rawResult) as LuaWorkerJsonValue
      if (canonicalByteLength(res) > registered.limits.resultValueBytes) {
        throw new WorkerRuntimeError('lua_worker_output_limit', 'Lua Worker result exceeds its limit')
      }
      const metrics = {
        wallMs: Math.max(0, now() - startedAt),
        luaMemoryBytes: registered.engine.global.getMemoryUsed(),
      }
      if (Object.keys(metrics).length > registered.limits.metricCount
        || Object.keys(metrics).some((key) => utf8ByteLength(key) > registered.limits.metricKeyBytes)
        || canonicalByteLength(metrics) > registered.limits.metricsBytes) {
        throw new WorkerRuntimeError('lua_worker_output_limit', 'Lua Worker metrics exceed their limit')
      }
      const response = {
        type: 'result',
        id: request.id,
        res,
        stopSending: res === false || currentExecution.stopSending,
        orderedMutations: currentExecution.mutations,
        metrics,
      } as const
      if (canonicalByteLength(response) > registered.limits.resultEnvelopeBytes) {
        throw new WorkerRuntimeError(
          'lua_worker_output_limit',
          'Lua Worker result envelope exceeds its limit',
        )
      }
      options.postMessage(response)
    }
    catch (error) {
      postError(next.request.id, error)
    }
    finally {
      for (const [callId, pending] of pendingHostCalls) {
        if (pending.invocationId === next.request.id) {
          pendingHostCalls.delete(callId)
          pending.reject(new WorkerRuntimeError(
            'lua_worker_host_error',
            'Lua Worker invocation ended before its host call completed',
          ))
        }
      }
      execution = undefined
      active = false
      void drain()
    }
  }

  return {
    handleMessage(message) {
      if (disposed) {
        return
      }
      if (message.type === 'register') {
        if (registration !== undefined) {
          registration = Promise.reject(new WorkerRuntimeError(
            'lua_worker_registration',
            'Lua Worker engine descriptor is immutable',
          ))
          registration.catch(() => undefined)
          return
        }
        registration = initialize(message)
        registration.catch(() => undefined)
        return
      }
      if (message.type === 'invoke') {
        queue.push({ request: message })
        void drain()
        return
      }
      if (message.type === 'hostResult') {
        const pending = pendingHostCalls.get(message.callId)
        if (pending === undefined || pending.invocationId !== message.id || execution === undefined) {
          return
        }
        let responseBytes: number
        try {
          responseBytes = canonicalByteLength(message)
        }
        catch (error) {
          pendingHostCalls.delete(message.callId)
          pending.reject(new WorkerRuntimeError(
            'lua_worker_host_limit',
            error instanceof Error ? error.message : String(error),
          ))
          return
        }
        execution.hostResponseBytes += responseBytes
        if (execution.hostResponseBytes > execution.limits.hostResponseBytes) {
          pendingHostCalls.delete(message.callId)
          pending.reject(new WorkerRuntimeError(
            'lua_worker_host_limit',
            'Lua Worker host-result bytes exceed their limit',
          ))
          return
        }
        pendingHostCalls.delete(message.callId)
        if (message.error !== undefined) {
          pending.reject(new WorkerRuntimeError(message.error.category, message.error.message))
          return
        }
        try {
          pending.resolve(JSON.stringify(message.result ?? null))
        }
        catch (error) {
          pending.reject(error)
        }
      }
    },
    dispose() {
      disposed = true
      queue.length = 0
      for (const pending of pendingHostCalls.values()) {
        pending.reject(new WorkerRuntimeError('lua_worker_disposed', 'Lua Worker runtime was disposed'))
      }
      pendingHostCalls.clear()
      void registration?.then(({ engine }) => engine.global.close(), () => undefined)
    },
  }
}

class WorkerRuntimeError extends Error {
  readonly publicMessage: string

  constructor(
    readonly category: string,
    message: string,
  ) {
    super(`${RUNTIME_ERROR_MARKER}${category}:${message}`)
    this.name = 'WorkerRuntimeError'
    this.publicMessage = message
  }
}

function classifyRuntimeError(error: unknown): WorkerRuntimeError {
  if (error instanceof WorkerRuntimeError) {
    return error
  }
  const message = error instanceof Error ? error.message : String(error)
  const markerIndex = message.indexOf(RUNTIME_ERROR_MARKER)
  if (markerIndex >= 0) {
    const encoded = message.slice(markerIndex + RUNTIME_ERROR_MARKER.length)
    const separator = encoded.indexOf(':')
    if (separator > 0) {
      return new WorkerRuntimeError(
        encoded.slice(0, separator),
        encoded.slice(separator + 1).split('\n')[0],
      )
    }
  }
  if (/memory|allocation/i.test(message)) {
    return new WorkerRuntimeError('lua_worker_memory', message)
  }
  if (/timeout/i.test(message)) {
    return new WorkerRuntimeError('lua_worker_timeout', message)
  }
  return new WorkerRuntimeError('lua_worker_handler', message)
}

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength
}

function canonicalByteLength(value: unknown): number {
  return utf8ByteLength(canonicalizeLuaWorkerJson(value))
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
