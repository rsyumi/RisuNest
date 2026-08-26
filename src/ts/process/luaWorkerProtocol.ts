export type LuaWorkerJsonValue =
  | null
  | boolean
  | number
  | string
  | LuaWorkerJsonValue[]
  | { [key: string]: LuaWorkerJsonValue }

export type LuaWorkerMode = 'editRequest' | 'editInput' | 'editOutput' | 'editDisplay'

export interface LuaWorkerPolicy {
  memoryBytes: number
  cpuDeadlineMs: number
}

export interface LuaWorkerProtocolLimits {
  sourceBytes: number
  contextMessages: number
  invocationContextBytes: number
  resultValueBytes: number
  resultEnvelopeBytes: number
  errorMessageBytes: number
  errorEnvelopeBytes: number
  mutationCount: number
  mutationBytes: number
  metricCount: number
  metricKeyBytes: number
  metricsBytes: number
  hostCallCount: number
  hostArgumentBytes: number
  hostCallEnvelopeBytes: number
  hostResponseBytes: number
}

export type LuaWorkerMutation =
  | { type: 'setChatVar', key: string, value: string }
  | { type: 'setChatVarChanged', key: string, value: string }
  | { type: 'setChat', index: number, value: string }
  | { type: 'setChatRole', index: number, role: 'user' | 'char' }
  | { type: 'cutChat', start: number, end: number }
  | { type: 'removeChat', index: number }
  | { type: 'addChat', role: 'user' | 'char', value: string }
  | { type: 'insertChat', index: number, role: 'user' | 'char', value: string }
  | { type: 'stopChat' }

export interface LuaWorkerMetrics {
  [key: string]: number
}

export interface LuaWorkerEngineDescriptor {
  ownerChaId: string
  mode: LuaWorkerMode
  exactSourceHash: string
}

export interface LuaWorkerContextMessage {
  role: 'user' | 'char'
  data: string
  time?: number
}

export interface LuaWorkerBoundedContext {
  messages: LuaWorkerContextMessage[]
  startIndex: number
  totalMessages: number
  chatVars?: Record<string, string>
  globalVars?: Record<string, string>
}

export type LuaWorkerRequest = {
  type: 'register'
  engineKey: string
  source: string
  policy: LuaWorkerPolicy
  limits: LuaWorkerProtocolLimits
} | {
  type: 'invoke'
  id: number
  mode: LuaWorkerMode
  lowLevelAccess: boolean
  data: LuaWorkerJsonValue
  meta: LuaWorkerJsonValue
  contextVersion: number
  boundedContext: LuaWorkerBoundedContext
} | {
  type: 'hostResult'
  id: number
  callId: number
  result?: LuaWorkerJsonValue
  error?: { category: string, message: string }
}

export type LuaWorkerHostMessage = {
  type: 'registered'
} | {
  type: 'hostCall'
  id: number
  callId: number
  name: string
  args: LuaWorkerJsonValue
} | {
  type: 'result'
  id: number
  res: LuaWorkerJsonValue
  stopSending: boolean
  orderedMutations: LuaWorkerMutation[]
  metrics: LuaWorkerMetrics
} | {
  type: 'error'
  id: number
  category: string
  message: string
}

export interface LuaWorkerInvocation {
  runtime?: 'lua' | 'py'
  lowLevelAccess?: boolean
  mode: LuaWorkerMode
  data: LuaWorkerJsonValue
  meta: LuaWorkerJsonValue
  contextVersion: number
  boundedContext: LuaWorkerBoundedContext
}

export interface LuaWorkerInvocationResult {
  res: LuaWorkerJsonValue
  stopSending: boolean
  metrics: LuaWorkerMetrics
}

export function createLuaWorkerEngineKey(
  ownerChaId: string,
  mode: LuaWorkerMode,
  exactSourceHash: string,
): string {
  return JSON.stringify([ownerChaId, mode, exactSourceHash])
}

export class LuaWorkerContextWindowError extends Error {
  readonly category = 'lua_worker_context_window'

  constructor(message: string) {
    super(message)
    this.name = 'LuaWorkerContextWindowError'
  }
}

