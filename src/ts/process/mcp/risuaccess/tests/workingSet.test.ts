import type { character } from 'src/ts/storage/database.svelte'
import { DBState } from 'src/ts/stores.svelte'
import { beforeEach, expect, test, vi } from 'vitest'
import type { RPCToolCallTextContent } from '../../mcplib'
import { CharacterHandler } from '../characters'
import { ChatHandler } from '../chats'

const runtimeMocks = vi.hoisted(() => ({
  unexpectedNativeRuntimeAccess: () => {
    throw new Error('Unexpected native runtime access in this test')
  },
  mutateCharacter: vi.fn(),
  readCharacter: vi.fn(),
  readConversationAt: vi.fn(),
  readSelectedConversation: vi.fn(),
  readSelectedConversationWindow: vi.fn(),
}))

const alertMocks = vi.hoisted(() => ({
  confirm: vi.fn(),
}))

vi.mock(import('katex'), () => ({}))
vi.mock(import('src/ts/lite'), () => ({}))
vi.mock('src/lang', () => ({
  language: {
    mcpAccessPrompt: '{{tool}} {{action}}',
  },
}))
vi.mock(import('src/ts/alert'), () => ({
  alertConfirm: alertMocks.confirm,
}))
vi.mock('src/ts/util', () => ({
  pickHashRand: () => 1,
}))
vi.mock(import('src/ts/storage/persistentDataRuntime.svelte'), () => ({
  acquireDestructiveReplacementFence: runtimeMocks.unexpectedNativeRuntimeAccess,
  capturePersistentMutationToken: runtimeMocks.unexpectedNativeRuntimeAccess,
  getPersistentDataRuntime: runtimeMocks.unexpectedNativeRuntimeAccess,
  materializeMaximumCompatibilityWorkingSet: vi.fn(),
  mutatePersistentCharacterDetail: runtimeMocks.mutateCharacter,
  readPersistentCharacterDetail: runtimeMocks.readCharacter,
  readPersistentConversationAt: runtimeMocks.readConversationAt,
  readPersistentSelectedConversation: runtimeMocks.readSelectedConversation,
  readPersistentSelectedConversationWindow: runtimeMocks.readSelectedConversationWindow,
  releaseInactiveWorkingSet: vi.fn(),
}))
vi.mock('src/ts/stores.svelte', () => ({
  DBState: {
    db: {
      characters: [],
    },
  },
  selIdState: {
    selId: 0,
  },
  selectedCharID: {
    subscribe(run: (value: number) => void) {
      run(0)
      return () => undefined
    },
    set: vi.fn(),
    update: vi.fn(),
  },
}))

const makeCharacter = (overrides: Partial<character> = {}): character => ({
  chaId: 'char-1',
  name: 'Catalog name',
  type: 'character',
  desc: 'authoritative description',
  firstMessage: 'hello',
  chats: [
    {
      id: 'chat-1',
      message: [
        { role: 'user', data: 'oldest' },
        { role: 'char', data: 'newest' },
      ],
    },
  ],
  chatPage: 0,
  globalLore: [],
  customscript: [],
  triggerscript: [],
  additionalAssets: [],
  alternateGreetings: [],
  ...overrides,
} as character)

const makeCharacterDetail = (overrides: Partial<character> = {}) => {
  const { chats: _chats, ...detail } = makeCharacter(overrides)
  return detail
}

const text = (result: Awaited<ReturnType<CharacterHandler['handle']>>): string =>
  (result?.[0] as RPCToolCallTextContent).text

beforeEach(() => {
  vi.resetAllMocks()
  alertMocks.confirm.mockResolvedValue(true)
  DBState.db.characters = [{
    chaId: 'char-1',
    name: 'Catalog name',
    type: 'character',
    chats: [],
  } as character]
  runtimeMocks.readCharacter.mockResolvedValue(makeCharacterDetail())
  runtimeMocks.readConversationAt.mockResolvedValue(makeCharacter().chats[0])
  runtimeMocks.readSelectedConversation.mockResolvedValue({
    character: makeCharacterDetail(),
    conversation: makeCharacter().chats[0],
  })
  runtimeMocks.readSelectedConversationWindow.mockResolvedValue({
    revision: 4,
    character: makeCharacterDetail(),
    conversation: {
      characterId: 'char-1',
      conversationId: 'chat-1',
      messages: makeCharacter().chats[0].message,
      startIndex: 0,
      endIndex: 2,
      totalMessages: 2,
      hasMoreBefore: false,
      hasMoreAfter: false,
    },
  })
  runtimeMocks.mutateCharacter.mockImplementation(
    async (_id: string, _reason: string, mutate: (state: { character: character }) => void) => {
      const character = makeCharacter()
      await mutate({ character })
      return true
    },
  )
})

test('reads an inactive catalog stub through the authoritative detached character seam', async () => {
  const stub = DBState.db.characters[0]
  const result = await new CharacterHandler().getCharacterInfo('Catalog name', ['description'])

  expect(text(result)).toBe(JSON.stringify({ description: 'authoritative description' }))
  expect(runtimeMocks.readCharacter).toHaveBeenCalledWith('char-1', 'risuaccess-character-detail-read')
  expect(DBState.db.characters[0]).toBe(stub)
  expect((DBState.db.characters[0] as character).desc).toBeUndefined()
})

