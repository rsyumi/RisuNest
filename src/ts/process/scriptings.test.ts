// @vitest-environment node

import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { beforeAll, expect, test, vi } from 'vitest'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { requestChatData } from './request/request'
import { readImage } from '../globalApi.svelte'
import { getCurrentCharacter, getCurrentChat, getDatabase } from '../storage/database.svelte'
import { asBuffer, getUserIcon } from '../util'
import { writeInlayImage } from './files/inlays'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { createConversationOperationContext } from './conversationOperationContext'
import type { Chat, character } from '../storage/database.svelte'
import { DBState } from '../stores.svelte'
import { risuChatParser } from '../parser/parser.svelte'
import type { LuaEngine } from 'wasmoon'
import { appendDefaultChatInput } from '../../lib/ChatScreens/defaultChatInput'
import {
  captureConversationMutationTarget,
  isConversationMutationTargetCurrent,
} from '../conversationMutations'
import { captureGenerationConversationOperation } from './generationConversationOperation'
import { createChatParserDependencyStamp } from '../chatRenderIdentity'
import { alertConfirm, alertNormal } from '../alert'

const continuationRuntime = vi.hoisted(() => ({
  session: null as ActiveConversationSession | null,
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
  peekActiveConversationSession: () => continuationRuntime.session,
}))

const scriptingSelectionState = vi.hoisted(() => ({ index: 0 }))
const createdLuaEngines = vi.hoisted(() => [] as LuaEngine[])
const createdLuaEngineOptions = vi.hoisted(() => [] as unknown[])

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
  parseKeyValue: vi.fn(() => []),
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
  ReloadGUIPointer: {
    subscribe: (run: (value: number) => void) => {
      run(0)
      return () => {}
    },
    set: vi.fn(),
    update: vi.fn(),
  },
  CurrentTriggerIdStore: {
    subscribe: (run: (value: null) => void) => {
      run(null)
      return () => {}
    },
    set: vi.fn(),
  },
  selectedCharID: {
    subscribe: (run: (value: number) => void) => (
      run(scriptingSelectionState.index),
      () => undefined
    ),
  },
}))

vi.mock('./modules', () => ({
  getModuleLorebooks: vi.fn(() => []),
  getModuleTriggers: vi.fn(() => []),
}))

vi.mock('./files/inlays', () => ({ getInlayAsset: vi.fn(), writeInlayImage: vi.fn() }))
vi.mock('./lorebook.svelte', () => ({ loadLoreBookV3PromptFromCompatibilitySnapshot: vi.fn() }))
vi.mock('./memory/hypamemory', () => ({ HypaProcesser: vi.fn() }))
vi.mock('./request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('./stableDiff', () => ({ generateAIImage: vi.fn() }))
vi.mock('./command', () => ({ processMultiCommand: vi.fn() }))
vi.mock('./luaRuntime', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./luaRuntime')>()
  return {
    ...actual,
    async createLuaFactory() {
      const factory = await actual.createLuaFactory()
      const createEngine = factory.createEngine.bind(factory)
      factory.createEngine = async (options) => {
        createdLuaEngineOptions.push(options)
        const engine = await createEngine(options)
        createdLuaEngines.push(engine)
        return engine
      }
      return factory
    },
  }
})

let runScripted: typeof import('./scriptings').runScripted
let jsonLuaSource = ''

function operationCharacterFixture(
  suffix: string,
  onMutation?: () => void,
) {
  const chat = {
    id: `lua-metadata-chat-${suffix}`,
    message: [{ role: 'user', data: 'message', chatId: `message-${suffix}` }],
    localLore: [],
  } as unknown as Chat
  const char = {
    type: 'character',
    chaId: `lua-metadata-character-${suffix}`,
    name: 'name-before',
    desc: 'description-before',
    chatPage: 0,
    chats: [chat],
    firstMessage: 'first-before',
    backgroundHTML: 'background-before',
    alternateGreetings: [],
    globalLore: [],
    defaultVariables: '',
  } as unknown as character
  const database = {
    characters: [char],
    templateDefaultVariables: '',
  }
  const session = new ActiveConversationSession({
    characterId: char.chaId,
    conversationId: chat.id!,
    conversation: chat,
    storeRevision: 14,
    onMutation,
  })
  return { chat, char, database, session }
}

function installOperationCharacterFixture(
  fixture: ReturnType<typeof operationCharacterFixture>,
) {
  scriptingSelectionState.index = 0
  DBState.db = fixture.database as never
  vi.mocked(getDatabase).mockReturnValue(fixture.database as never)
}

