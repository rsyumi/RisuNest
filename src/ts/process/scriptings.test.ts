// @vitest-environment node

import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { beforeAll, expect, test, vi } from 'vitest'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { requestChatData } from './request/request'
import { readImage } from '../globalApi.svelte'
import { getDatabase } from '../storage/database.svelte'
import { asBuffer } from '../util'
import { writeInlayImage } from './files/inlays'

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
vi.mock('../platform', () => ({ isTauriMobile: true }))
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

test('rejects Python scripting on Tauri mobile before creating a Worker', async () => {
  const worker = vi.fn()
  vi.stubGlobal('Worker', worker)

  await expect(runScripted('print("blocked")', {
    char: {} as never,
    chat: { message: [] } as never,
    mode: 'tauri-mobile-python-gate',
    type: 'py',
  })).rejects.toThrow(/Python scripting is unavailable on Tauri mobile/)

  expect(worker).not.toHaveBeenCalled()
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

test('trims idle Lua engines when switching to the lower low-spec budget', async () => {
  setRuntimePerformanceProfile('normal')
  const code = `
    counter = 0
    for i = 0, 4 do
      _G["low-spec-lru-" .. i] = function(id)
        counter = counter + 1
        return counter
      end
    end
  `
  const baseArg = {
    char: { chaId: 'low-spec-lru-owner' } as never,
    chat: { message: [] } as never,
  }

  for (let index = 0; index < 5; index++) {
    const result = await runScripted(code, {
      ...baseArg,
      mode: `low-spec-lru-${index}`,
    })
    expect(result.res).toBe(1)
  }

  setRuntimePerformanceProfile('low-spec')
  const result = await runScripted(code, {
    ...baseArg,
    mode: 'low-spec-lru-0',
  })
  setRuntimePerformanceProfile('normal')

  expect(result.res).toBe(1)
})

test('keeps active Lua engines open during a profile switch and evicts after completion', async () => {
  setRuntimePerformanceProfile('normal')
  const pendingInputs = Array.from({ length: 5 }, () => {
    let resolve!: (value: unknown) => void
    const promise = new Promise<unknown>((done) => {
      resolve = done
    })
    return { promise, resolve }
  })
  const mockedRequestChatData = vi.mocked(requestChatData)
  mockedRequestChatData.mockImplementation((request) =>
    pendingInputs[Number(request.formated[0].content)].promise as never
  )
  const code = `
    counter = 0
    for i = 0, 4 do
      _G["active-low-spec-" .. i] = async(function(id)
        counter = counter + 1
        LLM(id, {{ role = "user", content = tostring(i) }})
        return counter
      end)
    end
  `
  const invocations = Array.from({ length: 5 }, (_, index) =>
    runScripted(code, {
      char: { chaId: 'active-low-spec-owner' } as never,
      chat: { message: [] } as never,
      lowLevelAccess: true,
      mode: `active-low-spec-${index}`,
    })
  )

  await vi.waitFor(() => expect(mockedRequestChatData).toHaveBeenCalledTimes(5))
  setRuntimePerformanceProfile('low-spec')

  pendingInputs[0].resolve({ type: 'success', result: 'released' })
  await expect(invocations[0]).resolves.toEqual(expect.objectContaining({ res: 1 }))

  mockedRequestChatData.mockResolvedValue({ type: 'success', result: 'released' } as never)
  const recreated = await runScripted(code, {
    char: { chaId: 'active-low-spec-owner' } as never,
    chat: { message: [] } as never,
    lowLevelAccess: true,
    mode: 'active-low-spec-0',
  })

  for (const pending of pendingInputs.slice(1)) {
    pending.resolve({ type: 'success', result: 'released' })
  }
  await Promise.all(invocations.slice(1))
  setRuntimePerformanceProfile('normal')
  mockedRequestChatData.mockReset()

  expect(recreated.res).toBe(1)
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

test('revokes the character image URL once after the inlay write succeeds', async () => {
  vi.mocked(getDatabase).mockReturnValue({
    characters: [{ type: 'character', image: 'character.jpg' }],
  } as never)
  vi.mocked(readImage).mockResolvedValue(new Uint8Array([1, 2, 3]))
  vi.mocked(asBuffer).mockReturnValue(new ArrayBuffer(3))
  vi.mocked(writeInlayImage).mockResolvedValue('character.jpg')
  vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:lua-character-success')
  const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
  const previousImage = globalThis.Image
  globalThis.Image = class {} as never

  try {
    const result = await runScripted(`
      remaining_character_image_success = async(function(id)
        return getCharacterImage(id)
      end)
    `, {
      char: { chaId: 'remaining-character-image-success' } as never,
      chat: { message: [] } as never,
      lowLevelAccess: true,
      mode: 'remaining_character_image_success',
    })

    expect(result.res).toBe('{{inlayed::character.jpg}}')
    expect(revokeObjectURL).toHaveBeenCalledTimes(1)
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:lua-character-success')
  }
  finally {
    globalThis.Image = previousImage
    vi.restoreAllMocks()
  }
})

test('revokes the character image URL once when the inlay write fails', async () => {
  vi.mocked(getDatabase).mockReturnValue({
    characters: [{ type: 'character', image: 'character.jpg' }],
  } as never)
  vi.mocked(readImage).mockResolvedValue(new Uint8Array([1, 2, 3]))
  vi.mocked(asBuffer).mockReturnValue(new ArrayBuffer(3))
  vi.mocked(writeInlayImage).mockRejectedValue(new Error('write failed'))
  vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:lua-character-failure')
  const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const previousImage = globalThis.Image
  globalThis.Image = class {} as never

  try {
    const result = await runScripted(`
      remaining_character_image_failure = async(function(id)
        return getCharacterImage(id)
      end)
    `, {
      char: { chaId: 'remaining-character-image-failure' } as never,
      chat: { message: [] } as never,
      lowLevelAccess: true,
      mode: 'remaining_character_image_failure',
    })

    expect(result.res).toBe('')
    expect(consoleError).toHaveBeenCalled()
    expect(revokeObjectURL).toHaveBeenCalledTimes(1)
    expect(revokeObjectURL).toHaveBeenCalledWith('blob:lua-character-failure')
  }
  finally {
    globalThis.Image = previousImage
    vi.restoreAllMocks()
  }
})

test('records the bounded Worker pilot pure CPU and recursive golden result', async () => {
  const result = await runScripted(`
    function k3_fibonacci(value)
      if value < 2 then
        return value
      end
      return k3_fibonacci(value - 1) + k3_fibonacci(value - 2)
    end

    function k3_pure_cpu(id)
      return k3_fibonacci(10)
    end
  `, {
    char: { chaId: 'k3-pure-cpu' } as never,
    chat: { message: [] } as never,
    mode: 'k3_pure_cpu',
  })

  expect(result).toEqual({
    chat: { message: [] },
    res: 55,
    stopSending: false,
  })
})

test('records Lua global persistence with owner and mode isolation', async () => {
  const code = `
    counter = 0

    function k3_global_a(id)
      counter = counter + 1
      return counter
    end

    function k3_global_b(id)
      counter = counter + 1
      return counter
    end
  `
  const invoke = (owner: string, mode: string) => runScripted(code, {
    char: { chaId: owner } as never,
    chat: { message: [] } as never,
    mode,
  })

  await expect(invoke('k3-owner-a', 'k3_global_a')).resolves.toMatchObject({ res: 1 })
  await expect(invoke('k3-owner-a', 'k3_global_a')).resolves.toMatchObject({ res: 2 })
  await expect(invoke('k3-owner-a', 'k3_global_b')).resolves.toMatchObject({ res: 1 })
  await expect(invoke('k3-owner-b', 'k3_global_a')).resolves.toMatchObject({ res: 1 })
})

test('records nil, null, false, arrays, and JSON round trips', async () => {
  const data = {
    dense: [1, false, 'value'],
    object: { empty: '', falseValue: false, zero: 0 },
  }
  const code = `
    function k3_nil(id)
      return nil
    end

    listenEdit('editInput', function(id, value, meta)
      return value
    end)
  `

  await expect(runScripted(code, {
    char: { chaId: 'k3-values' } as never,
    chat: { message: [] } as never,
    mode: 'k3_nil',
  })).resolves.toMatchObject({ res: null, stopSending: false })

  await expect(runScripted(code, {
    char: { chaId: 'k3-values' } as never,
    chat: { message: [] } as never,
    data: data as never,
    mode: 'editInput',
  })).resolves.toMatchObject({ res: data, stopSending: false })

  await expect(runScripted(code, {
    char: { chaId: 'k3-values-null' } as never,
    chat: { message: [] } as never,
    data: { nullValue: null } as never,
    mode: 'editInput',
  })).resolves.toMatchObject({ res: {}, stopSending: false })
})

test('records sparse arrays as a current-runtime handler error', async () => {
  const sparse: unknown[] = []
  sparse[2] = 'tail'
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

  try {
    const result = await runScripted(`
      listenEdit('editInput', function(id, value, meta)
        return value
      end)
    `, {
      char: { chaId: 'k3-sparse-array' } as never,
      chat: { message: [] } as never,
      data: { sparse } as never,
      mode: 'editInput',
    })

    expect(result).toMatchObject({ res: undefined, stopSending: false })
    expect(consoleError).toHaveBeenCalledWith(
      expect.stringContaining('invalid table: mixed or invalid key types'),
    )
  }
  finally {
    consoleError.mockRestore()
  }
})

test.each(['editRequest', 'editInput', 'editOutput', 'editDisplay'] as const)(
  'records %s listener ordering',
  async (mode) => {
    const result = await runScripted(`
      listenEdit('${mode}', function(id, value, meta)
        table.insert(value.order, 'first')
        return value
      end)

      listenEdit('${mode}', function(id, value, meta)
        table.insert(value.order, 'second')
        return value
      end)
    `, {
      char: { chaId: `k3-listener-${mode}` } as never,
      chat: { message: [] } as never,
      data: { order: [] } as never,
      mode,
    })

    expect(result).toMatchObject({
      res: { order: ['first', 'second'] },
      stopSending: false,
    })
  },
)

test('records handler errors as an empty result without partial chat mutation', async () => {
  const chat = { message: [{ role: 'user', data: 'unchanged' }] }
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

  try {
    const result = await runScripted(`
      function k3_handler_error(id)
        error('synthetic handler failure')
      end
    `, {
      char: { chaId: 'k3-handler-error' } as never,
      chat: chat as never,
      mode: 'k3_handler_error',
    })

    expect(result).toEqual({ chat, res: undefined, stopSending: false })
    expect(consoleError).toHaveBeenCalled()
  }
  finally {
    consoleError.mockRestore()
  }
})

test('records explicit false stop and ordered chat mutations', async () => {
  const result = await runScripted(`
    function k3_ordered_mutations(id)
      setChat(id, 0, 'edited')
      insertChat(id, 1, 'char', 'inserted')
      setChatRole(id, 0, 'char')
      removeChat(id, 2)
      addChat(id, 'user', 'tail')
      cutChat(id, 1, 4)
      return false
    end
  `, {
    char: { chaId: 'k3-ordered-mutations' } as never,
    chat: {
      message: [
        { role: 'user', data: 'original' },
        { role: 'char', data: 'remove-me' },
        { role: 'user', data: 'keep-me' },
      ],
    } as never,
    mode: 'k3_ordered_mutations',
  })

  expect(result).toEqual({
    chat: {
      message: [
        { role: 'char', data: 'inserted' },
        { role: 'user', data: 'keep-me' },
        { role: 'user', data: 'tail' },
      ],
    },
    res: false,
    stopSending: true,
  })
})