test('blank character ID resolves the selected catalog entry without installing the full character', async () => {
  const stub = DBState.db.characters[0]
  const result = await new CharacterHandler().getCharacterInfo('', ['id', 'name'])

  expect(text(result)).toBe(JSON.stringify({ id: 'char-1', name: 'Catalog name' }))
  expect(runtimeMocks.readCharacter).toHaveBeenCalledWith('char-1', 'risuaccess-character-detail-read')
  expect(DBState.db.characters[0]).toBe(stub)
})

test('reads the current resident character back through persistence after the runtime flush boundary', async () => {
  const live = makeCharacter({ desc: 'live working copy' })
  DBState.db.characters = [live]
  runtimeMocks.readCharacter.mockResolvedValue(makeCharacterDetail({ desc: 'persisted after flush' }))

  const result = await new CharacterHandler().getCharacterInfo('', ['description'])

  expect(text(result)).toBe(JSON.stringify({ description: 'persisted after flush' }))
  expect(runtimeMocks.readCharacter).toHaveBeenCalledWith('char-1', 'risuaccess-character-detail-read')
  expect(DBState.db.characters[0]).toBe(live)
  expect(live.desc).toBe('live working copy')
})

test('reads the selected inactive conversation with one atomic persistence operation', async () => {
  const stub = DBState.db.characters[0]
  const result = await new ChatHandler().getChatHistory('char-1', 20, 0)

  expect((result[0] as RPCToolCallTextContent).text).toBe(JSON.stringify([
    { type: 'text', text: 'Catalog name: newest' },
    { type: 'text', text: 'User: oldest' },
  ]))
  expect(runtimeMocks.readSelectedConversationWindow).toHaveBeenCalledTimes(1)
  expect(runtimeMocks.readSelectedConversationWindow).toHaveBeenCalledWith(
    'char-1',
    20,
    0,
    'risuaccess-chat-history-read',
  )
  expect(runtimeMocks.readSelectedConversation).not.toHaveBeenCalled()
  expect(runtimeMocks.readCharacter).not.toHaveBeenCalled()
  expect(runtimeMocks.readConversationAt).not.toHaveBeenCalled()
  expect(DBState.db.characters[0]).toBe(stub)
})

test('reports a character missing when the atomic selected-conversation read returns null', async () => {
  runtimeMocks.readSelectedConversationWindow.mockResolvedValue(null)

  const result = await new ChatHandler().getChatHistory('char-1', 20, 0)

  expect((result[0] as RPCToolCallTextContent).text).toBe(
    'Error: Character with ID char-1 not found.',
  )
  expect(runtimeMocks.readSelectedConversationWindow).toHaveBeenCalledTimes(1)
})

test('returns an empty history when a character has no selected conversation', async () => {
  runtimeMocks.readSelectedConversationWindow.mockResolvedValue({
    revision: 4,
    character: makeCharacterDetail(),
    conversation: null,
  })

  const result = await new ChatHandler().getChatHistory('char-1', 20, 0)

  expect((result[0] as RPCToolCallTextContent).text).toBe(JSON.stringify([]))
  expect(runtimeMocks.readSelectedConversationWindow).toHaveBeenCalledTimes(1)
})

test('reports a group before treating its null selected conversation as empty history', async () => {
  runtimeMocks.readSelectedConversationWindow.mockResolvedValue({
    revision: 4,
    character: { ...makeCharacterDetail(), type: 'group', name: 'Group' },
    conversation: null,
  })

  const result = await new ChatHandler().getChatHistory('char-1', 20, 0)

  expect((result[0] as RPCToolCallTextContent).text).toBe(
    'Error: The id pointed to a group chat, not a character.',
  )
  expect(runtimeMocks.readSelectedConversationWindow).toHaveBeenCalledTimes(1)
})

test('preserves newest-first offset paging without requesting a complete conversation', async () => {
  runtimeMocks.readSelectedConversationWindow.mockResolvedValue({
    revision: 4,
    character: makeCharacterDetail(),
    conversation: {
      characterId: 'char-1',
      conversationId: 'chat-1',
      messages: [
        { role: 'char', data: 'third newest', chatId: 'duplicate' },
        { role: 'user', data: 'second newest' },
        { role: 'char', data: 'newest', chatId: 'duplicate' },
      ],
      startIndex: 97,
      endIndex: 100,
      totalMessages: 102,
      hasMoreBefore: true,
      hasMoreAfter: true,
    },
  })

  const result = await new ChatHandler().getChatHistory('char-1', 3, 2)

  expect((result[0] as RPCToolCallTextContent).text).toBe(JSON.stringify([
    { type: 'text', text: 'Catalog name: newest' },
    { type: 'text', text: 'User: second newest' },
    { type: 'text', text: 'Catalog name: third newest' },
  ]))
  expect(runtimeMocks.readSelectedConversationWindow).toHaveBeenCalledWith(
    'char-1',
    3,
    2,
    'risuaccess-chat-history-read',
  )
  expect(runtimeMocks.readSelectedConversation).not.toHaveBeenCalled()
  expect(runtimeMocks.readConversationAt).not.toHaveBeenCalled()
})

