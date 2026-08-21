// @vitest-environment node

import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { beforeAll, expect, test, vi } from 'vitest'

vi.mock('../parser/parser.svelte', () => ({
  hasher: vi.fn(),
  risuChatParser: vi.fn(),
}))

vi.mock('../alert', () => ({
  alertConfirm: vi.fn(),
  alertError: vi.fn(),
  alertInput: vi.fn(),
  alertNormal: vi.fn(),
  alertSelect: vi.fn(),
}))

vi.mock('../globalApi.svelte', () => ({ fetchNative: vi.fn(), readImage: vi.fn() }))
vi.mock('../tokenizer', () => ({ tokenize: vi.fn() }))
vi.mock('../util', () => ({
  asBuffer: vi.fn(),
  getPersonaPrompt: vi.fn(),
  getUserIcon: vi.fn(),
  getUserName: vi.fn(),
}))

vi.mock('../storage/database.svelte', () => ({
  getCurrentCharacter: vi.fn(() => ({})),
  getCurrentChat: vi.fn(() => ({ message: [] })),
  getDatabase: vi.fn(() => ({ characters: [] })),
  setDatabase: vi.fn(),
}))

vi.mock('../stores.svelte', () => ({
  DBState: { db: {} },
  ReloadChatPointer: { update: vi.fn() },
  ReloadGUIPointer: { update: vi.fn() },
  selectedCharID: { subscribe: (run: (value: number) => void) => (run(0), () => undefined) },
}))

vi.mock('./modules', () => ({
  getModuleLorebooks: vi.fn(() => []),
  getModuleTriggers: vi.fn(() => []),
}))

vi.mock('./files/inlays', () => ({ getInlayAsset: vi.fn(), writeInlayImage: vi.fn() }))
vi.mock('./lorebook.svelte', () => ({ loadLoreBookV3Prompt: vi.fn() }))
vi.mock('./memory/hypamemory', () => ({ HypaProcesser: vi.fn() }))
vi.mock('./request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('./stableDiff', () => ({ generateAIImage: vi.fn() }))

let runScripted: typeof import('./scriptings').runScripted

beforeAll(async () => {
  const jsonLua = await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8')
  vi.stubGlobal('fetch', vi.fn(async () => new Response(jsonLua, { status: 200 })))
  const scriptings = await import('./scriptings')
  runScripted = scriptings.runScripted
})

test('does not stop generation when setStateChanged is a no-op', async () => {
  const result = await runScripted(
    `
      function onStart(id)
        return setStateChanged(id, "unchanged", "value")
      end
    `,
    {
      char: {} as never,
      chat: { message: [] } as never,
      setVar: () => false,
      getVar: () => 'null',
      mode: 'start',
    }
  )

  expect(result.stopSending).toBe(false)
  expect(result.res).toBeNull()
})

test('keeps explicit false as the generation stop signal', async () => {
  const result = await runScripted('function onStart() return false end', {
    char: {} as never,
    chat: { message: [] } as never,
    mode: 'start',
  })

  expect(result.res).toBe(false)
  expect(result.stopSending).toBe(true)
})

test('keeps Lua globals separate for different character IDs', async () => {
  const code = `
    counter = 0
    function phase1_character_isolation(id)
      counter = counter + 1
      return counter
    end
  `

  const firstCharacter = await runScripted(code, {
    char: { chaId: 'phase1-character-a' } as never,
    chat: { message: [] } as never,
    mode: 'phase1_character_isolation',
  })
  const secondCharacter = await runScripted(code, {
    char: { chaId: 'phase1-character-b' } as never,
    chat: { message: [] } as never,
    mode: 'phase1_character_isolation',
  })

  expect(firstCharacter.res).toBe(1)
  expect(secondCharacter.res).toBe(1)
})

test('rejects malformed Lua source on every invocation', async () => {
  const arg = {
    char: { chaId: 'phase1-malformed-source' } as never,
    chat: { message: [] } as never,
    mode: 'phase1-malformed-source',
  }

  await expect(runScripted('function onStart(', arg)).rejects.toThrow()
  await expect(runScripted('function onStart(', arg)).rejects.toThrow()
})

test('rejects concurrent malformed Lua source callers', async () => {
  const arg = {
    char: { chaId: 'phase1-concurrent-malformed-source' } as never,
    chat: { message: [] } as never,
    mode: 'phase1-concurrent-malformed-source',
  }

  const results = await Promise.allSettled([
    runScripted('function onStart(', arg),
    runScripted('function onStart(', arg),
  ])

  expect(results).toEqual([
    expect.objectContaining({ status: 'rejected' }),
    expect.objectContaining({ status: 'rejected' }),
  ])
})

test('evicts the least recently used idle Lua engine after 17 modes', async () => {
  const code = `
    counter = 0
    for i = 0, 16 do
      _G["phase1-lru-" .. i] = function(id)
        counter = counter + 1
        return counter
      end
    end
  `
  const baseArg = {
    char: { chaId: 'phase1-lru-owner' } as never,
    chat: { message: [] } as never,
  }

  for (let index = 0; index < 17; index++) {
    const result = await runScripted(code, {
      ...baseArg,
      mode: `phase1-lru-${index}`,
    })
    expect(result.res).toBe(1)
  }

  const result = await runScripted(code, {
    ...baseArg,
    mode: 'phase1-lru-0',
  })

  expect(result.res).toBe(1)
})

test('does not retain an editDisplay access ID after its handler returns', async () => {
  const setVar = vi.fn()
  const code = `
    previousId = nil
    listenEdit('editDisplay', function(id, value, meta)
      if previousId then
        setChatVar(previousId, 'stale-access', 'should-not-write')
      end
      previousId = id
      return value
    end)
  `
  const arg = {
    char: { chaId: 'phase1-edit-display-id' } as never,
    chat: { message: [] } as never,
    data: 'content',
    setVar,
    mode: 'editDisplay',
  }

  await runScripted(code, arg)
  await runScripted(code, arg)

  expect(setVar).not.toHaveBeenCalled()
})
