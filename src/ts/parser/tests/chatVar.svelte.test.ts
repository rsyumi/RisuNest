import fc from 'fast-check'
import { writable } from 'svelte/store'
import { beforeEach, expect, test, vi } from 'vitest'
import { DBState, selectedCharID } from '../../stores.svelte'
import { getChatVar, getChatVarFromConversation, getGlobalChatVar, setChatVar } from '../chatVar.svelte'
import { risuChatParser } from '../parser.svelte'
import { resetChatVariables } from './cbs/lib'
import type { Chat, Database } from '../../storage/database.svelte'

//#region module mocks

vi.mock(
  import('../../storage/database.svelte'),
  () =>
    ({
      appVer: '1234.5.67',
      getCurrentCharacter: () => ({}),
      getCurrentChat: () => ({}),
      getDatabase: () => ({}),
    } as typeof import('../../storage/database.svelte'))
)

vi.mock(import('../../globalApi.svelte'), () => ({
  aiWatermarkingLawApplies: () => false,
  getFileSrc: () => Promise.resolve(''),
}))

vi.mock(import('../../stores.svelte'), () => {
  return {
    DBState: {
      db: {
        characters: [
          {
            chatPage: 0,
            chats: [
              {
                scriptstate: {},
              },
            ],
            defaultVariables: '',
          },
        ],
        globalChatVariables: {},
        templateDefaultVariables: '',
      },
    },
    selIdState: {
      selId: 0,
    },
    selectedCharID: writable(0),
  } as typeof import('../../stores.svelte')
})

//#endregion

const anyValidDefaultVarKey = fc.string({ minLength: 1, unit: 'grapheme' }).filter((s) => !/[=\n]/.test(s))
const anyValidDefaultVarValue = fc
  .anything()
  .map(JSON.stringify)
  .filter((s) => s !== undefined && !/[=\n]/.test(s))

beforeEach(() => {
  vi.resetAllMocks()
  resetChatVariables()
})

test('can get a character default variable', () => {
  fc.assert(
    fc.property(anyValidDefaultVarKey, anyValidDefaultVarValue, (key, value) => {
      DBState.db.characters[0].defaultVariables = `${key}=${value}`
      expect(getChatVar(key)).toBe(value)
    })
  )
})

test('can get a template default variable', () => {
  fc.assert(
    fc.property(anyValidDefaultVarKey, anyValidDefaultVarValue, (key, value) => {
      DBState.db.templateDefaultVariables = `${key}=${value}`
      expect(getChatVar(key)).toBe(value)
    })
  )
})

test('can set and get a chat variable', () => {
  fc.assert(
    fc.property(
      fc.string({ unit: 'grapheme' }),
      fc
        .anything()
        .filter((v) => v !== undefined)
        .map(JSON.stringify),
      (key, value) => {
        setChatVar(key, value)
        expect(getChatVar(key)).toBe(value)
      }
    )
  )
})

test('can set a chat variable over its default value', () => {
  DBState.db.characters[0].defaultVariables = 'char=default'
  DBState.db.templateDefaultVariables = 'template=default'

  setChatVar('char', 'overridden')
  setChatVar('template', 'overridden')

  expect(getChatVar('char')).toBe('overridden')
  expect(getChatVar('template')).toBe('overridden')
})

test('can get a global chat variable', () => {
  fc.assert(
    fc.property(
      fc.string({ unit: 'grapheme' }),
      fc
        .anything()
        .filter((v) => v !== undefined)
        .map(JSON.stringify),
      (key, value) => {
        DBState.db.globalChatVariables[`toggle_${key}`] = value

        expect(getGlobalChatVar(`toggle_${key}`)).toBe(value)
      }
    )
  )
})

test('returns "null" for undefined variables', () => {
  fc.assert(
    fc.property(fc.string({ unit: 'grapheme' }), (key) => {
      expect(getChatVar(key)).toBe('null')
      expect(getGlobalChatVar(`toggle_${key}`)).toBe('null')
    })
  )
})

test('reads defaults without mutating a fresh reactive conversation', () => {
  const chat = $state({ id: 'synthetic-chat' } as Chat)
  const database = {
    characters: [{ chaId: 'synthetic-character', defaultVariables: 'mode=1' }],
    templateDefaultVariables: 'template=2',
  } as Database
  const values = $derived.by(() =>
    ['mode', 'template', 'missing'].map((key) =>
      getChatVarFromConversation(database, 'synthetic-character', chat, key)
    )
  )
  const readValues = () => values

  expect(readValues()).toEqual(['1', '2', 'null'])
  expect(Object.hasOwn(chat, 'scriptstate')).toBe(false)
  chat.scriptstate = { $mode: '0' }
  expect(readValues()).toEqual(['0', '2', 'null'])
})

test('reads the selected owner without visiting the rest of the library', () => {
  const visited: number[] = []
  const library = ['first', 'second', 'selected'].map((name) => ({
    chaId: `synthetic-${name}`,
    defaultVariables: `owner=${name}`,
  }))
  const characters = new Proxy(library, {
    get(target, property, receiver) {
      if (typeof property === 'string' && /^\d+$/.test(property)) visited.push(Number(property))
      return Reflect.get(target, property, receiver)
    },
  })
  const database = { characters, templateDefaultVariables: '' } as unknown as Database
  const chat = { id: 'synthetic-chat' } as Chat
  selectedCharID.set(2)
  try {
    expect(getChatVarFromConversation(database, 'synthetic-selected', chat, 'owner')).toBe('selected')
    expect(visited).toEqual([2])
    expect(getChatVarFromConversation(database, 'synthetic-first', chat, 'owner')).toBe('first')
  } finally {
    selectedCharID.set(0)
  }
})

test('reads template defaults and model flags from the preset the parser is given', () => {
  DBState.db.templateDefaultVariables = 'mode=applied'
  const preset = {
    ...DBState.db,
    templateDefaultVariables: 'mode=edited',
    aiModel: 'claude-opus-4-5-20251101',
    enableCustomFlags: true,
    customFlags: [],
  } as unknown as Database

  expect(risuChatParser('{{getvar::mode}}')).toBe('applied')
  expect(risuChatParser('{{getvar::mode}} {{prefillsupported}}', { db: preset })).toBe('edited 0')
  expect(risuChatParser('{{prefillsupported}}', { db: { ...preset, enableCustomFlags: false } })).toBe('1')
})
