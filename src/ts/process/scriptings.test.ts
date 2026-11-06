// @vitest-environment node

import { spawn } from 'node:child_process'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { beforeAll, expect, inject, test, vi } from 'vitest'

// Factory inspection and injected clocks apply only to the main-thread runtime.
const localRuntimeTest = test.skipIf(inject('luaWorker') === true)
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { requestChatData } from './request/request'
import { fetchNative, readImage } from '../globalApi.svelte'
import { HypaProcesser } from './memory/hypamemory'
import { getCurrentCharacter, getCurrentChat, getDatabase } from '../storage/database.svelte'
import { asBuffer, getPersonaPrompt, getUserIcon } from '../util'
import { tokenize } from '../tokenizer'
import { loadLoreBookV3PromptFromCompatibilitySnapshot } from './lorebook.svelte'
import { writeInlayImage } from './files/inlays'
import { generateAIImage } from './stableDiff'
import {
  ActiveConversationSession,
  ConversationSessionStaleError,
} from '../storage/activeConversationSession'
import { createConversationOperationContext } from './conversationOperationContext'
import type { Chat, character } from '../storage/database.svelte'
import { DBState, ScriptDisplayRefreshPointer } from '../stores.svelte'
import { risuChatParser } from '../parser/parser.svelte'
import type { LuaEngine } from 'wasmoon'
import { appendDefaultChatInput } from '../../lib/ChatScreens/defaultChatInput'
import {
  captureConversationMutationTarget,
  isConversationMutationTargetCurrent,
} from '../conversationMutations'
import { captureGenerationConversationOperation } from './generationConversationOperation'
import { createChatParserDependencyStamp } from '../chatRenderIdentity'
import { alertConfirm, alertNormal, alertToast } from '../alert'
import { language } from '../../lang'
import { DisplayReadRecord, recordAmbientChatVariableReads, registerDisplayRow } from './scriptVariableReads'
import { pluginV2 } from '../plugins/plugins.svelte'
import { forgetDisplayRecovery, installDisplayRecovery } from '../displayRecovery.svelte'
import { captureSelectedConversationAuthority, getActiveConversationViewportSource } from '../storage/persistentDataRuntime.svelte'
import { createMetadataOnlySelectedConversation } from '../storage/selectedConversationLifecycle'
import { recoveryStore } from '../displayRecovery.testSupport'

const continuationRuntime = vi.hoisted(() => ({
  session: null as ActiveConversationSession | null,
}))
vi.mock('../storage/persistentDataRuntime.svelte', async () => (await import('./tests/scriptingsTestHarness')).persistentRuntimeModule(() => continuationRuntime.session))

const scriptingSelectionState = vi.hoisted(() => ({ index: 0 }))
const createdLuaEngines = vi.hoisted(() => [] as LuaEngine[])
const createdLuaEngineOptions = vi.hoisted(() => [] as unknown[])
const diagnosticEngineSetup = vi.hoisted(() => ({ next: null as ((engine: LuaEngine) => void) | null }))
// Replaces the product's function deadline, whose value the engine option tests check, so a run that waits it out is short.
const engineFunctionTimeout = vi.hoisted(() => ({ ms: null as number | null }))

vi.mock('../parser/parser.svelte', async () => (await import('./tests/scriptingsTestHarness')).parserModule())

vi.mock('../alert', async () => (await import('./tests/scriptingsTestHarness')).alertModule())

vi.mock('../globalApi.svelte', () => ({ fetchNative: vi.fn(), readImage: vi.fn() }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'android' }))
vi.mock('../tokenizer', async () => (await import('./tests/scriptingsTestHarness')).tokenizerModule())
vi.mock('../util', async () => (await import('./tests/scriptingsTestHarness')).utilModule())

vi.mock('../storage/database.svelte', async () => (await import('./tests/scriptingsTestHarness')).databaseModule())

vi.mock('../stores.svelte', async () => (await import('./tests/scriptingsTestHarness')).storesModule(() => scriptingSelectionState.index))

vi.mock('./modules', async () => (await import('./tests/scriptingsTestHarness')).modulesModule())
vi.mock('../plugins/plugins.svelte', () => ({ pluginV2: {
  editdisplay: new Set(), editinput: new Set(), editoutput: new Set(), editprocess: new Set(),
} }))