test('persists an inactive character write through a stable ID without mutating the catalog stub', async () => {
  const stub = DBState.db.characters[0]
  let committed: character | undefined
  runtimeMocks.mutateCharacter.mockImplementation(
    async (_id: string, _reason: string, mutate: (state: { character: character }) => void) => {
      committed = makeCharacter()
      await mutate({ character: committed })
      return true
    },
  )

  const result = await new CharacterHandler().setCharacterInfo('Catalog name', { name: 'Updated' })

  expect(text(result)).toBe('Successfully updated character Updated')
  expect(runtimeMocks.mutateCharacter).toHaveBeenCalledWith(
    'char-1',
    'risu-set-character-info',
    expect.any(Function),
  )
  expect(committed?.name).toBe('Updated')
  expect(DBState.db.characters[0]).toBe(stub)
  expect(DBState.db.characters[0].name).toBe('Catalog name')
})

test('does not report success when a persistent character commit fails', async () => {
  const failure = new Error('commit failed')
  runtimeMocks.mutateCharacter.mockRejectedValue(failure)

  await expect(
    new CharacterHandler().setCharacterInfo('char-1', { description: 'changed' }),
  ).rejects.toBe(failure)
})

test('routes lore, regex, asset, and Lua writes through persistent detail mutations', async () => {
  const makeMutableCharacter = () => makeCharacter({
    globalLore: [{
      alwaysActive: false,
      comment: 'Lore',
      content: 'old lore',
      insertorder: 100,
      key: 'old',
      mode: 'normal',
      secondkey: '',
      selective: false,
    }],
    customscript: [{
      ableFlag: true,
      comment: 'Regex',
      flag: '',
      in: 'old regex',
      out: 'old output',
      type: 'editdisplay',
    }],
    additionalAssets: [['Asset', 'asset/path', 'png']],
    triggerscript: [{
      comment: '',
      conditions: [],
      effect: [{ type: 'triggerlua', code: 'old lua' }],
      type: 'manual',
    }],
  })
  let committed = makeMutableCharacter()
  runtimeMocks.readCharacter.mockImplementation(async () => makeCharacterDetail())
  runtimeMocks.mutateCharacter.mockImplementation(
    async (_id: string, _reason: string, mutate: (state: { character: character }) => void) => {
      committed = makeMutableCharacter()
      await mutate({ character: committed })
      return true
    },
  )
  const handler = new CharacterHandler()

  await handler.setCharacterLorebook('char-1', 'Lore', 'new lore')
  expect(committed.globalLore[0].content).toBe('new lore')

  await handler.deleteCharacterLorebook('char-1', 'Lore')
  expect(committed.globalLore).toEqual([])

  await handler.setCharacterRegexScripts('char-1', 'Regex', undefined, 'new regex')
  expect(committed.customscript[0].in).toBe('new regex')

  await handler.deleteCharacterRegexScripts('char-1', 'Regex')
  expect(committed.customscript).toEqual([])

  await handler.deleteCharacterAdditionalAssets('char-1', 'Asset')
  expect(committed.additionalAssets).toEqual([])

  await handler.setCharacterLuaScript('char-1', 'new lua')
  expect(committed.triggerscript[0].effect[0]).toMatchObject({
    type: 'triggerlua',
    code: 'new lua',
  })
  expect(runtimeMocks.mutateCharacter).toHaveBeenCalledTimes(6)
})

test('preserves missing and group error responses without changing selection', async () => {
  const handler = new CharacterHandler()
  runtimeMocks.readCharacter.mockResolvedValueOnce(null)
  expect(text(await handler.getCharacterInfo('char-1', ['name']))).toBe(
    'Error: Character with ID char-1 not found.',
  )

  runtimeMocks.readCharacter.mockResolvedValueOnce({
    chaId: 'char-1',
    name: 'Group',
    type: 'group',
    chats: [],
  })
  expect(text(await handler.getCharacterInfo('char-1', ['name']))).toBe(
    'Error: The id pointed to a group chat, not a character.',
  )

  runtimeMocks.readCharacter.mockResolvedValue(makeCharacterDetail())
  runtimeMocks.mutateCharacter.mockResolvedValueOnce(false)
  expect(text(await handler.setCharacterInfo('char-1', { name: 'Updated' }))).toBe(
    'Error: Character with ID char-1 not found.',
  )

  runtimeMocks.mutateCharacter.mockImplementationOnce(
    async (_id: string, _reason: string, mutate: (state: { character: any }) => void) => {
      await mutate({ character: { chaId: 'char-1', name: 'Group', type: 'group' } })
      return true
    },
  )
  expect(text(await handler.setCharacterInfo('char-1', { name: 'Updated' }))).toBe(
    'Error: The id pointed to a group chat, not a character.',
  )
  expect(DBState.db.characters[0].chaId).toBe('char-1')
})