export function getLuaWorkerContextLength(context: LuaWorkerBoundedContext): number {
  return context.totalMessages
}

export function readLuaWorkerContextMessage(
  context: LuaWorkerBoundedContext,
  index: number,
): LuaWorkerContextMessage {
  const absoluteIndex = normalizeLuaWorkerContextIndex(context, index)
  const message = context.messages[absoluteIndex - context.startIndex]
  if (message === undefined) {
    throw new LuaWorkerContextWindowError(
      `Lua Worker chat index ${absoluteIndex} is outside the bounded context window`,
    )
  }
  return projectLuaWorkerContextMessage(message)
}

export function readLuaWorkerRecentMessages(
  context: LuaWorkerBoundedContext,
  count: number,
): LuaWorkerContextMessage[] {
  if (!Number.isSafeInteger(count) || count < 0) {
    throw new LuaWorkerContextWindowError('Lua Worker recent chat count must be a non-negative integer')
  }
  if (count === 0) {
    return []
  }
  const firstIndex = Math.max(0, context.totalMessages - count)
  const windowEnd = context.startIndex + context.messages.length
  if (context.startIndex > firstIndex || windowEnd < context.totalMessages) {
    throw new LuaWorkerContextWindowError(
      `Lua Worker recent ${count} chats are outside the bounded context window`,
    )
  }
  return context.messages
    .slice(firstIndex - context.startIndex)
    .map(projectLuaWorkerContextMessage)
}

function projectLuaWorkerContextMessage(message: LuaWorkerContextMessage): LuaWorkerContextMessage {
  return {
    role: message.role,
    data: message.data,
    time: message.time ?? 0,
  }
}

export function assertLuaWorkerMutationInContextWindow(
  context: LuaWorkerBoundedContext,
  mutation: LuaWorkerMutation,
): void {
  switch (mutation.type) {
    case 'setChat':
    case 'setChatRole':
    case 'removeChat':
      readLuaWorkerContextMessage(context, mutation.index)
      return
    case 'insertChat':
      assertLuaWorkerContextBoundary(context, mutation.index, 'insertChat')
      return
    case 'cutChat': {
      const start = assertLuaWorkerContextBoundary(context, mutation.start, 'cutChat start')
      const end = assertLuaWorkerContextBoundary(context, mutation.end, 'cutChat end')
      if (end < start) {
        throw new LuaWorkerContextWindowError('Lua Worker cutChat end precedes its start')
      }
      return
    }
    default:
      return
  }
}

export function applyLuaWorkerMutationToContext(
  context: LuaWorkerBoundedContext,
  mutation: LuaWorkerMutation,
): void {
  assertLuaWorkerMutationInContextWindow(context, mutation)
  switch (mutation.type) {
    case 'setChat': {
      const offset = normalizeLuaWorkerContextIndex(context, mutation.index) - context.startIndex
      context.messages[offset] = { ...context.messages[offset], data: mutation.value }
      return
    }
    case 'setChatRole': {
      const offset = normalizeLuaWorkerContextIndex(context, mutation.index) - context.startIndex
      context.messages[offset] = { ...context.messages[offset], role: mutation.role }
      return
    }
    case 'removeChat': {
      const offset = normalizeLuaWorkerContextIndex(context, mutation.index) - context.startIndex
      context.messages.splice(offset, 1)
      context.totalMessages--
      return
    }
    case 'insertChat': {
      const absoluteIndex = assertLuaWorkerContextBoundary(context, mutation.index, 'insertChat')
      context.messages.splice(absoluteIndex - context.startIndex, 0, {
        role: mutation.role,
        data: mutation.value,
      })
      context.totalMessages++
      return
    }
    case 'addChat': {
      if (context.startIndex + context.messages.length !== context.totalMessages) {
        throw new LuaWorkerContextWindowError(
          'Lua Worker cannot append outside the bounded context window',
        )
      }
      context.messages.push({ role: mutation.role, data: mutation.value })
      context.totalMessages++
      return
    }
    case 'cutChat': {
      const start = assertLuaWorkerContextBoundary(context, mutation.start, 'cutChat start')
      const end = assertLuaWorkerContextBoundary(context, mutation.end, 'cutChat end')
      context.messages = context.messages.slice(
        start - context.startIndex,
        end - context.startIndex,
      )
      context.startIndex = 0
      context.totalMessages = context.messages.length
      return
    }
    default:
      return
  }
}