vi.mock('./files/inlays', async () => (await import('./tests/scriptingsTestHarness')).inlaysModule())
vi.mock('./lorebook.svelte', async () => (await import('./tests/scriptingsTestHarness')).lorebookModule())
vi.mock('./memory/hypamemory', async () => (await import('./tests/scriptingsTestHarness')).hypamemoryModule())
vi.mock('./request/request', async () => (await import('./tests/scriptingsTestHarness')).requestModule())
vi.mock('./stableDiff', async () => (await import('./tests/scriptingsTestHarness')).stableDiffModule())
vi.mock('./command', async () => (await import('./tests/scriptingsTestHarness')).commandModule())
vi.mock('./luaRuntime', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./luaRuntime')>()
  return {
    ...actual,
    async createLuaFactory() {
      const factory = await actual.createLuaFactory()
      const createEngine = factory.createEngine.bind(factory)
      factory.createEngine = async (options) => {
        createdLuaEngineOptions.push(options)
        const engine = await createEngine(engineFunctionTimeout.ms === null ? options : { ...options, functionTimeout: engineFunctionTimeout.ms })
        createdLuaEngines.push(engine)
        const setup = diagnosticEngineSetup.next
        diagnosticEngineSetup.next = null
        setup?.(engine)
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
  messages: Chat['message'] = [{ role: 'user', data: 'message', chatId: `message-${suffix}` }],
) {
  const chat = {
    id: `lua-metadata-chat-${suffix}`,
    message: messages,
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
  const originalWorker = globalThis.Worker
  vi.stubGlobal('Worker', worker)

  try {
  await expect(runScripted('print("blocked")', {
    char: {} as never,
    chat: { message: [] } as never,
    mode: 'tauri-mobile-python-gate',
    type: 'py',
  })).rejects.toThrow(/Python scripting is unavailable on Tauri mobile/)

  expect(worker).not.toHaveBeenCalled()
  } finally {
    vi.stubGlobal('Worker', originalWorker)
  }
})

localRuntimeTest('captures field ownership before the Lua factory wait and rejects navigation', async () => {
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

localRuntimeTest('evicts the least recently used idle Lua engine after 17 modes', async () => {
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

localRuntimeTest('trims idle Lua engines when switching to the lower low-spec budget', async () => {
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

localRuntimeTest('keeps active Lua engines open during a profile switch and evicts after completion', async () => {
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

localRuntimeTest('releases cached invocation state while preserving Lua globals', async () => {
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
  expect(() => getFullChat('inactive')).toThrow(/is not a function/)
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
  expect(() => getFullChat('inactive')).toThrow(/is not a function/)

  const second = await runScripted(code, {
    char: { chaId: fixture.char.chaId } as never,
    chat: { message: [] } as never,
    data: 'cancel',
    mode: 'editInput',
  })

  expect(second.res).toBe(3)
  expect(second.stopSending).toBe(true)
  expect(createdLuaEngines.at(-1)).toBe(engine)
  expect(() => getFullChat('inactive')).toThrow(/is not a function/)
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

test.each(['editRequest', 'editInput', 'editOutput'] as const)(
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

test('passes editDisplay strings through listeners and keeps the previous string for a non-string return', async () => {
  const result = await runScripted(`
    local printed = {}
    print = function(message) printed[#printed + 1] = message end
    listenEdit('editDisplay', function(id, value, meta)
      return value .. ' first'
    end)
    listenEdit('editDisplay', function(id, value, meta)
      return nil
    end)
    listenEdit('editDisplay', function(id, value, meta)
      return value .. ' third | ' .. table.concat(printed, '; ')
    end)
  `, {
    char: { chaId: 'edit-display-raw-string' } as never,
    chat: { message: [] } as never,
    data: 'display',
    mode: 'editDisplay',
  })

  expect(result).toMatchObject({
    res: 'display first third | Error: Lua editDisplay must return a string, received nil',
    stopSending: false,
  })
})

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
        { role: 'char', data: 'inserted', chatId: expect.any(String) },
        { role: 'user', data: 'keep-me' },
        { role: 'user', data: 'tail', chatId: expect.any(String) },
      ],
    },
    res: false,
    stopSending: true,
  })
})

test('gives Lua-created messages IDs that keep plugin fields through a later full-chat round trip', async () => {
  const created = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      addChat(id, 'user', 'added')
      insertChat(id, 0, 'char', 'inserted')
      local messages = getFullChat(id)
      table.insert(messages, {role = 'char', data = 'new entry'})
      table.insert(messages, messages[2])
      setFullChat(id, messages)
      return value
    end)
  `, {
    char: { chaId: 'lua-created-ids' } as never,
    chat: { message: [{ role: 'user', data: 'original', chatId: 'original' }] } as never,
    mode: 'editInput',
  }) as { chat: Chat }

  const messages = created.chat.message
  expect(messages.map((message) => message.data)).toEqual([
    'inserted', 'original', 'added', 'new entry', 'original',
  ])
  const ids = messages.map((message) => message.chatId)
  expect(ids.every((id) => typeof id === 'string' && id.length > 0)).toBe(true)
  expect(new Set(ids).size).toBe(ids.length)
  // The stored message keeps its id; the copy of it becomes a new message.
  expect(ids[1]).toBe('original')

  const written = {
    message: messages.map((message, index) => ({ ...message, __plugin: `record-${index}` })),
  }
  const roundTrip = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      setFullChat(id, getFullChat(id))
      return value
    end)
  `, {
    char: { chaId: 'lua-created-ids' } as never,
    chat: written as never,
    mode: 'editInput',
  }) as { chat: Chat }

  expect(roundTrip.chat.message.map((message) => message.chatId)).toEqual(ids)
  expect(roundTrip.chat.message.map((message) => (message as unknown as Record<string, unknown>).__plugin)).toEqual([
    'record-0', 'record-1', 'record-2', 'record-3', 'record-4',
  ])
})

test('keeps repeated stored IDs through a full-chat round trip and drops only their plugin fields', async () => {
  const result = await runScripted(`
    listenEdit('editInput', function(id, value, meta)
      setFullChat(id, getFullChat(id))
      return value
    end)
  `, {
    char: { chaId: 'lua-repeated-ids' } as never,
    chat: {
      message: [
        { role: 'user', data: 'first', chatId: 'repeated', __plugin: 'a' },
        { role: 'char', data: 'second', chatId: 'repeated', __plugin: 'b' },
        { role: 'user', data: 'third', chatId: 'single', __plugin: 'c' },
      ],
    } as never,
    mode: 'editInput',
  }) as { chat: Chat }

  expect(result.chat.message.map((message) => message.chatId)).toEqual(['repeated', 'repeated', 'single'])
  expect(result.chat.message.map((message) => (message as unknown as Record<string, unknown>).__plugin)).toEqual([
    undefined, undefined, 'c',
  ])
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
  // The global selection points at the replacement, so reading it instead of the captured character fails this test.
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

test('Lua reads the selected character without visiting the rest of the library', async () => {
  const visited: number[] = []
  const library = ['first', 'second', 'selected'].map((name) => ({
    type: 'character',
    chaId: `lua-library-${name}`,
    name,
    chatPage: 0,
    chats: [{ id: `lua-library-chat-${name}`, message: [], scriptstate: {} }],
    alternateGreetings: [],
    defaultVariables: '',
  }))
  const characters = new Proxy(library, {
    get(target, property, receiver) {
      if (typeof property === 'string' && /^\d+$/.test(property)) visited.push(Number(property))
      return Reflect.get(target, property, receiver)
    },
  })
  const database = { characters, templateDefaultVariables: '' }
  scriptingSelectionState.index = 2
  DBState.db = database as never
  vi.mocked(getDatabase).mockReturnValue(database as never)
  try {
    const result = await runScripted(`
      function phase1_selected_lookup(id)
        return getName(id)
      end
    `, {
      char: library[2] as never,
      chat: library[2].chats[0] as never,
      mode: 'phase1_selected_lookup',
    })

    expect(result.res).toBe('selected')
    expect(new Set(visited)).toEqual(new Set([2]))
  } finally {
    scriptingSelectionState.index = 0
  }
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

test.each([
  { option: "{ mode = 'memory' }", requested: 'memory' },
  { option: '{}', requested: 'otherAx' },
  { option: "{ mode = 'model' }", requested: null },
])('runs axLLM with the $option mode', async ({ option, requested }) => {
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockResolvedValueOnce({ type: 'success', result: 'synthetic response' } as never)
  const result = await runScripted(`onStart = async(function(id)
      return axLLM(id, {{ role = 'user', content = 'prompt' }}, false, ${option})
    end)`, {
    char: { chaId: `lua-axllm-mode-${requested}` } as never,
    chat: { message: [] } as never,
    lowLevelAccess: true,
    mode: 'start',
  })

  if (requested) {
    expect(result.res).toEqual({ success: true, result: 'synthetic response' })
    expect(vi.mocked(requestChatData).mock.calls.map(([, mode]) => mode)).toEqual([requested])
  } else {
    expect(result.res).toEqual({ success: false, result: 'Error: Invalid axLLM mode: model' })
    expect(requestChatData).not.toHaveBeenCalled()
  }
})

test.each([
  { options: "{ orientation = 'portrait' }", sent: { orientation: 'portrait' } },
  { options: "{ orientation = 'square' }", sent: null },
])('generates a Lua image with $options', async ({ options, sent }) => {
  vi.mocked(generateAIImage).mockReset()
  vi.mocked(generateAIImage).mockResolvedValue(false)
  const operationCharacter = { chaId: `lua-image-orientation-${sent ? 'valid' : 'invalid'}` }
  const result = await runScripted(`onStart = async(function(id)
      return generateImage(id, 'prompt', 'negative', ${options}):await()
    end)`, {
    char: operationCharacter as never,
    chat: { message: [] } as never,
    lowLevelAccess: true,
    mode: 'start',
  })

  if (sent) {
    expect(result.res).toBe('Error: Image generation failed')
    expect(vi.mocked(generateAIImage).mock.calls).toEqual([['prompt', operationCharacter, 'negative', 'inlay', sent]])
  } else {
    expect(result.res).toBe('Error: Image orientation must be landscape or portrait, received square')
    expect(generateAIImage).not.toHaveBeenCalled()
  }
})

test('passes API results to Lua as plain tables without JS constructors', async () => {
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockResolvedValueOnce({ type: 'success', result: 'synthetic response' } as never)
  const result = await runScripted(`onStart = async(function(id)
      local response = simpleLLM(id, 'prompt'):await()
      return { type = type(response), hasConstructor = response.constructor ~= nil }
    end)`, {
    char: { chaId: 'lua-api-result-boundary' } as never,
    chat: { message: [] } as never,
    lowLevelAccess: true,
    mode: 'start',
  })

  expect(result.res).toEqual({ type: 'table', hasConstructor: false })
})

test('passes host errors to Lua without JS constructors', async () => {
  const result = await runScripted(`
    function onInput(id)
      local ok, failure = pcall(getChatVar, id, 'synthetic')
      return { ok = ok, constructorHidden = failure.constructor == null }
    end
  `, {
    char: { chaId: 'lua-host-error-boundary' } as never,
    chat: { message: [] } as never,
    getVar: () => { throw new Error('synthetic host failure') },
    mode: 'input',
  })

  expect(result.res).toEqual({ ok: false, constructorHidden: true })
})

localRuntimeTest.each([
  ['listener', 5_000],
  ['button', 60_000],
  ['form', 60_000],
  ['manual trigger', 60_000],
] as const)('creates production Lua engines with the %s deadline', async (entry, functionTimeout) => {
  const char = {
    type: 'character',
    chaId: `bounded-${entry.replace(' ', '-')}-options`,
    chatPage: 0,
    chats: [{ message: [] }],
    triggerscript: [{
      comment: 'manualAction',
      type: 'manual',
      conditions: [],
      effect: [{ type: 'triggerlua', code: `-- ${entry} deadline` }],
    }],
    defaultVariables: '',
  } as unknown as character
  DBState.db = { characters: [char], templateDefaultVariables: '' } as never
  vi.mocked(getDatabase).mockReturnValue(DBState.db as never)
  if (entry === 'listener') {
    await runScripted(`-- ${entry} deadline`, { char, chat: { message: [] } as never, mode: 'output' })
  } else if (entry === 'button' || entry === 'form') {
    const { runLuaInteractionTrigger } = await import('./scriptings')
    await runLuaInteractionTrigger(entry, char, 'click')
  } else {
    const { runTrigger } = await import('./triggers')
    await runTrigger(char, 'manual', { chat: char.chats[0], manualName: 'manualAction' })
  }

  expect(createdLuaEngineOptions.at(-1)).toEqual({ injectObjects: true, enableProxy: false, functionTimeout })
})

localRuntimeTest.each([
  { kind: 'direct', invocation: 'return work()', asynchronous: false, ordinary: false },
  { kind: 'wrapped', invocation: 'return coroutine.wrap(work)()', asynchronous: false, ordinary: false },
  { kind: 'rethrown', invocation: 'local ok, failure = pcall(work); if not ok then error(failure) end', asynchronous: false, ordinary: false },
  { kind: 'wrapped rethrow', invocation: 'return coroutine.wrap(function() local ok, failure = pcall(work); if not ok then error(failure) end end)()', asynchronous: false, ordinary: false },
  { kind: 'async wrapped', invocation: 'return coroutine.wrap(work)()', asynchronous: true, ordinary: false },
  { kind: 'async rethrown', invocation: 'local ok, failure = pcall(work); if not ok then error(failure) end', asynchronous: true, ordinary: false },
  { kind: 'ordinary', invocation: 'return work()', asynchronous: false, ordinary: true },
  { kind: 'ordinary timeout suffix', invocation: 'return work()', asynchronous: false, ordinary: true },
])('distinguishes the actual Wasmoon instruction-hook timeout from ordinary errors ($kind)', async ({ kind, invocation, asynchronous, ordinary }) => {
  let now = 0
  const failure = new Error(kind === 'ordinary timeout suffix'
    ? 'ordinary failure mentioning thread timeout exceeded'
    : 'synthetic ordinary listener failure')
  diagnosticEngineSetup.next = (engine) => {
    engine.global.set('diagnosticFailure', () => {
      if (ordinary) throw failure
      now += 5_001
    })
  }
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  vi.mocked(alertToast).mockClear()
  const deadlineClock = vi.spyOn(performance, 'now').mockImplementation(() => now)
  try {
    const callback = asynchronous ? `onOutput = async(function() ${invocation} end)` : `function onOutput() ${invocation} end`
    await runScripted(`local function work()
      diagnosticFailure()
      local total = 0
      for i = 1, 10000 do total = total + i end
      return total
    end
    ${callback}`, {
      char: { chaId: `listener-diagnostic-${kind}` } as never,
      chat: { message: [] } as never,
      mode: 'output',
    })
    if (!ordinary) {
      expect(alertToast).toHaveBeenCalledExactlyOnceWith(language.risuNest.scriptTimeout)
      expect(errors.mock.calls).toEqual([[{ code: 'lua-listener-timeout', mode: 'output', elapsedMs: 5_001 }]])
      expect(Number.isFinite(errors.mock.calls[0][0].elapsedMs)).toBe(true)
      expect(errors.mock.calls[0][0].elapsedMs).toBeGreaterThanOrEqual(0)
    } else {
      expect(alertToast).not.toHaveBeenCalled()
      expect(errors).toHaveBeenCalledOnce()
      expect(errors.mock.calls[0]).toHaveLength(1)
      expect(errors.mock.calls[0][0]).toBeInstanceOf(Error)
      expect(errors.mock.calls[0][0].message.split('\n', 1)[0]).toBe(failure.message)
      expect(errors.mock.calls[0][0]).not.toHaveProperty('code')
    }
    expect(createdLuaEngineOptions.at(-1)).toEqual({ injectObjects: true, enableProxy: false, functionTimeout: 5_000 })
  } finally {
    diagnosticEngineSetup.next = null
    deadlineClock.mockRestore()
    errors.mockRestore()
  }
})

test('shows one Lua timeout notice for queued display pipelines and another for a later refresh', async () => {
  const fixture = operationCharacterFixture('display-timeout-notice')
  fixture.char.customscript = []
  let now = 0
  const worker = inject('luaWorker') === true
  diagnosticEngineSetup.next = (engine) => {
    engine.global.set('slowDisplay', () => {
      now += 5_001
    })
  }
  fixture.char.triggerscript = luaTriggerScript(`
    count = 0
    listenEdit('editDisplay', function(id, value)
      count = count + 1
      if value == 'fast' then return value .. count end
      ${worker ? 'while true do end' : 'slowDisplay()'}
      local total = 0
      for i = 1, 10000 do total = total + i end
      return value .. total
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const previousLocalStorage = globalThis.localStorage
  vi.stubGlobal('localStorage', { getItem: () => null })
  vi.mocked(risuChatParser).mockImplementation(value => value)
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const deadlineClock = worker ? undefined : vi.spyOn(performance, 'now').mockImplementation(() => now)
  vi.mocked(alertToast).mockClear()
  try {
    const { processScriptFull } = await import('./scripts')
    await expect(Promise.all([
      processScriptFull(fixture.char, 'first', 'editdisplay', 0),
      processScriptFull(fixture.char, 'second', 'editdisplay', 1),
    ])).resolves.toMatchObject([{ data: 'first' }, { data: 'second' }])
    // The second row was already waiting when the listener timed out, so it skips it.
    expect(errors.mock.calls.filter(([error]) => error?.code === 'lua-listener-timeout')).toHaveLength(1)
    expect(alertToast).toHaveBeenCalledExactlyOnceWith(language.risuNest.scriptTimeout)

    expect((await processScriptFull(fixture.char, 'fast', 'editdisplay', 0)).data).toBe('fast2')

    await processScriptFull(fixture.char, 'later', 'editdisplay', 0)
    expect(alertToast).toHaveBeenCalledTimes(2)
  } finally {
    diagnosticEngineSetup.next = null
    deadlineClock?.mockRestore()
    errors.mockRestore()
    continuationRuntime.session = null
    vi.mocked(risuChatParser).mockReset()
    vi.stubGlobal('localStorage', previousLocalStorage)
  }
})

localRuntimeTest('runs a timed-out display listener again only for a new row, message, input, conversation or script', async () => {
  const fixture = operationCharacterFixture('display-timeout-rows', undefined, [
    { role: 'char', data: 'zero', chatId: 'message-zero' },
    { role: 'char', data: 'one', chatId: 'message-one' },
  ])
  fixture.char.customscript = []
  let now = 0
  let listenerRuns = 0
  const setupEngine = (engine: LuaEngine) => {
    engine.global.set('noteRun', (value: string) => {
      listenerRuns++
      if (value.startsWith('slow')) now += 5_001
    })
  }
  diagnosticEngineSetup.next = setupEngine
  const listener = (marker: string) => luaTriggerScript(`
    -- ${marker}
    listenEdit('editDisplay', function(id, value)
      noteRun(value)
      local total = 0
      for i = 1, 10000 do total = total + i end
      return value .. '!'
    end)
  `)
  fixture.char.triggerscript = listener('original')
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const previousLocalStorage = globalThis.localStorage
  vi.stubGlobal('localStorage', { getItem: () => null })
  vi.mocked(risuChatParser).mockImplementation(value => value)
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const deadlineClock = vi.spyOn(performance, 'now').mockImplementation(() => now)
  const { processScriptFull } = await import('./scripts')
  const render = async (data: string, index: number) => (await processScriptFull(fixture.char, data, 'editdisplay', index)).data
  const timeouts = () => errors.mock.calls.filter(([error]) => error?.code === 'lua-listener-timeout').length
  try {
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect([listenerRuns, timeouts()]).toEqual([1, 1])

    // The same row and input skip it; a fast row still runs it.
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect(await render('one', 1)).toBe('one!')
    expect([listenerRuns, timeouts()]).toEqual([2, 1])

    // Changed input runs it again.
    expect(await render('slow zero again', 0)).toBe('slow zero again')
    expect([listenerRuns, timeouts()]).toEqual([3, 2])

    // A different message at the same row runs it again.
    fixture.chat.message[0] = { role: 'char', data: 'zero', chatId: 'message-replacement' }
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect([listenerRuns, timeouts()]).toEqual([4, 3])
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect(listenerRuns).toBe(4)

    // The same row of another conversation runs it again.
    vi.mocked(getCurrentChat).mockReturnValue({ ...fixture.chat, id: 'display-timeout-other-chat' })
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect([listenerRuns, timeouts()]).toEqual([5, 4])

    // A conversation that keeps part of its history loaded has no active session and names its rows through the viewport.
    const windowed = createMetadataOnlySelectedConversation({ ...fixture.chat, id: 'display-timeout-windowed' }) as Chat
    continuationRuntime.session = null
    let windowedMessageId = 'windowed-message'
    vi.mocked(captureSelectedConversationAuthority).mockReturnValue({ conversationId: windowed.id } as never)
    vi.mocked(getActiveConversationViewportSource).mockReturnValue({
      snapshot: () => ({ rowAt: (index: number) => index === 0 ? { message: { chatId: windowedMessageId } } : undefined }),
    } as never)
    vi.mocked(getCurrentChat).mockReturnValue(windowed)
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect([listenerRuns, timeouts()]).toEqual([6, 5])
    windowedMessageId = 'windowed-replacement'
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect([listenerRuns, timeouts()]).toEqual([7, 6])
    vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
    continuationRuntime.session = fixture.session

    // An edited script is a new engine without the earlier failures.
    fixture.char.triggerscript = listener('edited')
    diagnosticEngineSetup.next = setupEngine
    expect(await render('slow zero', 0)).toBe('slow zero')
    expect([listenerRuns, timeouts()]).toEqual([8, 7])
  } finally {
    vi.mocked(captureSelectedConversationAuthority).mockReturnValue(null)
    vi.mocked(getActiveConversationViewportSource).mockReturnValue(null)
    diagnosticEngineSetup.next = null
    deadlineClock.mockRestore()
    errors.mockRestore()
    continuationRuntime.session = null
    vi.mocked(risuChatParser).mockReset()
    vi.stubGlobal('localStorage', previousLocalStorage)
  }
})

test.each(['runtime', 'pipeline'] as const)('spends one source budget on the display rows queued behind top-level code that timed out (%s)', async boundary => {
  const worker = inject('luaWorker') === true
  const fixture = operationCharacterFixture(`display-timeout-source-${boundary}`, undefined, [
    { role: 'char', data: 'zero', chatId: 'message-zero' },
    { role: 'char', data: 'one', chatId: 'message-one' },
  ])
  fixture.char.customscript = []
  const code = `
    ${worker ? 'while true do end' : 'noteSource()'}
    local total = 0
    for i = 1, 10000 do total = total + i end
    listenEdit('editDisplay', function(id, value) return value .. '!' end)
  `
  fixture.char.triggerscript = luaTriggerScript(code)
  let now = 0
  let sourceRuns = 0
  const setupEngine = (engine: LuaEngine) => {
    diagnosticEngineSetup.next = setupEngine
    engine.global.set('noteSource', () => {
      sourceRuns++
      now += 5_001
    })
  }
  diagnosticEngineSetup.next = setupEngine
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const previousLocalStorage = globalThis.localStorage
  vi.stubGlobal('localStorage', { getItem: () => null })
  vi.mocked(risuChatParser).mockImplementation(value => value)
  const deadlineClock = worker ? undefined : vi.spyOn(performance, 'now').mockImplementation(() => now)
  const { nextDisplayListenerOrder } = await import('./luaDisplayTimeouts')
  const { processScriptFull } = await import('./scripts')
  const render = async (data: string, index: number, displayRequestOrder?: number) => {
    if (boundary === 'pipeline') {
      return (await processScriptFull(fixture.char, data, 'editdisplay', index, {}, { displayRequestOrder })).data
    }
    return (await runScripted(code, { char: fixture.char, chat: fixture.chat, mode: 'editDisplay', data,
      displayListener: { order: displayRequestOrder ?? nextDisplayListenerOrder(), row: {
        conversationId: fixture.chat.id, index, messageId: fixture.chat.message[index].chatId!,
      } },
    })).res
  }
  try {
    // Both rows asked for their display before the source ran.
    const first = nextDisplayListenerOrder()
    const second = nextDisplayListenerOrder()
    if (boundary === 'pipeline') expect(await render('zero', 0, first)).toBe('zero')
    else await expect(render('zero', 0, first)).rejects.toThrow(/thread timeout exceeded/)
    expect(await render('one', 1, second)).toBe(boundary === 'pipeline' ? 'one' : undefined)
    // Rendering the same row again later skips it too.
    expect(await render('zero', 0)).toBe(boundary === 'pipeline' ? 'zero' : undefined)
    if (!worker) expect(sourceRuns).toBe(1)
  } finally {
    diagnosticEngineSetup.next = null
    deadlineClock?.mockRestore()
    continuationRuntime.session = null
    vi.mocked(risuChatParser).mockReset()
    vi.stubGlobal('localStorage', previousLocalStorage)
  }
})

test('opens a chat left in the display recovery record without its Modify Display steps but with its CBS', async () => {
  const fixture = operationCharacterFixture('display-recovery')
  fixture.char.customscript = [{ comment: '', type: 'editdisplay', in: 'raw', out: 'REGEX', flag: 'g', ableFlag: true }]
  fixture.char.triggerscript = [
    ...luaTriggerScript(`
      listenEdit('editDisplay', function(id, value)
        return value .. ' LUA'
      end)
    `),
    { comment: '', type: 'display', conditions: [], effect: [
      { type: 'v2Header', code: '', indent: 0 },
      { type: 'v2GetDisplayState', outputVar: 'shown', indent: 0 },
      { type: 'v2ConcatString', source1: 'shown', source1Type: 'var', source2: ' TRIGGER', source2Type: 'value', outputVar: 'shown', indent: 0 },
      { type: 'v2SetDisplayState', value: 'shown', valueType: 'var', indent: 0 },
    ] },
  ] as never
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const previousLocalStorage = globalThis.localStorage
  vi.stubGlobal('localStorage', { getItem: () => null })
  vi.mocked(risuChatParser).mockImplementation(value => value.replaceAll('{{cbs}}', 'CBS'))
  const pluginHook = async (value: string) => `${value} PLUGIN`
  pluginV2.editdisplay.add(pluginHook)
  await installDisplayRecovery(recoveryStore({
    working: [{ characterId: fixture.char.chaId, conversationId: fixture.chat.id }],
    marked: [],
  }, { commitAtOnce: true }).store)
  const { processScriptFull } = await import('./scripts')
  const render = async () => (await processScriptFull(fixture.char, 'raw {{cbs}}', 'editdisplay', 0)).data
  try {
    expect(await render()).toBe('raw CBS')

    await forgetDisplayRecovery(() => true)
    expect(await render()).toBe('REGEX CBS LUA TRIGGER PLUGIN')
  } finally {
    await installDisplayRecovery(null)
    pluginV2.editdisplay.delete(pluginHook)
    continuationRuntime.session = null
    vi.mocked(risuChatParser).mockReset()
    vi.stubGlobal('localStorage', previousLocalStorage)
  }
})

localRuntimeTest('resumes a Lua listener after an LLM wait longer than its CPU deadline', async () => {
  let now = 0
  const clock = vi.spyOn(performance, 'now').mockImplementation(() => now)
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockImplementationOnce(async () => {
    await Promise.resolve()
    now += 61_000
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

test('commits overlapping Lua button and form updates in click order', async () => {
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
      function onFormSubmit(id, name, meta)
        local values = json.decode(meta).data
        setChatVar(id, 'counter', tostring(tonumber(getChatVar(id, 'counter')) + 1))
        addChat(id, 'char', name .. ':' .. table.concat(values.choice, ',') .. ':' .. values.go)
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
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    const results = await Promise.allSettled([
      runLuaInteractionTrigger('button', fixture.char, 'first'),
      runLuaInteractionTrigger('form', fixture.char, 'survey', { data: { choice: ['a', 'b'], go: 'yes' } }),
      runLuaInteractionTrigger('button', fixture.char, 'third'),
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
      'survey:a,b:yes',
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
    const { runLuaInteractionTrigger } = await import('./scriptings')
    const { runTrigger } = await import('./triggers')
    try {
      const first = runLuaInteractionTrigger('button', fixture.char, 'first')
      await vi.waitFor(() => expect(confirm).toBeTypeOf('function'))
      const manual = runTrigger(fixture.char, 'manual', {
        chat: fixture.chat,
        manualName: 'manualAction',
      })
      const last = runLuaInteractionTrigger('button', fixture.char, 'last')
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
        // The first handler read no message before the external edit, so its append
        // applies to the edited conversation.
        expect(settled.map((result) => result.status)).toEqual([
          'fulfilled',
          'fulfilled',
          'fulfilled',
        ])
        expect(fixture.chat.scriptstate.$counter).toBe('3')
        expect(alertNormal).toHaveBeenCalledTimes(2)
        expect(fixture.chat.message.map((entry) => entry.data)).toEqual([
          outcome === 'complete' ? 'message' : 'external',
          'first',
          'manual',
          'last',
        ])
      }
      expect(fixture.session.activePinReasons).toEqual([])
    } finally {
      confirm?.(true)
      continuationRuntime.session = null
      vi.mocked(alertConfirm).mockReset()
    }
  },
)

function luaTriggerScript(code: string) {
  return [
    { type: 'start', conditions: [], effect: [{ type: 'triggerlua', code }] },
  ] as never
}

test('runs variable-only Lua handlers and display listeners without visiting messages', async () => {
  const fixture = operationCharacterFixture('variables-without-messages')
  fixture.chat.scriptstate = { $counter: '0' }
  fixture.char.triggerscript = luaTriggerScript(`
    function onButtonClick(id, data)
      setChatVar(id, 'counter', tostring(tonumber(getChatVar(id, 'counter')) + 1))
    end
    listenEdit('editDisplay', function(id, value)
      setChatVar(id, 'seen', getChatVar(id, 'counter'))
      return value .. ':' .. getChatVar(id, 'counter')
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  // The length is a constant-time read; any element read or iteration visits messages.
  let messagesReadable = true
  fixture.chat.message = new Proxy(fixture.chat.message, {
    get(target, property, receiver) {
      if (!messagesReadable && property !== 'length') {
        throw new Error(`message read: ${String(property)}`)
      }
      return Reflect.get(target, property, receiver)
    },
  })
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const { runLuaInteractionTrigger, runLuaEditTrigger } = await import('./scriptings')
  try {
    messagesReadable = false
    await runLuaInteractionTrigger('button', fixture.char, 'click')
    // The display pipeline records what its listener reads for the row it renders.
    const reads = new DisplayReadRecord()
    const stopRecording = recordAmbientChatVariableReads(reads)
    try {
      await expect(runLuaEditTrigger(fixture.char, 'editdisplay', 'row')).resolves.toBe('row:1')
    } finally {
      stopRecording()
    }
    expect([...reads.keys]).toEqual(['counter'])
    expect(errors).not.toHaveBeenCalled()
    expect(fixture.chat.scriptstate).toEqual({ $counter: '1', $seen: '1' })
    // One variable commit each for the handler and the listener.
    expect(fixture.session.version).toBe(2)
  } finally {
    messagesReadable = true
    errors.mockRestore()
    continuationRuntime.session = null
  }
})

test.each(['after', 'before'].flatMap(edit => ['content', 'metadata'].map(change => ({ edit, change }))))('rejects a Lua message edit made $edit the message $change changed and keeps its variables', async ({ edit, change }) => {
  const fixture = operationCharacterFixture(`stale-message-edit-${edit}-${change}`)
  if (change === 'metadata') fixture.chat.scriptstate = { $asked: 'yes' }
  const setChat = `setChat(id, 0, 'edited from ' .. before)`
  fixture.char.triggerscript = luaTriggerScript(`
    onButtonClick = async(function(id, data)
      local before = getChatMain(id, 0)
      setChatVar(id, 'asked', 'yes')
      ${edit === 'before' ? setChat : ''}
      alertConfirm(id, 'synthetic confirmation'):await()
      setChatVar(id, 'answered', 'yes')
      ${edit === 'after' ? setChat : ''}
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  let confirm!: (value: boolean) => void
  let askedWhileWaiting: unknown
  vi.mocked(alertConfirm).mockImplementationOnce(() => {
    askedWhileWaiting = fixture.chat.scriptstate?.$asked
    return new Promise<boolean>((resolve) => { confirm = resolve })
  })
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    const click = runLuaInteractionTrigger('button', fixture.char, 'click')
    await vi.waitFor(() => expect(confirm).toBeTypeOf('function'))
    expect(askedWhileWaiting).toBe('yes')
    if (change === 'metadata') {
      expect(fixture.session.adoptPersistedMetadata({
        ...fixture.chat,
        message: fixture.chat.message.map(message => ({ ...message, pluginMetadata: 'external' })),
      }, 15)).toBe(true)
    } else {
      fixture.session.edit(fixture.session.locate(0), {
        ...fixture.chat.message[0],
        data: 'external',
      })
    }
    confirm(true)

    await expect(click).rejects.toThrow(ConversationSessionStaleError)
    expect(fixture.chat.message[0]).toMatchObject(change === 'metadata'
      ? { data: 'message', pluginMetadata: 'external' }
      : { data: 'external' })
    expect(fixture.chat.scriptstate).toEqual({ $asked: 'yes', $answered: 'yes' })
    expect(fixture.session.activePinReasons).toEqual([])
  } finally {
    confirm?.(true)
    errors.mockRestore()
    continuationRuntime.session = null
    vi.mocked(alertConfirm).mockReset()
  }
})

test('shows Lua variables at reloadDisplay and commits message edits at the end', async () => {
  const fixture = operationCharacterFixture('reload-display-flush')
  fixture.chat.scriptstate = { $other: 'kept' }
  fixture.char.triggerscript = luaTriggerScript(`
    function onButtonClick(id, data)
      setChat(id, 0, 'edited')
      setChatVar(id, 'counter', '1')
      reloadDisplay(id)
      setChatVar(id, 'after', '2')
    end
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const atRefresh: unknown[] = []
  vi.mocked(ScriptDisplayRefreshPointer.update).mockImplementation(() => {
    atRefresh.push({
      messages: fixture.chat.message.map((entry) => entry.data),
      scriptstate: { ...fixture.chat.scriptstate },
    })
  })
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    const result = await runLuaInteractionTrigger('button', fixture.char, 'click')

    // The end commit refreshes for the variable written after reloadDisplay.
    expect(atRefresh).toEqual([
      { messages: ['message'], scriptstate: { $other: 'kept', $counter: '1' } },
      { messages: ['edited'], scriptstate: { $other: 'kept', $counter: '1', $after: '2' } },
    ])
    expect(result.chat).toBe(fixture.chat)
    expect(fixture.chat.message.map((entry) => entry.data)).toEqual(['edited'])
    expect(fixture.chat.scriptstate).toEqual({ $other: 'kept', $counter: '1', $after: '2' })
  } finally {
    vi.mocked(ScriptDisplayRefreshPointer.update).mockReset()
    continuationRuntime.session = null
  }
})

test.each([
  ['reloadDisplay(id)', [true, true]],
  ['reloadChat(id, 1)', [false, true]],
  ["reloadChat(id, '1')", [false, true]],
])('rebuilds the mounted rows that %s reaches', async (call, rebuilt) => {
  const fixture = operationCharacterFixture('reload-rebuild')
  fixture.char.triggerscript = luaTriggerScript(`
    function onButtonClick(id, data)
      ${call}
    end
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  // Rows whose display read no chat variable.
  const rows = [0, 1].map((index) => {
    const row = registerDisplayRow(() => index)
    row.track('body', new DisplayReadRecord())
    return row
  })
  const initial = rows.map((row) => row.rebuildRevision)
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    await runLuaInteractionTrigger('button', fixture.char, 'click')
    await Promise.resolve()

    expect(rows.map((row, index) => row.rebuildRevision !== initial[index])).toEqual(rebuilt)
  } finally {
    for (const row of rows) row.dispose()
    continuationRuntime.session = null
  }
})

test.each([
  ['LLM', `LLM(id, {{ role = 'user', content = 'wait' }})`],
  ['axLLM', `axLLM(id, {{ role = 'user', content = 'wait' }})`],
  ['simpleLLM', `simpleLLM(id, 'wait'):await()`],
  ['generateImage', `generateImage(id, 'prompt', 'negative'):await()`],
  ['request', `request(id, 'https://example.invalid/wait'):await()`],
  ['similarity', `similarity(id, 'source', { 'value' }):await()`],
  ['sleep', `sleep(id, 4321):await()`],
  ['loadLoreBooks', `loadLoreBooks(id)`],
])('commits Lua variables before %s waits and message edits at the end', async (api, call) => {
  const fixture = operationCharacterFixture(`wait-flush-${api}`)
  fixture.char.lowLevelAccess = true
  fixture.char.triggerscript = luaTriggerScript(`
    onButtonClick = async(function(id, data)
      setChat(id, 0, 'edited')
      setChatVar(id, 'before', '1')
      ${call}
      setChatVar(id, 'after', '2')
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const atWait: unknown[] = []
  const observeWait = () => atWait.push({
    messages: fixture.chat.message.map((entry) => entry.data),
    scriptstate: { ...fixture.chat.scriptstate },
  })
  vi.mocked(requestChatData).mockReset().mockImplementation(async () => {
    observeWait()
    return { type: 'success', result: 'done' } as never
  })
  vi.mocked(generateAIImage).mockReset().mockImplementation(async () => {
    observeWait()
    return false
  })
  vi.mocked(fetchNative).mockReset().mockImplementation(async () => {
    observeWait()
    return new Response('done')
  })
  vi.mocked(HypaProcesser).mockReset().mockImplementation(function () {
    return {
      addText: async () => { observeWait() },
      similaritySearch: async () => [],
    } as never
  })
  vi.mocked(loadLoreBookV3PromptFromCompatibilitySnapshot).mockReset().mockImplementation(async () => {
    observeWait()
    return { actives: [] } as never
  })
  const realSetTimeout = globalThis.setTimeout
  const timeout = vi.spyOn(globalThis, 'setTimeout').mockImplementation(((handler: () => void, delay?: number) => {
    if (delay !== 4321) return realSetTimeout(handler, delay)
    observeWait()
    return realSetTimeout(handler, 0)
  }) as typeof setTimeout)
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    await runLuaInteractionTrigger('button', fixture.char, 'click')

    expect(atWait).toEqual([{ messages: ['message'], scriptstate: { $before: '1' } }])
    expect(fixture.chat.message.map((entry) => entry.data)).toEqual(['edited'])
    expect(fixture.chat.scriptstate).toEqual({ $before: '1', $after: '2' })
  } finally {
    timeout.mockRestore()
    vi.mocked(requestChatData).mockReset()
    vi.mocked(generateAIImage).mockReset()
    vi.mocked(fetchNative).mockReset()
    vi.mocked(HypaProcesser).mockReset()
    vi.mocked(loadLoreBookV3PromptFromCompatibilitySnapshot).mockReset()
    continuationRuntime.session = null
  }
})

test('stops a Lua handler at the next wait once its conversation is no longer current', async () => {
  const fixture = operationCharacterFixture('wait-flush-stale')
  const other = operationCharacterFixture('wait-flush-other')
  fixture.char.lowLevelAccess = true
  fixture.char.triggerscript = luaTriggerScript(`
    onButtonClick = async(function(id, data)
      setChatVar(id, 'before', '1')
      LLM(id, {{ role = 'user', content = 'first' }})
      setChatVar(id, 'after', '2')
      LLM(id, {{ role = 'user', content = 'second' }})
      setChatVar(id, 'last', '3')
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  let resolveFirst!: (value: unknown) => void
  vi.mocked(requestChatData).mockReset()
    .mockReturnValueOnce(new Promise((resolve) => { resolveFirst = resolve }) as never)
    .mockResolvedValue({ type: 'success', result: 'second' } as never)
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    const settled = runLuaInteractionTrigger('button', fixture.char, 'click').then(
      () => null,
      (error: unknown) => error,
    )
    await vi.waitFor(() => expect(requestChatData).toHaveBeenCalledOnce())
    continuationRuntime.session = other.session
    resolveFirst({ type: 'success', result: 'first' })

    expect(await settled).toEqual(expect.objectContaining({ message: expect.stringMatching(/inactive/i) }))
    expect(requestChatData).toHaveBeenCalledOnce()
    expect(fixture.chat.scriptstate).toEqual({ $before: '1' })
    expect(other.chat.scriptstate).toBeUndefined()
  } finally {
    vi.mocked(requestChatData).mockReset()
    continuationRuntime.session = null
  }
})

localRuntimeTest('keeps Lua variables committed before a wait when the handler later swallows a timeout', async () => {
  const fixture = operationCharacterFixture('wait-flush-timeout')
  fixture.char.lowLevelAccess = true
  let now = 0
  diagnosticEngineSetup.next = (engine) => {
    engine.global.set('expire', () => { now = 1e12 })
  }
  fixture.char.triggerscript = luaTriggerScript(`
    onButtonClick = async(function(id, data)
      setChat(id, 0, 'edited')
      setChatVar(id, 'before', '1')
      sleep(id, 4321):await()
      setChatVar(id, 'after', '2')
      local tries = 0
      repeat
        tries = tries + 1
        setChatVar(id, 'tries', tostring(tries))
      until pcall(function() expire(); while true do end end) or tries >= 1e6
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const atWait: unknown[] = []
  const realSetTimeout = globalThis.setTimeout
  const timeout = vi.spyOn(globalThis, 'setTimeout').mockImplementation(((handler: () => void, delay?: number) => {
    if (delay !== 4321) return realSetTimeout(handler, delay)
    atWait.push({ messages: fixture.chat.message.map((entry) => entry.data), scriptstate: { ...fixture.chat.scriptstate } })
    return realSetTimeout(handler, 0)
  }) as typeof setTimeout)
  const deadlineClock = vi.spyOn(performance, 'now').mockImplementation(() => now)
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  vi.mocked(alertToast).mockClear()
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    await runLuaInteractionTrigger('button', fixture.char, 'click')

    expect(atWait).toEqual([{ messages: ['message'], scriptstate: { $before: '1' } }])
    expect(fixture.chat.scriptstate).toEqual({ $before: '1', $after: '2', $tries: '1' })
    expect(fixture.chat.message.map((entry) => entry.data)).toEqual(['edited'])
    expect(alertToast).toHaveBeenCalledExactlyOnceWith(language.risuNest.scriptTimeout)
  } finally {
    diagnosticEngineSetup.next = null
    deadlineClock.mockRestore()
    errors.mockRestore()
    timeout.mockRestore()
    continuationRuntime.session = null
  }
})

test.each([
  ['field edits', `
    setChat(id, 1, 'edited ' .. getChatData(id, 1))
    setChatRole(id, 3, 'char')
    setChatVar(id, 'read', getChatData(id, 1) .. '/' .. getChatRole(id, 3) .. '/' .. cbs('{{message 1}}'))
  `, ['user:zero', 'user:edited one', 'user:two', 'char:three'], 'edited one/char/edited one'],
  ['an edit and an append', `
    setChat(id, 3, 'edited ' .. getChatData(id, 3))
    addChat(id, 'char', 'tail')
    setChatVar(id, 'read', getChatData(id, -2) .. '/' .. getChatData(id, -1))
  `, ['user:zero', 'user:one', 'user:two', 'user:edited three', 'char:tail'], 'edited three/tail'],
] as const)('copies only the messages a Lua handler edits or appends (%s)', async (name, body, stored, read) => {
  // Every property read of the other messages throws once the handler starts.
  let locked = false
  const messages = ['zero', 'one', 'two', 'three'].map((data, index) => {
    const message = { role: 'user', data, chatId: `message-${index}` }
    if (index % 2 === 1) return message
    return new Proxy(message, {
      get(target, property, receiver) {
        if (locked) throw new Error(`read of message ${index}: ${String(property)}`)
        return Reflect.get(target, property, receiver)
      },
    })
  }) as Chat['message']
  const fixture = operationCharacterFixture(`edit-set-${name.replaceAll(' ', '-')}`, undefined, messages)
  fixture.char.triggerscript = luaTriggerScript(`
    function onButtonClick(id, data)
      ${body}
    end
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  // CBS reads the conversation from the database it is given.
  vi.mocked(risuChatParser).mockImplementation((_value, options) =>
    options!.db!.characters[0].chats[0].message[1].data)
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    locked = true
    await runLuaInteractionTrigger('button', fixture.char, 'click')
    locked = false
    expect(errors).not.toHaveBeenCalled()
    expect(fixture.chat.message.map((entry) => `${entry.role}:${entry.data}`)).toEqual(stored)
    expect(fixture.chat.scriptstate).toEqual({ $read: read })
    expect(fixture.session.activePinReasons).toEqual([])
  } finally {
    locked = false
    vi.mocked(risuChatParser).mockReset()
    errors.mockRestore()
    continuationRuntime.session = null
  }
})

test.each([
  ['field edits and appends', `
    setChat(id, 1, 'edited ' .. getChatData(id, 1))
    setChatRole(id, 1, 'user')
    addChat(id, 'char', 'tail')
    setChat(id, -1, getChatData(id, -1) .. ' after ' .. getChatData(id, 1))
    setChat(id, 9, 'ignored')
    setChat(id, -9, 'ignored')
    setChatVar(id, 'read', getChatData(id, -1))
  `, ['user:zero', 'user:edited one', 'user:two', 'char:tail after edited one']],
  ['a structural edit after field edits', `
    setChat(id, 0, 'edited')
    addChat(id, 'user', 'tail')
    insertChat(id, 1, 'char', 'inserted')
    setChat(id, -1, getChatData(id, -1) .. ' edited')
    setChatVar(id, 'read', getChatData(id, 1))
  `, ['user:edited', 'char:inserted', 'char:one', 'user:two', 'user:tail edited']],
  ['an unchanged value', `
    setChat(id, 0, getChatData(id, 0))
  `, ['user:zero', 'char:one', 'user:two']],
] as const)('stores what a whole conversation copy stores for Lua edits (%s)', async (name, body, stored) => {
  const code = `
    function mutate(id)
      ${body}
    end
    function onButtonClick(id, data)
      mutate(id)
    end
    listenEdit('editInput', function(id, value)
      mutate(id)
      return value
    end)
  `
  const slug = name.replaceAll(' ', '-')
  const messages = () => ['zero', 'one', 'two'].map((data, index) => ({
    role: index % 2 === 0 ? 'user' : 'char',
    data,
    chatId: `message-${index}`,
  })) as Chat['message']
  const normalize = (chat: Chat) => chat.message.map((entry) => ({
    role: entry.role,
    data: entry.data,
    id: entry.chatId?.startsWith('message-') ? entry.chatId : typeof entry.chatId,
  }))

  const copy = operationCharacterFixture(`whole-copy-${slug}`, undefined, messages())
  const operationContext = createConversationOperationContext(copy.session, copy.chat)
  await runScripted(code, { char: copy.char, mode: 'editInput', operationContext })
  operationContext.commit(copy.session)

  const fixture = operationCharacterFixture(`edit-set-copy-${slug}`, undefined, messages())
  fixture.char.triggerscript = luaTriggerScript(code)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  continuationRuntime.session = fixture.session
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    await runLuaInteractionTrigger('button', fixture.char, 'click')
  } finally {
    continuationRuntime.session = null
  }

  expect(copy.chat.message.map((entry) => `${entry.role}:${entry.data}`)).toEqual(stored)
  expect(normalize(fixture.chat)).toEqual(normalize(copy.chat))
  expect(fixture.chat.scriptstate).toEqual(copy.chat.scriptstate)
  expect(fixture.session.version > 0).toBe(copy.session.version > 0)
  expect(fixture.session.activePinReasons).toEqual([])
})

test('later trigger effects read the messages a Lua trigger edited', async () => {
  const fixture = operationCharacterFixture('trigger-after-lua', undefined, [
    { role: 'user', data: 'zero', chatId: 'message-0' },
    { role: 'char', data: 'one', chatId: 'message-1' },
  ])
  fixture.char.triggerscript = [{
    comment: 'luaThenRead',
    type: 'manual',
    conditions: [],
    effect: [
      { type: 'triggerlua', code: `
        function luaThenRead(id)
          setChat(id, -1, 'lua edited')
          addChat(id, 'user', 'lua tail')
        end
      ` },
      { type: 'v2GetMessageAtIndex', index: '1', indexType: 'value', outputVar: 'edited', indent: 0 },
      { type: 'v2GetMessageCount', outputVar: 'count', indent: 0 },
      { type: 'modifychat', index: '0', value: 'trigger edited' },
    ],
  }] as never
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  vi.mocked(risuChatParser).mockImplementation((value) => value)
  continuationRuntime.session = fixture.session
  const { runTrigger } = await import('./triggers')
  try {
    await runTrigger(fixture.char, 'manual', { chat: fixture.chat, manualName: 'luaThenRead' })

    expect(fixture.chat.message.map((entry) => entry.data)).toEqual([
      'trigger edited',
      'lua edited',
      'lua tail',
    ])
    expect(fixture.chat.scriptstate).toEqual({ $edited: 'lua edited', $count: '3' })
    expect(fixture.session.activePinReasons).toEqual([])
  } finally {
    vi.mocked(risuChatParser).mockReset()
    continuationRuntime.session = null
  }
})

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

test.each(['current', 'navigated'] as const)('releases a read-only Lua edit operation without publishing an empty commit (%s)', async (state) => {
  const fixture = operationCharacterFixture(`read-only-edit-operation-${state}`)
  const replacement = operationCharacterFixture(`read-only-edit-replacement-${state}`)
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
        alertNormal(id, 'synthetic read')
        return value .. ':read'
      end)
    `,
        },
      ],
    },
  ]
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  continuationRuntime.session = fixture.session
  // The conversation closes after the listener's last read.
  vi.mocked(alertNormal).mockImplementationOnce(() => {
    if (state === 'navigated') continuationRuntime.session = replacement.session
  })
  const applyOperation = vi.spyOn(fixture.session, 'applyOperation')
  const { runLuaEditTrigger } = await import('./scriptings')

  try {
    await expect(
      runLuaEditTrigger(fixture.char, 'editoutput', 'unchanged'),
    ).resolves.toBe('unchanged:read')
    expect(applyOperation).not.toHaveBeenCalled()
    expect(fixture.session.activePinReasons).toEqual([])
  } finally {
    continuationRuntime.session = null
  }
})

test('settles a resumed coroutine rejection without leaving an unhandled rejection', async ({ task }) => {
  const readyMessage = '\nRISUNEST_A4_COROUTINE_READY\n'
  if (process.env.RISUNEST_A4_COROUTINE_CHILD !== 'true') {
    // Another project's copy would start inside this copy's execution allowance.
    const project = task.file.projectName
    const spawnedAt = performance.now()
    let readyAt: number | undefined
    const child = spawn(process.execPath, [
      resolve(process.cwd(), 'node_modules/vitest/vitest.mjs'),
      'run',
      'src/ts/process/scriptings.test.ts',
      ...(project ? ['--project', project] : []),
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
    const childResult = await new Promise<{
      code: number | null
      ready: boolean
      timedOut: 'startup' | 'execution' | null
    }>((resolveChild, rejectChild) => {
      let ready = false
      let timedOut: 'startup' | 'execution' | null = null
      const expire = (phase: 'startup' | 'execution') => {
        timedOut = phase
        child.kill()
      }
      // Cold Vitest transforms and Lua initialization have their own bounded allowance.
      let timeout = setTimeout(() => expire('startup'), 30_000)
      child.stderr.on('data', (chunk) => {
        stderr += chunk
      })
      child.stdout.on('data', (chunk) => {
        stdout += chunk
        if (!ready && !timedOut && stdout.includes(readyMessage)) {
          ready = true
          readyAt = performance.now()
          clearTimeout(timeout)
          timeout = setTimeout(() => expire('execution'), 8_000)
        }
      })
      child.once('error', (error) => {
        clearTimeout(timeout)
        rejectChild(error)
      })
      child.once('close', (code) => {
        clearTimeout(timeout)
        resolveChild({ code, ready, timedOut })
      })
    }).finally(() => {
      if (child.exitCode === null) child.kill()
    })
    const closedAt = performance.now()
    const timing = readyAt === undefined
      ? `no ready line; closed ${Math.round(closedAt - spawnedAt)} ms after spawn`
      : `ready ${Math.round(readyAt - spawnedAt)} ms after spawn; closed ${Math.round(closedAt - readyAt)} ms after ready`

    expect(childResult, `${timing}\n${stdout}\n${stderr}`).toEqual({ code: 0, ready: true, timedOut: null })
    return
  }

  const unhandledRejections: unknown[] = []
  const onUnhandledRejection = (reason: unknown) => {
    unhandledRejections.push(reason)
  }
  process.on('unhandledRejection', onUnhandledRejection)
  engineFunctionTimeout.ms = 50
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockImplementationOnce(async () => {
    // The coroutine has reached its async boundary; only its resumed execution remains.
    process.stdout.write(readyMessage)
    return { type: 'success', result: 'resume into loop' } as never
  })

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
    expect(requestChatData).toHaveBeenCalledOnce()
    expect(unhandledRejections).toEqual([])
  } finally {
    engineFunctionTimeout.ms = null
    process.off('unhandledRejection', onUnhandledRejection)
    vi.mocked(requestChatData).mockReset()
  }
}, 40_000)

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


test.each(['current', 'version-changed', 'session-switched'] as const)(
  'settles started Lua effects on Stop only against the %s owned session', async (state) => {
    const mutation = vi.fn()
    const fixture = operationCharacterFixture(`stop-${state}`, mutation)
    const controller = new AbortController()
    let release!: (value: boolean) => void
    const waiting = new Promise<boolean>((resolve) => { release = resolve })
    vi.mocked(alertConfirm).mockReset()
    vi.mocked(alertConfirm).mockReturnValueOnce(waiting)
    vi.mocked(alertNormal).mockClear()
    fixture.char.triggerscript = [
      { type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: `
        listenEdit('editOutput', function(id, value)
          setChat(id, 0, 'started effect')
          alertConfirm(id, 'wait'):await()
          return value .. ':processed'
        end)
      ` }] },
      { type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: `
        listenEdit('editOutput', function(id, value)
          alertNormal(id, 'must not run')
          return value
        end)
      ` }] },
    ] as never
    installOperationCharacterFixture(fixture)
    vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
    vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
    continuationRuntime.session = fixture.session
    const { runLuaEditTrigger } = await import('./scriptings')
    const pending = runLuaEditTrigger(fixture.char, 'editoutput', 'input', {}, undefined, undefined, controller.signal)
    const outcome = pending.then(value => ({ value }), error => ({ error }))
    await vi.waitFor(() => expect(alertConfirm).toHaveBeenCalledOnce())
    expect(fixture.chat.message[0].data).toBe('message')
    controller.abort()
    if (state === 'version-changed') fixture.session.edit(fixture.session.locate(0), { ...fixture.chat.message[0], data: 'concurrent' })
    if (state === 'session-switched') continuationRuntime.session = operationCharacterFixture('replacement').session
    release(true)
    try {
      const result = await outcome
      if (state === 'current') {
        expect(result).toEqual({ value: 'input' })
        expect(fixture.chat.message[0].data).toBe('started effect')
        expect(mutation).toHaveBeenCalledOnce()
      } else {
        expect(result).toHaveProperty('error')
        expect(fixture.chat.message[0].data).toBe(state === 'version-changed' ? 'concurrent' : 'message')
      }
      expect(alertNormal).not.toHaveBeenCalled()
      expect(fixture.session.activePinReasons).toEqual([])
    } finally { continuationRuntime.session = null }
  },
)

test('preserves an async result while a display engine is created', async () => {
  vi.mocked(requestChatData).mockReset()
  let resolveRequest!: (value: Awaited<ReturnType<typeof requestChatData>>) => void
  let startedRequest!: () => void
  const requestStarted = new Promise<void>((resolve) => { startedRequest = resolve })
  vi.mocked(requestChatData).mockImplementationOnce(() => {
    startedRequest()
    return new Promise((resolve) => { resolveRequest = resolve })
  })
  const fixture = operationCharacterFixture('interleaved-lua-engines')
  const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
  try {
    const pending = runScripted(`onOutput = async(function(id)
      local response = LLM(id, {{role = 'user', content = 'synthetic'}})
      local sum = 0
      for i = 1,5000 do sum = sum + i end
      return response.result .. ':' .. sum
    end)`, { char: fixture.char, chat: fixture.chat, lowLevelAccess: true, mode: 'output' })
    await requestStarted
    await new Promise<void>((resolve) => setImmediate(resolve))
    const display = await runScripted(`listenEdit('editDisplay', function(id, value) return value end)`, {
      char: fixture.char, chat: fixture.chat, mode: 'editDisplay', data: 'synthetic display',
    })
    expect(display.res).toBe('synthetic display')
    resolveRequest({ type: 'success', result: 'synthetic response' })
    expect((await pending).res).toBe('synthetic response:12502500')
    expect(errors).not.toHaveBeenCalled()
  } finally {
    errors.mockRestore()
  }
})

test('preserves synchronous writes and reads around an async LLM boundary', async () => {
  vi.mocked(requestChatData).mockReset()
  vi.mocked(requestChatData).mockResolvedValueOnce({ type: 'success', result: 'synthetic response' } as never)
  const fixture = operationCharacterFixture('lua-write-read-await')
  const variables = new Map<string, string>()
  const result = await runScripted(`onOutput = async(function(id)
    setChatVar(id, 'counter', '1')
    local first = getChatVar(id, 'counter')
    LLM(id, {{role = 'user', content = 'synthetic'}})
    setChatVar(id, 'counter', tostring(tonumber(getChatVar(id, 'counter')) + 1))
    return first .. ':' .. getChatVar(id, 'counter')
  end)`, {
    char: fixture.char, chat: fixture.chat, mode: 'output', lowLevelAccess: true,
    setVar: (key, value) => { variables.set(key, value) },
    getVar: key => variables.get(key) ?? 'null',
  })
  expect(result.res).toBe('1:2')
  expect(variables.get('counter')).toBe('2')
})

test('expands lorebook and persona text with the variables and messages written earlier in the Lua button handler', async () => {
  const fixture = operationCharacterFixture('lorebook-pending-variables')
  const template = '{{getvar::request_context}} after {{lastmessage}}'
  fixture.char.globalLore = [{
    comment: 'request-template', content: `Lore: ${template}`,
    key: '', secondkey: '', alwaysActive: false, selective: false, insertorder: 100, mode: 'normal',
  }]
  fixture.char.lowLevelAccess = true
  fixture.chat.scriptstate = { $request_context: 'previous context' }
  fixture.char.triggerscript = luaTriggerScript(`
    onButtonClick = async(function(id, data)
      setChatVar(id, 'request_context', 'updated context')
      addChat(id, 'char', 'fresh reply')
      local books = getLoreBooks(id, 'request-template')
      local persona = getPersonaDescription(id)
      local loaded = loadLoreBooks(id)
      addChat(id, 'user', books[1].content .. ' | ' .. persona .. ' | ' .. loaded[1].data)
    end)
  `)
  installOperationCharacterFixture(fixture)
  vi.mocked(getCurrentChat).mockReturnValue(fixture.chat)
  vi.mocked(getCurrentCharacter).mockReturnValue(fixture.char)
  vi.mocked(getPersonaPrompt).mockReturnValue(`Persona: ${template}`)
  vi.mocked(tokenize).mockResolvedValue(1)
  vi.mocked(loadLoreBookV3PromptFromCompatibilitySnapshot).mockResolvedValue(
    { actives: [{ prompt: `Loaded: ${template}`, role: 'system' }] } as never,
  )
  continuationRuntime.session = fixture.session
  vi.doMock('dompurify', async () => {
    const { Window } = await import('happy-dom')
    const { default: createDOMPurify } = await vi.importActual<typeof import('dompurify')>('dompurify')
    return { default: createDOMPurify(new Window() as unknown as Parameters<typeof createDOMPurify>[0]) }
  })
  const parser = await vi.importActual<typeof import('../parser/parser.svelte')>('../parser/parser.svelte')
  vi.mocked(risuChatParser).mockImplementation(parser.risuChatParser)
  const { runLuaInteractionTrigger } = await import('./scriptings')
  try {
    await runLuaInteractionTrigger('button', fixture.char, 'request')
    const expanded = 'updated context after fresh reply'
    expect(fixture.chat.message.at(-1)?.data)
      .toBe(`Lore: ${expanded} | Persona: ${expanded} | Loaded: ${expanded}`)
    expect(fixture.chat.scriptstate?.$request_context).toBe('updated context')
  } finally {
    vi.doUnmock('dompurify')
    vi.mocked(risuChatParser).mockReset()
    vi.mocked(getPersonaPrompt).mockReset()
    vi.mocked(tokenize).mockReset()
    vi.mocked(loadLoreBookV3PromptFromCompatibilitySnapshot).mockReset()
    continuationRuntime.session = null
  }
})