beforeAll(async () => {
  jsonLuaSource = await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8')
  vi.stubGlobal('fetch', vi.fn(async () => new Response(jsonLuaSource, { status: 200 })))
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

test('captures field ownership before the Lua factory wait and rejects navigation', async () => {
  const original = operationCharacterFixture('factory-owner-original')
  const replacement = operationCharacterFixture('factory-owner-replacement')
  installOperationCharacterFixture(original)
  const originalFetch = globalThis.fetch
  let releaseFetch!: () => void
  const fetchGate = new Promise<void>((resolve) => {
    releaseFetch = resolve
  })
  const delayedFetch = vi.fn(async () => {
    await fetchGate
    return new Response(jsonLuaSource, { status: 200 })
  })
  vi.stubGlobal('fetch', delayedFetch)

  try {
    const pending = runScripted(`
      listenEdit('editInput', function(id, value, meta)
        setName(id, 'stale-name')
        setDescription(id, 'stale-description')
        setCharacterFirstMessage(id, 'stale-first')
        setBackgroundEmbedding(id, 'stale-background')
        return value
      end)
    `, {
      char: original.char,
      chat: original.chat,
      data: 'input',
      mode: 'editInput',
    })
    await vi.waitFor(() => expect(delayedFetch).toHaveBeenCalled(), {
      timeout: 5_000,
    })
    DBState.db = replacement.database as never
    vi.mocked(getDatabase).mockReturnValue(replacement.database as never)
    scriptingSelectionState.index = 0
    releaseFetch()
    await pending

    expect(original.char.name).toBe('name-before')
    expect(original.char.desc).toBe('description-before')
    expect(original.char.firstMessage).toBe('first-before')
    expect(original.char.backgroundHTML).toBe('background-before')
    expect(replacement.char.name).toBe('name-before')
    expect(replacement.char.desc).toBe('description-before')
    expect(replacement.char.firstMessage).toBe('first-before')
    expect(replacement.char.backgroundHTML).toBe('background-before')
  } finally {
    releaseFetch()
    vi.stubGlobal('fetch', originalFetch)
    installOperationCharacterFixture(original)
  }
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
    characters: [{
      type: 'character',
      chaId: 'remaining-character-image-success',
      image: 'character.jpg',
    }],
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

test('character and persona image APIs preserve the mapped Inlay identity returned by the writer', async () => {
  vi.mocked(getDatabase).mockReturnValue({
    characters: [{
      type: 'character',
      chaId: 'mapped-lua-images',
      image: 'assets/character-source.png',
    }],
  } as never)
  vi.mocked(getUserIcon).mockReturnValue('assets/persona-source.png')
  vi.mocked(readImage).mockResolvedValue(new Uint8Array([1, 2, 3]))
  vi.mocked(asBuffer).mockReturnValue(new ArrayBuffer(3))
  vi.mocked(writeInlayImage).mockImplementation(async (_image, options) =>
    options.id?.replace(/^assets\//, '') ?? '')
  vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:mapped-lua-image')
  vi.spyOn(URL, 'revokeObjectURL').mockImplementation(() => undefined)
  const previousImage = globalThis.Image
  globalThis.Image = class {} as never

  try {
    const character = await runScripted(`
      mapped_character_image = async(function(id)
        return getCharacterImage(id)
      end)
    `, {
      char: { chaId: 'mapped-lua-images' } as never,
      chat: { message: [] } as never,
      lowLevelAccess: true,
      mode: 'mapped_character_image',
    })
    const persona = await runScripted(`
      mapped_persona_image = async(function(id)
        return getPersonaImage(id)
      end)
    `, {
      char: { chaId: 'mapped-lua-images' } as never,
      chat: { message: [] } as never,
      lowLevelAccess: true,
      mode: 'mapped_persona_image',
    })

    expect(character.res).toBe('{{inlayed::character-source.png}}')
    expect(persona.res).toBe('{{inlayed::persona-source.png}}')
    expect(vi.mocked(writeInlayImage).mock.calls.slice(-2).map(([, options]) => options?.id)).toEqual([
      'assets/character-source.png',
      'assets/persona-source.png',
    ])
  }
  finally {
    globalThis.Image = previousImage
    vi.restoreAllMocks()
  }
})

test('revokes the character image URL once when the inlay write fails', async () => {
  vi.mocked(getDatabase).mockReturnValue({
    characters: [{
      type: 'character',
      chaId: 'remaining-character-image-failure',
      image: 'character.jpg',
    }],
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

    listenEdit('editInput', function(id, value, meta)
      return k3_fibonacci(10)
    end)
  `, {
    char: { chaId: 'k3-pure-cpu' } as never,
    chat: { message: [] } as never,
    mode: 'editInput',
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

    listenEdit('editInput', function(id, value, meta)
      counter = counter + 1
      return counter
    end)

    listenEdit('editOutput', function(id, value, meta)
      counter = counter + 1
      return counter
    end)
  `
  const invoke = (owner: string, mode: 'editInput' | 'editOutput') => runScripted(code, {
    char: { chaId: owner } as never,
    chat: { message: [] } as never,
    mode,
  })

  await expect(invoke('k3-owner-a', 'editInput')).resolves.toMatchObject({ res: 1 })
  await expect(invoke('k3-owner-a', 'editInput')).resolves.toMatchObject({ res: 2 })
  await expect(invoke('k3-owner-a', 'editOutput')).resolves.toMatchObject({ res: 1 })
  await expect(invoke('k3-owner-b', 'editInput')).resolves.toMatchObject({ res: 1 })
})

test('releases cached invocation state while preserving Lua globals', async () => {
  const fixture = operationCharacterFixture('cached-invocation-release')
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )
  const code = `
    retained_counter = retained_counter or 0
    listenEdit('editInput', function(id, value)
      retained_counter = retained_counter + 1
      if value == 'error' then
        error('synthetic cached invocation failure')
      end
      if value == 'cancel' then
        stopChat(id)
      end
      return retained_counter
    end)
  `

  const first = await runScripted(code, {
    char: fixture.char,
    data: 'success',
    mode: 'editInput',
    operationContext,
  })
  const engine = createdLuaEngines.at(-1)!
  const getFullChat = engine.global.get('getFullChatMain') as (id: string) => string
  const getChatVar = engine.global.get('getChatVar') as (id: string, key: string) => string
  const parseCbs = engine.global.get('cbs') as (value: string) => string
  const emptyDatabase = { characters: [] }
  DBState.db = emptyDatabase as never
  vi.mocked(getDatabase).mockReturnValue(emptyDatabase as never)
  vi.mocked(risuChatParser).mockClear()

  expect(first.res).toBe(1)
  expect(() => getFullChat('inactive')).toThrow(/Cannot read properties of undefined/)
  expect(() => getChatVar('inactive', 'key')).toThrow(/is not a function/)
  parseCbs('after invocation')
  expect(risuChatParser).toHaveBeenLastCalledWith('after invocation', {
    chara: undefined,
    db: undefined,
    selectedCharacterId: undefined,
    getChatVar: undefined,
    setChatVar: undefined,
  })

  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const failed = await runScripted(code, {
    char: fixture.char,
    data: 'error',
    mode: 'editInput',
    operationContext,
  })
  consoleError.mockRestore()

  expect(failed.res).toBeUndefined()
  expect(() => getFullChat('inactive')).toThrow(/Cannot read properties of undefined/)

  const second = await runScripted(code, {
    char: { chaId: fixture.char.chaId } as never,
    chat: { message: [] } as never,
    data: 'cancel',
    mode: 'editInput',
  })

  expect(second.res).toBe(3)
  expect(second.stopSending).toBe(true)
  expect(createdLuaEngines.at(-1)).toBe(engine)
  expect(() => getFullChat('inactive')).toThrow(/Cannot read properties of undefined/)
})

test('records nil, null, false, arrays, and JSON round trips', async () => {
  const data = {
    dense: [1, false, 'value'],
    object: { empty: '', falseValue: false, zero: 0 },
  }
  const code = `
    listenEdit('editInput', function(id, value, meta)
      return nil
    end)
  `

  await expect(runScripted(code, {
    char: { chaId: 'k3-values' } as never,
    chat: { message: [] } as never,
    mode: 'editInput',
  })).resolves.toMatchObject({ res: null, stopSending: false })

  const roundTripCode = `
    listenEdit('editInput', function(id, value, meta)
      return value
    end)
  `
  await expect(runScripted(roundTripCode, {
    char: { chaId: 'k3-values-round-trip' } as never,
    chat: { message: [] } as never,
    data: data as never,
    mode: 'editInput',
  })).resolves.toMatchObject({ res: data, stopSending: false })

  await expect(runScripted(roundTripCode, {
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

test('records eager partial chat mutation before a listener handler error', async () => {
  const chat = { message: [{ role: 'user', data: 'before-error' }] }
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

  try {
    const result = await runScripted(`
      listenEdit('editInput', function(id, value, meta)
        setChat(id, 0, 'mutated-before-error')
        error('synthetic handler failure')
      end)
    `, {
      char: { chaId: 'k3-handler-error' } as never,
      chat: chat as never,
      mode: 'editInput',
    })

    expect(result).toEqual({
      chat: { message: [{ role: 'user', data: 'mutated-before-error' }] },
      res: undefined,
      stopSending: false,
    })
    expect(consoleError).toHaveBeenCalled()
  }
  finally {
    consoleError.mockRestore()
  }
})

test('records explicit false stop and ordered chat mutations', async () => {
  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      setChat(id, 0, 'edited')
      insertChat(id, 1, 'char', 'inserted')
      setChatRole(id, 0, 'char')
      removeChat(id, 2)
      addChat(id, 'user', 'tail')
      cutChat(id, 1, 4)
      return false
    end)
  `, {
    char: { chaId: 'k3-ordered-mutations' } as never,
    chat: {
      message: [
        { role: 'user', data: 'original' },
        { role: 'char', data: 'remove-me' },
        { role: 'user', data: 'keep-me' },
      ],
    } as never,
    mode: 'editInput',
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

test('runs ordered Lua chat mutations inside one versioned conversation operation', async () => {
  const conversation = {
    id: 'lua-operation-chat',
    message: [
      { role: 'user', data: 'original', chatId: 'message-0' },
      { role: 'char', data: 'remove-me', chatId: 'message-1' },
      { role: 'user', data: 'keep-me', chatId: 'message-2' },
    ],
  } as Chat
  const session = new ActiveConversationSession({
    characterId: 'lua-operation-character',
    conversationId: 'lua-operation-chat',
    conversation,
    storeRevision: 11,
  })
  const operationContext = createConversationOperationContext(session, conversation)

  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      setChat(id, 0, 'edited')
      insertChat(id, 1, 'char', 'inserted')
      removeChat(id, 2)
      addChat(id, 'user', 'tail')
      return false
    end)
  `, {
    char: { chaId: 'lua-operation-character' } as never,
    mode: 'editInput',
    operationContext,
  })

  expect(result.stopSending).toBe(true)
  expect(conversation.message.map((entry) => entry.data)).toEqual([
    'original',
    'remove-me',
    'keep-me',
  ])
  expect(operationContext.chat.message.map((entry) => entry.data)).toEqual([
    'edited',
    'inserted',
    'keep-me',
    'tail',
  ])

  operationContext.commit(session)
  expect(conversation.message.map((entry) => entry.data)).toEqual([
    'edited',
    'inserted',
    'keep-me',
    'tail',
  ])
})

test('keeps the eager Lua mutation in the operation batch after a handler error', async () => {
  const conversation = {
    id: 'lua-partial-chat',
    message: [{ role: 'user', data: 'before-error', chatId: 'message-0' }],
  } as Chat
  const session = new ActiveConversationSession({
    characterId: 'lua-partial-character',
    conversationId: 'lua-partial-chat',
    conversation,
    storeRevision: 12,
  })
  const operationContext = createConversationOperationContext(session, conversation)
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

  try {
    const result = await runScripted(`
      listenEdit('editInput', function(id, value, meta)
        setChat(id, 0, 'mutated-before-error')
        error('synthetic operation failure')
      end)
    `, {
      char: { chaId: 'lua-partial-character' } as never,
      mode: 'editInput',
      operationContext,
    })

    expect(result.res).toBeUndefined()
    operationContext.commit(session)
    expect(conversation.message[0].data).toBe('mutated-before-error')
  }
  finally {
    consoleError.mockRestore()
  }
})

test('publishes local lore through the conversation metadata batch', async () => {
  const fixture = operationCharacterFixture('local-lore-success')
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )

  await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      upsertLocalLoreBook(id, 'operation lore', 'operation content', {})
      return value
    end)
  `, {
    char: fixture.char,
    data: 'input',
    mode: 'editInput',
    operationContext,
  })

  expect(fixture.chat.localLore).toEqual([])
  expect(operationContext.chat.localLore).toEqual([
    expect.objectContaining({
      comment: 'operation lore',
      content: 'operation content',
    }),
  ])

  operationContext.commit(fixture.session)
  expect(fixture.chat.localLore).toEqual([
    expect.objectContaining({ comment: 'operation lore' }),
  ])
  expect(fixture.session.version).toBe(1)
})

test('keeps local lore partial-error semantics inside the original owner', async () => {
  const fixture = operationCharacterFixture('local-lore-partial')
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

  try {
    const result = await runScripted(`
      listenEdit('editInput', function(id, value, meta)
        upsertLocalLoreBook(id, 'partial lore', 'partial content', {})
        error('after local lore mutation')
      end)
    `, {
      char: fixture.char,
      mode: 'editInput',
      operationContext,
    })

    expect(result.res).toBeUndefined()
    expect(fixture.chat.localLore).toEqual([])
    operationContext.commit(fixture.session)
    expect(fixture.chat.localLore).toEqual([
      expect.objectContaining({ comment: 'partial lore' }),
    ])
  } finally {
    consoleError.mockRestore()
  }
})

test('does not publish local lore after conversation navigation', async () => {
  const original = operationCharacterFixture('local-lore-original')
  const replacement = operationCharacterFixture('local-lore-replacement')
  installOperationCharacterFixture(original)
  const operationContext = createConversationOperationContext(
    original.session,
    original.chat,
  )

  await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      upsertLocalLoreBook(id, 'stale lore', 'stale content', {})
      return value
    end)
  `, {
    char: original.char,
    data: 'input',
    mode: 'editInput',
    operationContext,
  })

  expect(() => operationContext.commit(replacement.session)).toThrow(/inactive/i)
  expect(original.chat.localLore).toEqual([])
  expect(replacement.chat.localLore).toEqual([])
})

test('rolls local lore back when metadata publication fails', async () => {
  const fixture = operationCharacterFixture('local-lore-rollback', () => {
    throw new Error('local lore publication failed')
  })
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )

  await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      upsertLocalLoreBook(id, 'rollback lore', 'rollback content', {})
      return value
    end)
  `, {
    char: fixture.char,
    data: 'input',
    mode: 'editInput',
    operationContext,
  })

  expect(() => operationContext.commit(fixture.session)).toThrow(
    'local lore publication failed',
  )
  expect(fixture.chat.localLore).toEqual([])
  expect(fixture.session.version).toBe(0)
})

test('updates only captured live character fields without installing the projection', async () => {
  const fixture = operationCharacterFixture('captured-fields')
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )
  const liveCharacter = fixture.database.characters[0]
  const liveChat = liveCharacter.chats[0]

  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      setName(id, 'name-after')
      setDescription(id, 'description-after')
      return {
        first = setCharacterFirstMessage(id, 'first-after'),
        background = setBackgroundEmbedding(id, 'background-after')
      }
    end)
  `, {
    char: fixture.char,
    mode: 'editInput',
    operationContext,
  })

  expect(result.res).toEqual({ first: true, background: true })
  expect(fixture.database.characters[0]).toBe(liveCharacter)
  expect(fixture.database.characters[0].chats[0]).toBe(liveChat)
  expect(liveCharacter.name).toBe('name-after')
  expect(liveCharacter.desc).toBe('description-after')
  expect(liveCharacter.firstMessage).toBe('first-after')
  expect(liveCharacter.backgroundHTML).toBe('background-after')
  expect(fixture.session.version).toBe(0)
  operationContext.release()
})

test('re-finds the captured character after an awaited character array reorder', async () => {
  const fixture = operationCharacterFixture('captured-field-reorder')
  const replacement = operationCharacterFixture('captured-field-replacement')
  fixture.database.characters.push(replacement.char)
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )
  let resolveRequest!: (value: unknown) => void
  const pendingRequest = new Promise<unknown>((resolve) => {
    resolveRequest = resolve
  })
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockReturnValueOnce(pendingRequest as never)

  const pending = runScripted(`
    captured_field_reorder = async(function(id)
      LLM(id, {{ role = 'user', content = 'wait' }})
      setName(id, 'reordered-name')
      setDescription(id, 'reordered-description')
      return {
        first = setCharacterFirstMessage(id, 'reordered-first'),
        background = setBackgroundEmbedding(id, 'reordered-background')
      }
    end)
  `, {
    char: fixture.char,
    lowLevelAccess: true,
    mode: 'captured_field_reorder',
    operationContext,
  })
  await vi.waitFor(() => expect(requestChatData).toHaveBeenCalledOnce())
  fixture.database.characters.reverse()
  scriptingSelectionState.index = 0
  resolveRequest({ type: 'success', result: 'done' })

  await expect(pending).resolves.toEqual(expect.objectContaining({
    res: { first: true, background: true },
  }))
  expect(fixture.char.name).toBe('reordered-name')
  expect(fixture.char.desc).toBe('reordered-description')
  expect(fixture.char.firstMessage).toBe('reordered-first')
  expect(fixture.char.backgroundHTML).toBe('reordered-background')
  expect(replacement.char.name).toBe('name-before')
  expect(replacement.char.desc).toBe('description-before')
  expect(replacement.char.firstMessage).toBe('first-before')
  expect(replacement.char.backgroundHTML).toBe('background-before')
  expect(fixture.database.characters[1]).toBe(fixture.char)
  operationContext.release()
  vi.mocked(requestChatData).mockReset()
})

test('rejects all captured field writes after a same-ID database replacement', async () => {
  const original = operationCharacterFixture('same-id-owner-original')
  const replacement = operationCharacterFixture('same-id-owner-replacement')
  replacement.char.chaId = original.char.chaId
  installOperationCharacterFixture(original)
  let resolveRequest!: (value: unknown) => void
  const pendingRequest = new Promise<unknown>((resolve) => {
    resolveRequest = resolve
  })
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockReturnValueOnce(pendingRequest as never)

  const pending = runScripted(`
    same_id_field_owner = async(function(id)
      LLM(id, {{ role = 'user', content = 'wait' }})
      setName(id, 'stale-name')
      setDescription(id, 'stale-description')
      return {
        first = setCharacterFirstMessage(id, 'stale-first'),
        background = setBackgroundEmbedding(id, 'stale-background')
      }
    end)
  `, {
    char: original.char,
    chat: original.chat,
    lowLevelAccess: true,
    mode: 'same_id_field_owner',
  })
  await vi.waitFor(() => expect(requestChatData).toHaveBeenCalledOnce())
  original.database.characters[0] = replacement.char
  resolveRequest({ type: 'success', result: 'done' })

  await expect(pending).resolves.toEqual(expect.objectContaining({
    res: { first: false, background: false },
  }))
  expect(original.char.name).toBe('name-before')
  expect(original.char.desc).toBe('description-before')
  expect(original.char.firstMessage).toBe('first-before')
  expect(original.char.backgroundHTML).toBe('background-before')
  expect(replacement.char.name).toBe('name-before')
  expect(replacement.char.desc).toBe('description-before')
  expect(replacement.char.firstMessage).toBe('first-before')
  expect(replacement.char.backgroundHTML).toBe('background-before')
  vi.mocked(requestChatData).mockReset()
})

test('does not fall back to another selection after waiting for the Lua mutex', async () => {
  const original = operationCharacterFixture('mutex-owner-original')
  const replacement = operationCharacterFixture('mutex-owner-replacement')
  original.database.characters.push(replacement.char)
  installOperationCharacterFixture(original)
  let resolveRequest!: (value: unknown) => void
  const pendingRequest = new Promise<unknown>((resolve) => {
    resolveRequest = resolve
  })
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockReturnValueOnce(pendingRequest as never)
  const code = `
    mutex_owner_calls = 0
    mutex_field_owner = async(function(id)
      mutex_owner_calls = mutex_owner_calls + 1
      if mutex_owner_calls == 1 then
        LLM(id, {{ role = 'user', content = 'wait' }})
        return 'holder'
      end
      setName(id, 'stale-name')
      setDescription(id, 'stale-description')
      setCharacterFirstMessage(id, 'stale-first')
      setBackgroundEmbedding(id, 'stale-background')
      return 'writer'
    end)
  `

  const holder = runScripted(code, {
    char: original.char,
    chat: original.chat,
    lowLevelAccess: true,
    mode: 'mutex_field_owner',
  })
  await vi.waitFor(() => expect(requestChatData).toHaveBeenCalledOnce())
  const waiter = runScripted(code, {
    char: original.char,
    chat: original.chat,
    lowLevelAccess: true,
    mode: 'mutex_field_owner',
  })
  original.database.characters = [replacement.char]
  scriptingSelectionState.index = 0
  resolveRequest({ type: 'success', result: 'done' })

  await expect(Promise.all([holder, waiter])).resolves.toEqual([
    expect.objectContaining({ res: 'holder' }),
    expect.objectContaining({ res: 'writer' }),
  ])
  expect(original.char.name).toBe('name-before')
  expect(original.char.desc).toBe('description-before')
  expect(original.char.firstMessage).toBe('first-before')
  expect(original.char.backgroundHTML).toBe('background-before')
  expect(replacement.char.name).toBe('name-before')
  expect(replacement.char.desc).toBe('description-before')
  expect(replacement.char.firstMessage).toBe('first-before')
  expect(replacement.char.backgroundHTML).toBe('background-before')
  vi.mocked(requestChatData).mockReset()
})

test('rejects awaited captured field writes after a concurrent update', async () => {
  const fixture = operationCharacterFixture('captured-field-cas')
  installOperationCharacterFixture(fixture)
  const operationContext = createConversationOperationContext(
    fixture.session,
    fixture.chat,
  )
  let resolveRequest!: (value: unknown) => void
  const pendingRequest = new Promise<unknown>((resolve) => {
    resolveRequest = resolve
  })
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockReturnValueOnce(pendingRequest as never)

  const pending = runScripted(`
    captured_field_cas = async(function(id)
      LLM(id, {{ role = 'user', content = 'wait' }})
      setName(id, 'stale-name')
      setDescription(id, 'stale-description')
      return {
        first = setCharacterFirstMessage(id, 'stale-first'),
        background = setBackgroundEmbedding(id, 'stale-background')
      }
    end)
  `, {
    char: fixture.char,
    lowLevelAccess: true,
    mode: 'captured_field_cas',
    operationContext,
  })
  await vi.waitFor(() => expect(requestChatData).toHaveBeenCalledOnce())
  fixture.char.name = 'concurrent-name'
  fixture.char.desc = 'concurrent-description'
  fixture.char.firstMessage = 'concurrent-first'
  fixture.char.backgroundHTML = 'concurrent-background'
  resolveRequest({ type: 'success', result: 'done' })

  await expect(pending).resolves.toEqual(expect.objectContaining({
    res: { first: false, background: false },
  }))
  expect(fixture.char.name).toBe('concurrent-name')
  expect(fixture.char.desc).toBe('concurrent-description')
  expect(fixture.char.firstMessage).toBe('concurrent-first')
  expect(fixture.char.backgroundHTML).toBe('concurrent-background')
  expect(fixture.database.characters[0]).toBe(fixture.char)
  operationContext.release()
  vi.mocked(requestChatData).mockReset()
})

test('Lua history and chat variables stay bound to the captured character', async () => {
  const originalChat = {
    id: 'lua-captured-chat',
    message: [{ role: 'user', data: 'user-only', chatId: 'message-0' }],
    scriptstate: {},
  } as Chat
  const replacementChat = {
    id: 'lua-global-chat',
    message: [],
    scriptstate: {},
  } as Chat
  const originalCharacter = {
    type: 'character',
    chaId: 'lua-captured-character',
    chatPage: 0,
    chats: [originalChat],
    firstMessage: 'captured-first-message',
    alternateGreetings: [],
    defaultVariables: '',
  }
  const replacementCharacter = {
    type: 'character',
    chaId: 'lua-global-character',
    chatPage: 0,
    chats: [replacementChat],
    firstMessage: 'global-first-message',
    alternateGreetings: [],
    defaultVariables: '',
  }
  const database = {
    characters: [originalCharacter, replacementCharacter],
    templateDefaultVariables: '',
  }
  const session = new ActiveConversationSession({
    characterId: originalCharacter.chaId,
    conversationId: originalChat.id!,
    conversation: originalChat,
    storeRevision: 13,
  })
  const operationContext = createConversationOperationContext(session, originalChat)
  scriptingSelectionState.index = 1
  vi.mocked(getDatabase).mockReturnValue(database as never)
  vi.mocked(getCurrentCharacter).mockReturnValue(replacementCharacter as never)

  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      setChatVar(id, 'operation_key', '')
      return getCharacterLastMessage(id)
    end)
  `, {
    char: originalCharacter as never,
    mode: 'editInput',
    operationContext,
  })

  expect(result.res).toBe('captured-first-message')
  expect(operationContext.chat.scriptstate).toEqual({ '$operation_key': '' })
  expect(replacementChat.scriptstate).toEqual({})

  operationContext.commit(session)
  expect(originalChat.scriptstate).toEqual({ '$operation_key': '' })
  scriptingSelectionState.index = 0
})

test('records absolute and negative chat reads plus recent and length semantics', async () => {
  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      return {
        first = getChat(id, 0),
        last = getChat(id, -1),
        lastData = getChatData(id, -1),
        firstRole = getChatRole(id, 0),
        missingChatIsNil = getChat(id, 99) == nil,
        missingNegativeIsNil = getChat(id, -4) == nil,
        missingData = getChatData(id, 99),
        missingRole = getChatRole(id, 99),
        recent = getRecentChats(id, 2),
        recentAll = getRecentChats(id, 99),
        recentZero = getRecentChats(id, 0),
        length = getChatLength(id)
      }
    end)
  `, {
    char: { chaId: 'k3-chat-reads' } as never,
    chat: {
      message: [
        { role: 'user', data: 'zero', time: 10 },
        { role: 'char', data: 'one' },
        { role: 'user', data: 'two', time: 30 },
      ],
    } as never,
    mode: 'editInput',
  })

  expect(result.res).toEqual({
    first: { role: 'user', data: 'zero', time: 10 },
    last: { role: 'user', data: 'two', time: 30 },
    lastData: 'two',
    firstRole: 'user',
    missingChatIsNil: true,
    missingNegativeIsNil: true,
    missingData: '',
    missingRole: '',
    recent: [
      { role: 'char', data: 'one', time: 0 },
      { role: 'user', data: 'two', time: 30 },
    ],
    recentAll: [
      { role: 'user', data: 'zero', time: 10 },
      { role: 'char', data: 'one', time: 0 },
      { role: 'user', data: 'two', time: 30 },
    ],
    recentZero: [],
    length: 3,
  })
})

test('records the synthetic LLM Promise await result in a supported listener mode', async () => {
  const mockedRequestChatData = vi.mocked(requestChatData)
  mockedRequestChatData.mockReset()
  mockedRequestChatData.mockResolvedValueOnce({
    type: 'success',
    result: 'synthetic LLM response',
  } as never)

  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      local response = LLM(id, {
        { role = 'user', content = 'synthetic fixture' }
      })
      return response.result
    end)
  `, {
    char: { chaId: 'k3-promise-await' } as never,
    chat: { message: [] } as never,
    lowLevelAccess: true,
    mode: 'editInput',
  })

  expect(result).toMatchObject({
    res: 'synthetic LLM response',
    stopSending: false,
  })
  expect(mockedRequestChatData).toHaveBeenCalledTimes(1)
  expect(requestChatData).toHaveBeenCalledWith({
    formated: [{ role: 'user', content: 'synthetic fixture' }],
    bias: {},
    useStreaming: false,
    forceStreaming: false,
    noMultiGen: true,
  }, 'model')
})

test('creates production Lua engines with the handler deadline', async () => {
  await runScripted('', {
    char: { chaId: 'bounded-handler-options' } as never,
    chat: { message: [] } as never,
    mode: 'bounded-handler-options',
  })

  expect(createdLuaEngineOptions.at(-1)).toEqual({
    injectObjects: true,
    functionTimeout: 2_000,
  })
})

test('resumes a Lua listener after an LLM wait longer than its CPU deadline', async () => {
  let now = Date.now()
  const clock = vi.spyOn(Date, 'now').mockImplementation(() => now)
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockImplementationOnce(async () => {
    await Promise.resolve()
    now += 5_000
    return { type: 'success', result: 'synthetic delayed response' } as never
  })

  try {
    const result = await runScripted(
      `
      listenEdit('editInput', function(id, value, meta)
        local response = LLM(id, {{ role = 'user', content = 'fixture' }})
        local total = 0
        for i = 1, 10000 do total = total + i end
        return response.result .. ':' .. total
      end)
    `,
      {
        char: { chaId: 'delayed-listener-cpu-deadline' } as never,
        chat: { message: [] } as never,
        lowLevelAccess: true,
        mode: 'editInput',
      },
    )

    expect(result.res).toBe('synthetic delayed response:50005000')
    expect(errors).not.toHaveBeenCalled()
  } finally {
    clock.mockRestore()
    errors.mockRestore()
    vi.mocked(requestChatData).mockReset()
  }
})

test('commits overlapping Lua button updates in click order', async () => {
  const fixture = operationCharacterFixture('ordered-buttons')
  fixture.chat.scriptstate = { $counter: '0' }
  fixture.char.triggerscript = [
    {
      type: 'start',
      conditions: [],
      effect: [
        {
          type: 'triggerlua',
          code: `
      function onButtonClick(id, data)
        setChatVar(id, 'counter', tostring(tonumber(getChatVar(id, 'counter')) + 1))
        addChat(id, 'char', data)
      end
    `,
        },
      ],
    },
  ] as never
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const { runLuaButtonTrigger } = await import('./scriptings')
  try {
    const results = await Promise.allSettled([
      runLuaButtonTrigger(fixture.char, 'first'),
      runLuaButtonTrigger(fixture.char, 'second'),
      runLuaButtonTrigger(fixture.char, 'third'),
    ])
    expect(results.map((result) => result.status)).toEqual([
      'fulfilled',
      'fulfilled',
      'fulfilled',
    ])
    expect(fixture.chat.scriptstate.$counter).toBe('3')
    expect(fixture.chat.message.map((entry) => entry.data)).toEqual([
      'message',
      'first',
      'second',
      'third',
    ])
    expect(fixture.session.activePinReasons).toEqual([])
  } finally {
    continuationRuntime.session = null
  }
})

test.each(['complete', 'navigate', 'external-edit'] as const)(
  'orders buttons and manual triggers across an awaited confirmation (%s)',
  async (outcome) => {
    const fixture = operationCharacterFixture(`awaited-buttons-${outcome}`)
    fixture.chat.scriptstate = { $counter: '0' }
    fixture.char.triggerscript = [
      {
        type: 'start',
        conditions: [],
        effect: [
          {
            type: 'triggerlua',
            code: `
        onButtonClick = async(function(id, data)
          if data == 'first' then alertConfirm(id, 'synthetic confirmation'):await() end
          if data ~= 'first' then alertNormal(id, 'synthetic button effect') end
          setChatVar(id, 'counter', tostring(tonumber(getChatVar(id, 'counter')) + 1))
          addChat(id, 'char', data)
        end)
        function manualAction(id)
          alertNormal(id, 'synthetic manual effect')
          setChatVar(id, 'counter', tostring(tonumber(getChatVar(id, 'counter')) + 1))
          addChat(id, 'char', 'manual')
        end
      `,
          },
        ],
      },
    ] as never
    installOperationCharacterFixture(fixture)
    vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
    vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
    continuationRuntime.session = fixture.session
    let confirm!: (value: boolean) => void
    vi.mocked(alertNormal).mockClear()
    vi.mocked(alertConfirm).mockImplementationOnce(
      () =>
        new Promise<boolean>((resolve) => {
          confirm = resolve
        }),
    )
    const { runLuaButtonTrigger } = await import('./scriptings')
    const { runTrigger } = await import('./triggers')
    try {
      const first = runLuaButtonTrigger(fixture.char, 'first')
      await vi.waitFor(() => expect(confirm).toBeTypeOf('function'))
      const manual = runTrigger(fixture.char, 'manual', {
        chat: fixture.chat,
        manualName: 'manualAction',
      })
      const last = runLuaButtonTrigger(fixture.char, 'last')
      const results = Promise.allSettled([first, manual, last])
      await new Promise<void>((resolve) => setImmediate(resolve))
      expect(fixture.chat.scriptstate.$counter).toBe('0')
      expect(fixture.chat.message).toHaveLength(1)
      expect(alertNormal).not.toHaveBeenCalled()
      if (outcome === 'navigate') {
        continuationRuntime.session = null
      } else if (outcome === 'external-edit') {
        fixture.session.edit(fixture.session.locate(0), {
          ...fixture.chat.message[0],
          data: 'external',
        })
      }
      confirm(true)
      const settled = await results
      if (outcome === 'navigate') {
        expect(settled.map((result) => result.status)).toEqual([
          'rejected',
          'rejected',
          'rejected',
        ])
        expect(fixture.chat.scriptstate.$counter).toBe('0')
        expect(fixture.chat.message.map((entry) => entry.data)).toEqual([
          'message',
        ])
        expect(alertNormal).not.toHaveBeenCalled()
      } else {
        expect(settled.map((result) => result.status)).toEqual([
          outcome === 'complete' ? 'fulfilled' : 'rejected',
          'fulfilled',
          'fulfilled',
        ])
        expect(fixture.chat.scriptstate.$counter).toBe(
          outcome === 'complete' ? '3' : '2',
        )
        expect(alertNormal).toHaveBeenCalledTimes(2)
        expect(fixture.chat.message.map((entry) => entry.data)).toEqual(
          outcome === 'complete'
            ? ['message', 'first', 'manual', 'last']
            : ['external', 'manual', 'last'],
        )
      }
      expect(fixture.session.activePinReasons).toEqual([])
    } finally {
      confirm?.(true)
      continuationRuntime.session = null
      vi.mocked(alertConfirm).mockReset()
    }
  },
)

test('renders concurrent Lua display listeners that update chat variables', async () => {
  const fixture = operationCharacterFixture('concurrent-display-variables')
  fixture.chat.scriptstate = { $__renders: '0' }
  fixture.char.triggerscript = [
    {
      type: 'start',
      conditions: [],
      effect: [
        {
          type: 'triggerlua',
          code: `
        listenEdit('editDisplay', function(id, value)
          local renders = getState(id, 'renders') + 1
          setState(id, 'renders', renders)
          return value .. ':' .. renders
        end)
      `,
        },
      ],
    },
  ] as never
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  continuationRuntime.session = fixture.session
  const { runLuaEditTrigger } = await import('./scriptings')

  try {
    const results = await Promise.allSettled([
      runLuaEditTrigger(fixture.char, 'editdisplay', 'first'),
      runLuaEditTrigger(fixture.char, 'editdisplay', 'second'),
    ])
    expect(results).toEqual([
      { status: 'fulfilled', value: 'first:1' },
      { status: 'fulfilled', value: 'second:2' },
    ])
    expect(fixture.chat.scriptstate.$__renders).toBe('2')
  } finally {
    continuationRuntime.session = null
  }
})

test('does not run queued Lua display listeners after conversation navigation', async () => {
  const fixture = operationCharacterFixture('queued-display-original')
  const replacement = operationCharacterFixture('queued-display-replacement')
  fixture.char.triggerscript = [
    {
      type: 'start',
      conditions: [],
      effect: [
        {
          type: 'triggerlua',
          code: `
        listenEdit('editDisplay', function(id, value)
          setState(id, 'unexpected', true)
          return value .. ':changed'
        end)
      `,
        },
      ],
    },
  ] as never
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  continuationRuntime.session = fixture.session
  const { runLuaEditTrigger } = await import('./scriptings')

  try {
    const pending = runLuaEditTrigger(fixture.char, 'editdisplay', 'original')
    installOperationCharacterFixture(replacement)
    vi.mocked(getCurrentChat).mockReturnValue(replacement.chat)
    continuationRuntime.session = replacement.session

    await expect(pending).resolves.toBe('original')
    expect(fixture.chat.scriptstate).toBeUndefined()
    expect(replacement.chat.scriptstate).toBeUndefined()
    expect(fixture.session.activePinReasons).toEqual([])
    expect(replacement.session.activePinReasons).toEqual([])
  } finally {
    continuationRuntime.session = null
  }
})

test('preserves display text for a simple character without optional triggers', async () => {
  const { runLuaEditTrigger } = await import('./scriptings')
  await expect(
    runLuaEditTrigger(
      { type: 'simple', customscript: [], chaId: 'simple-no-triggers' },
      'editdisplay',
      'unchanged',
    ),
  ).resolves.toBe('unchanged')
})

test('keeps stored trigger permissions and parser identity stable during Lua display', async () => {
  const fixture = operationCharacterFixture('display-trigger-identity')
  fixture.char.lowLevelAccess = true
  fixture.char.customscript = []
  fixture.char.triggerscript = [
    {
      comment: 'synthetic display listener',
      type: 'start',
      conditions: [],
      lowLevelAccess: true,
      effect: [
        {
          type: 'triggerlua',
          code: `listenEdit('editDisplay', function(id, value) return value .. ':rendered' end)`,
        },
      ],
    },
  ]
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  continuationRuntime.session = fixture.session
  const { runLuaEditTrigger } = await import('./scriptings')
  const before = createChatParserDependencyStamp(fixture.char)
  const readRange = vi.spyOn(fixture.session, 'readRange')
  try {
    for (let index = 0; index < 3; index++) {
      await expect(
        runLuaEditTrigger(fixture.char, 'editdisplay', 'input'),
      ).resolves.toBe('input:rendered')
      expect(fixture.char.triggerscript[0].lowLevelAccess).toBe(true)
      expect(createChatParserDependencyStamp(fixture.char)).toBe(before)
    }
    expect(readRange).not.toHaveBeenCalled()
  } finally {
    continuationRuntime.session = null
  }
})

test('releases a read-only Lua edit operation without publishing an empty commit', async () => {
  const fixture = operationCharacterFixture('read-only-edit-operation')
  fixture.char.customscript = []
  fixture.char.triggerscript = [
    {
      type: 'output',
      comment: 'synthetic read-only listener',
      conditions: [],
      effect: [
        {
          type: 'triggerlua',
          code: `
      listenEdit('editOutput', function(id, value)
        getChatVar(id, 'missing')
        return value
      end)
    `,
        },
      ],
    },
  ]
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  continuationRuntime.session = fixture.session
  const applyOperation = vi.spyOn(fixture.session, 'applyOperation')
  const { runLuaEditTrigger } = await import('./scriptings')

  try {
    await expect(
      runLuaEditTrigger(fixture.char, 'editoutput', 'unchanged'),
    ).resolves.toBe('unchanged')
    expect(applyOperation).not.toHaveBeenCalled()
    expect(fixture.session.activePinReasons).toEqual([])
  } finally {
    continuationRuntime.session = null
  }
})

test('settles a resumed coroutine rejection without leaving an unhandled rejection', async () => {
  if (process.env.RISUNEST_A4_COROUTINE_CHILD !== 'true') {
    const child = spawn(process.execPath, [
      resolve(process.cwd(), 'node_modules/vitest/vitest.mjs'),
      'run',
      'src/ts/process/scriptings.test.ts',
      '--pool=threads',
      '--maxWorkers=1',
      '--no-file-parallelism',
      '-t',
      'settles a resumed coroutine rejection without leaving an unhandled rejection',
    ], {
      env: {
        ...process.env,
        RISUNEST_A4_COROUTINE_CHILD: 'true',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stderr = ''
    let stdout = ''
    child.stderr.setEncoding('utf8')
    child.stdout.setEncoding('utf8')
    child.stderr.on('data', (chunk) => {
      stderr += chunk
    })
    child.stdout.on('data', (chunk) => {
      stdout += chunk
    })
    const childResult = await new Promise<{ code: number | null; timedOut: boolean }>(
      (resolveChild) => {
        let timedOut = false
        const timeout = setTimeout(() => {
          timedOut = true
          child.kill()
        }, 8_000)
        child.once('exit', (code) => {
          clearTimeout(timeout)
          resolveChild({ code, timedOut })
        })
      },
    )

    expect(childResult, `${stdout}\n${stderr}`).toEqual({ code: 0, timedOut: false })
    return
  }

  const unhandledRejections: unknown[] = []
  const onUnhandledRejection = (reason: unknown) => {
    unhandledRejections.push(reason)
  }
  process.on('unhandledRejection', onUnhandledRejection)
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockResolvedValueOnce({
    type: 'success',
    result: 'resume into loop',
  } as never)

  try {
    const scriptingPromise = runScripted(`
      coroutine_rejection_boundary = async(function(id)
        LLM(id, {{ role = 'user', content = 'reject' }})
        while true do end
      end)
    `, {
      char: { chaId: 'coroutine-rejection-boundary' } as never,
      chat: { message: [] } as never,
      lowLevelAccess: true,
      mode: 'coroutine_rejection_boundary',
    })
    const settled = await scriptingPromise.then(() => true, () => true)
    await new Promise<void>((resolveImmediate) => setImmediate(resolveImmediate))

    expect(settled).toBe(true)
    expect(unhandledRejections).toEqual([])
  } finally {
    process.off('unhandledRejection', onUnhandledRejection)
    vi.mocked(requestChatData).mockReset()
  }
}, 12_000)

test.each(['editinput', 'editoutput'] as const)(
  'continues after real Lua %s full-chat and variable edits',
  async (mode) => {
    const fixture = operationCharacterFixture(`continuation-${mode}`)
    fixture.session.edit(fixture.session.locate(0), {
      ...fixture.chat.message[0],
      ...{ __translation: { value: 'retained' } },
    })
    fixture.char.customscript = []
    fixture.char.triggerscript = [
      {
        type: 'output',
        comment: 'synthetic',
        conditions: [],
        effect: [
          {
            type: 'triggerlua',
            code: `
      listenEdit('${mode === 'editinput' ? 'editInput' : 'editOutput'}', function(id, value, meta)
        local messages = getFullChat(id)
        messages[1].data = 'rewritten'
        table.insert(messages, 1, {role = 'user', data = 'inserted'})
        setFullChat(id, messages)
        setChatVar(id, 'state', 'updated')
        return value .. ' processed'
      end)
    `,
          },
        ],
      },
    ]
    installOperationCharacterFixture(fixture)
    vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
    vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
    continuationRuntime.session = fixture.session
    const { runLuaEditTrigger } = await import('./scriptings')
    try {
      if (mode === 'editinput') {
        const target = captureConversationMutationTarget(
          fixture.char,
          fixture.chat,
          fixture.session,
        )
        await expect(
          appendDefaultChatInput({
            target,
            runInputTrigger: async () => null,
            processInput: (onCommitted) =>
              runLuaEditTrigger(fixture.char, mode, 'input', {}, undefined, onCommitted),
            isTargetCurrent: (current) =>
              isConversationMutationTargetCurrent(
                current,
                fixture.char,
                fixture.chat,
                fixture.session,
              ),
            createMessage: (data) => ({ role: 'user', data }),
          }),
        ).resolves.toBe(true)
        expect(fixture.chat.message.map((message) => message.data)).toEqual([
          'inserted',
          'rewritten',
          'input processed',
        ])
      } else {
        const output = captureGenerationConversationOperation({
          session: fixture.session,
          getCurrentSession: () => fixture.session,
          chat: fixture.chat,
          getCurrentChat: () => fixture.chat,
          continueLast: true,
        })
        await runLuaEditTrigger(fixture.char, mode, 'output', {}, undefined, (commit) => {
          output.acceptCommit(commit)
        })
        expect(output.snapshot()?.data).toBe('rewritten')
        expect(output.absoluteIndex).toBe(1)
        output.release()
      }
      expect(fixture.chat.scriptstate).toEqual({ $state: 'updated' })
      expect(fixture.chat.message[1].chatId).toBe(`message-continuation-${mode}`)
      expect(fixture.chat.message[1]).toMatchObject({ __translation: { value: 'retained' } })
      expect(fixture.session.activePinReasons).toEqual([])
    } finally {
      continuationRuntime.session = null
    }
  },
)