function normalizeLuaWorkerContextIndex(
  context: LuaWorkerBoundedContext,
  index: number,
): number {
  if (!Number.isSafeInteger(index)) {
    throw new LuaWorkerContextWindowError('Lua Worker chat index must be a safe integer')
  }
  const absoluteIndex = index < 0 ? context.totalMessages + index : index
  if (absoluteIndex < 0 || absoluteIndex >= context.totalMessages) {
    throw new LuaWorkerContextWindowError(`Lua Worker chat index ${index} is out of range`)
  }
  return absoluteIndex
}

function assertLuaWorkerContextBoundary(
  context: LuaWorkerBoundedContext,
  index: number,
  operation: string,
): number {
  if (!Number.isSafeInteger(index)) {
    throw new LuaWorkerContextWindowError(`Lua Worker ${operation} index must be a safe integer`)
  }
  const absoluteIndex = index < 0 ? context.totalMessages + index : index
  const windowEnd = context.startIndex + context.messages.length
  if (absoluteIndex < context.startIndex || absoluteIndex > windowEnd) {
    throw new LuaWorkerContextWindowError(
      `Lua Worker ${operation} index ${absoluteIndex} is outside the bounded context window`,
    )
  }
  return absoluteIndex
}

export function canonicalizeLuaWorkerJson(value: unknown): string {
  const active = new Set<object>()

  const serialize = (current: unknown): string => {
    if (current === null || typeof current === 'boolean' || typeof current === 'string') {
      return JSON.stringify(current)
    }
    if (typeof current === 'number') {
      if (!Number.isFinite(current)) {
        throw new TypeError('Lua Worker JSON numbers must be finite')
      }
      return JSON.stringify(current)
    }
    if (typeof current !== 'object') {
      throw new TypeError(`Lua Worker JSON contains unsupported ${typeof current}`)
    }
    if (active.has(current)) {
      throw new TypeError('Lua Worker JSON contains a cycle')
    }

    active.add(current)
    try {
      if (Array.isArray(current)) {
        for (let index = 0; index < current.length; index++) {
          if (!Object.hasOwn(current, index)) {
            throw new TypeError('Lua Worker JSON contains a sparse array')
          }
        }
        return `[${current.map(serialize).join(',')}]`
      }

      const prototype = Object.getPrototypeOf(current)
      if (prototype !== Object.prototype && prototype !== null) {
        throw new TypeError('Lua Worker JSON contains a non-plain object')
      }
      const record = current as Record<string, unknown>
      return `{${Object.keys(record).sort().map((key) => (
        `${JSON.stringify(key)}:${serialize(record[key])}`
      )).join(',')}}`
    }
    finally {
      active.delete(current)
    }
  }

  return serialize(value)
}

export function isLuaWorkerMutation(value: unknown): value is LuaWorkerMutation {
  if (value === null || Array.isArray(value) || typeof value !== 'object') {
    return false
  }
  const mutation = value as Record<string, unknown>
  const integer = (field: string) => Number.isInteger(mutation[field])
  const string = (field: string) => typeof mutation[field] === 'string'
  const role = () => mutation.role === 'user' || mutation.role === 'char'

  switch (mutation.type) {
    case 'setChatVar':
    case 'setChatVarChanged':
      return string('key') && string('value')
    case 'setChat':
      return integer('index') && string('value')
    case 'setChatRole':
      return integer('index') && role()
    case 'cutChat':
      return integer('start') && integer('end')
    case 'removeChat':
      return integer('index')
    case 'addChat':
      return role() && string('value')
    case 'insertChat':
      return integer('index') && role() && string('value')
    case 'stopChat':
      return true
    default:
      return false
  }
}

export function isLuaWorkerMetrics(value: unknown): value is LuaWorkerMetrics {
  if (value === null || Array.isArray(value) || typeof value !== 'object') {
    return false
  }
  return Object.values(value).every((metric) => (
    typeof metric === 'number' && Number.isFinite(metric) && metric >= 0
  ))
}
