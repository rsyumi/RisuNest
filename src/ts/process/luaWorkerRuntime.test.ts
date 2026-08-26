// @vitest-environment node

import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { beforeAll, describe, expect, it, vi } from 'vitest'
import type {
  LuaWorkerHostMessage,
  LuaWorkerRequest,
} from './luaWorkerProtocol'
import {
  DEFAULT_LUA_WORKER_POLICY,
  LUA_WORKER_PROTOCOL_LIMITS,
} from './luaWorkerHarness'
import { createWasmoonLuaWorkerRuntime } from './luaWorkerRuntime'

let jsonLua = ''

beforeAll(async () => {
  jsonLua = await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8')
})

function register(
  source: string,
  overrides: Partial<Extract<LuaWorkerRequest, { type: 'register' }>> = {},
): Extract<LuaWorkerRequest, { type: 'register' }> {
  return {
    type: 'register',
    engineKey: JSON.stringify(['runtime-owner', 'editInput', 'runtime-source']),
    source,
    policy: DEFAULT_LUA_WORKER_POLICY,
    limits: LUA_WORKER_PROTOCOL_LIMITS,
    ...overrides,
  }
}

function invoke(
  id: number,
  overrides: Partial<Extract<LuaWorkerRequest, { type: 'invoke' }>> = {},
): Extract<LuaWorkerRequest, { type: 'invoke' }> {
  return {
    type: 'invoke',
    id,
    mode: 'editInput',
    lowLevelAccess: false,
    data: null,
    meta: {},
    contextVersion: 1,
    boundedContext: {
      messages: [],
      startIndex: 0,
      totalMessages: 0,
    },
    ...overrides,
  }
}

async function waitForMessage(
  messages: LuaWorkerHostMessage[],
  predicate: (message: LuaWorkerHostMessage) => boolean,
): Promise<LuaWorkerHostMessage> {
  await vi.waitFor(() => {
    expect(messages.some(predicate)).toBe(true)
  })
  return messages.find(predicate)!
}

