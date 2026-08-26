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

export interface LuaWorkerBoundedContext {
  messages: Array<{
    role: 'user' | 'char'
    data: string
    time?: number
  }>
  chatVars?: Record<string, string>
  globalVars?: Record<string, string>
}

export type LuaWorkerRequest = {
  type: 'register'
  engineKey: string
  source: string
  policy: LuaWorkerPolicy
} | {
  type: 'invoke'
  id: number
  mode: LuaWorkerMode
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