describe('real Wasmoon Lua Worker runtime', () => {
  it('loads json.lua and preserves globals for FIFO invocations', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      counter = 0
      listenEdit('editInput', function(id, value, meta)
        counter = counter + 1
        return { count = counter, value = value }
      end)
    `))

    runtime.handleMessage(invoke(1, {
      data: { dense: [1, false, 'value'], nullValue: null },
    }))
    runtime.handleMessage(invoke(2, { data: false }))

    await waitForMessage(messages, (message) => message.type === 'result' && message.id === 2)
    expect(messages.filter((message) => message.type === 'result')).toMatchObject([
      {
        type: 'result',
        id: 1,
        res: { count: 1, value: { dense: [1, false, 'value'] } },
        stopSending: false,
        orderedMutations: [],
      },
      {
        type: 'result',
        id: 2,
        res: { count: 2, value: false },
        stopSending: false,
        orderedMutations: [],
      },
    ])
  })

  it('reads the bounded chat window and records ordered mutations', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        local first = getChat(id, 0)
        local last = getChat(id, -1)
        setChat(id, 0, 'edited')
        insertChat(id, 1, 'char', 'inserted')
        setChatRole(id, 0, 'char')
        removeChat(id, 2)
        addChat(id, 'user', 'tail')
        cutChat(id, 1, 3)
        return {
          first = first,
          last = last,
          recent = getRecentChats(id, 2),
          length = getChatLength(id)
        }
      end)
    `))
    runtime.handleMessage(invoke(3, {
      boundedContext: {
        messages: [
          { role: 'user', data: 'zero', time: 10 },
          { role: 'char', data: 'one' },
          { role: 'user', data: 'two', time: 30 },
        ],
        startIndex: 0,
        totalMessages: 3,
      },
    }))

    const result = await waitForMessage(
      messages,
      (message) => message.type === 'result' && message.id === 3,
    )
    expect(result).toMatchObject({
      type: 'result',
      res: {
        first: { role: 'user', data: 'zero', time: 10 },
        last: { role: 'user', data: 'two', time: 30 },
        recent: [
          { role: 'char', data: 'inserted', time: 0 },
          { role: 'user', data: 'two', time: 30 },
        ],
        length: 2,
      },
      orderedMutations: [
        { type: 'setChat', index: 0, value: 'edited' },
        { type: 'insertChat', index: 1, role: 'char', value: 'inserted' },
        { type: 'setChatRole', index: 0, role: 'char' },
        { type: 'removeChat', index: 2 },
        { type: 'addChat', role: 'user', value: 'tail' },
        { type: 'cutChat', start: 1, end: 3 },
      ],
    })
  })

  it('reflects ordered invocation-local chat mutations in later reads', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        setChat(id, 0, 'edited')
        setChatRole(id, 0, 'char')
        insertChat(id, 1, 'user', 'inserted')
        removeChat(id, 2)
        addChat(id, 'char', 'tail')
        cutChat(id, 1, 4)
        return getRecentChats(id, getChatLength(id))
      end)
    `))
    runtime.handleMessage(invoke(11, {
      boundedContext: {
        messages: [
          { role: 'user', data: 'zero' },
          { role: 'char', data: 'one' },
          { role: 'user', data: 'two' },
        ],
        startIndex: 0,
        totalMessages: 3,
      },
    }))

    const result = await waitForMessage(
      messages,
      (message) => message.type === 'result' && message.id === 11,
    )
    expect(result).toMatchObject({
      type: 'result',
      res: [
        { role: 'user', data: 'inserted', time: 0 },
        { role: 'user', data: 'two', time: 0 },
        { role: 'char', data: 'tail', time: 0 },
      ],
    })
  })

  it('does not expose a caught mutation that exceeds the mutation limit', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        local succeeded = pcall(function()
          setChat(id, 0, 'rejected')
        end)
        return {
          succeeded = succeeded,
          current = getChatData(id, 0)
        }
      end)
    `, {
      limits: { ...LUA_WORKER_PROTOCOL_LIMITS, mutationCount: 0 },
    }))
    runtime.handleMessage(invoke(14, {
      boundedContext: {
        messages: [{ role: 'user', data: 'original' }],
        startIndex: 0,
        totalMessages: 1,
      },
    }))

    const result = await waitForMessage(
      messages,
      (message) => message.type === 'result' && message.id === 14,
    )
    expect(result).toMatchObject({
      type: 'result',
      res: { succeeded: false, current: 'original' },
      orderedMutations: [],
    })
  })

  it('provides the production log helper', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        log({ diagnostic = true })
        return 'logged'
      end)
    `))
    runtime.handleMessage(invoke(12))

    const result = await waitForMessage(
      messages,
      (message) => message.type === 'result' && message.id === 12,
    )
    expect(result).toMatchObject({ type: 'result', res: 'logged' })
  })

  it.each([
    ['getLoreBooks', "getLoreBooks(id, '')"],
    ['loadLoreBooks', 'loadLoreBooks(id)'],
    ['axLLM', 'axLLM(id, {})'],
    ['getCharacterImage', 'getCharacterImage(id)'],
    ['getPersonaImage', 'getPersonaImage(id)'],
  ])('classifies unsupported production helper %s explicitly', async (_name, call) => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        return ${call}
      end)
    `))
    runtime.handleMessage(invoke(13))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 13,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_unsupported_callback',
    })
  })

  it('awaits the bounded synthetic LLM host RPC', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        local response = LLM(id, {
          { role = 'user', content = 'synthetic fixture' }
        })
        return response.result
      end)
    `))
    runtime.handleMessage(invoke(4, { lowLevelAccess: true }))

    const hostCall = await waitForMessage(
      messages,
      (message) => message.type === 'hostCall' && message.id === 4,
    )
    expect(hostCall).toMatchObject({
      type: 'hostCall',
      id: 4,
      callId: 1,
      name: 'LLMMain',
      args: {
        prompt: [{ role: 'user', content: 'synthetic fixture' }],
        useMultimodal: false,
        options: {},
      },
    })

    runtime.handleMessage({
      type: 'hostResult',
      id: 4,
      callId: 1,
      result: { success: true, result: 'synthetic LLM response' },
    })
    const result = await waitForMessage(
      messages,
      (message) => message.type === 'result' && message.id === 4,
    )
    expect(result).toMatchObject({
      type: 'result',
      res: 'synthetic LLM response',
    })
  })

  it('rejects an unsupported callback explicitly', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        alertNormal(id, 'blocked')
        return value
      end)
    `))
    runtime.handleMessage(invoke(5))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 5,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_unsupported_callback',
    })
  })

  it('rejects valid history reads outside the admitted window', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        return getChat(id, 0)
      end)
    `))
    runtime.handleMessage(invoke(6, {
      boundedContext: {
        messages: [{ role: 'char', data: 'resident tail' }],
        startIndex: 1,
        totalMessages: 2,
      },
    }))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 6,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_context_window',
    })
  })

  it('rejects oversized invocation context before executing Lua', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        return 'must not execute'
      end)
    `, {
      limits: { ...LUA_WORKER_PROTOCOL_LIMITS, invocationContextBytes: 64 },
    }))
    runtime.handleMessage(invoke(7, { data: 'x'.repeat(128) }))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 7,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_context_limit',
    })
  })

  it('rejects oversized results before posting them', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        return string.rep('x', 128)
      end)
    `, {
      limits: { ...LUA_WORKER_PROTOCOL_LIMITS, resultValueBytes: 32 },
    }))
    runtime.handleMessage(invoke(8))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 8,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_output_limit',
    })
    expect(messages.some((message) => message.type === 'result' && message.id === 8)).toBe(false)
  })

  it('enforces the Wasmoon tracked-allocation memory cap', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        local allocations = {}
        for index = 1, 200000 do
          allocations[index] = string.rep('x', 64)
        end
        return #allocations
      end)
    `, {
      policy: { memoryBytes: 512 * 1024, cpuDeadlineMs: 2_000 },
    }))
    runtime.handleMessage(invoke(9))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 9,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_memory',
    })
    expect(messages.some((message) => message.type === 'result' && message.id === 9)).toBe(false)
  })

  it('enforces the Wasmoon synchronous Lua deadline', async () => {
    const messages: LuaWorkerHostMessage[] = []
    const runtime = createWasmoonLuaWorkerRuntime({
      loadJsonLua: async () => jsonLua,
      postMessage: (message) => messages.push(message),
    })
    runtime.handleMessage(register(`
      listenEdit('editInput', function(id, value, meta)
        while true do end
      end)
    `, {
      policy: { memoryBytes: 64 * 1024 * 1024, cpuDeadlineMs: 25 },
    }))
    runtime.handleMessage(invoke(10))

    const error = await waitForMessage(
      messages,
      (message) => message.type === 'error' && message.id === 10,
    )
    expect(error).toMatchObject({
      type: 'error',
      category: 'lua_worker_timeout',
    })
  })
})
