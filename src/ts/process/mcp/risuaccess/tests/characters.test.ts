import type { loreBook } from 'src/ts/storage/database.svelte'
import { beforeEach, expect, test, vi } from 'vitest'
import { CharacterHandler } from '../characters'

const state = vi.hoisted(() => ({
  character: { chaId: 'synthetic-character', name: 'Synthetic', globalLore: [] as loreBook[] },
}))

vi.mock(import('katex'), () => ({}))
vi.mock(import('src/ts/lite'), () => ({}))
vi.mock(import('src/ts/stores.svelte'), () => ({
  DBState: { db: { characters: [] } },
  selIdState: { selId: 0 },
}) as unknown as typeof import('src/ts/stores.svelte'))
vi.mock(import('src/ts/alert'), () => ({
  alertConfirm: vi.fn(async () => true),
}))
vi.mock(import('../utils'), () => ({
  getCharacter: vi.fn(async () => state.character),
  resolveCharacterId: vi.fn(() => state.character.chaId),
}) as unknown as typeof import('../utils'))
vi.mock(import('src/ts/storage/persistentDataRuntime.svelte'), async (importOriginal) => ({
  ...await importOriginal(),
  mutatePersistentCharacterDetail: vi.fn(async (_id, _reason, mutate) => {
    mutate({ character: state.character } as Parameters<typeof mutate>[0])
    return true
  }),
}))

const entry = (): loreBook => ({
  alwaysActive: false,
  comment: 'synthetic',
  content: 'syntheticContent',
  insertorder: 100,
  key: 'syntheticKey',
  mode: 'normal',
  secondkey: '',
  selective: false,
})

beforeEach(() => {
  state.character.globalLore = [entry()]
})

test('treats null lorebook arguments as not given', async () => {
  const instance = new CharacterHandler()

  await instance.handle('risu-set-character-lorebook', {
    id: 'synthetic-character', name: 'synthetic', content: null, keys: null, newName: null, alwaysActive: null,
  })
  await instance.handle('risu-set-character-lorebook', {
    id: 'synthetic-character', name: 'added', content: 'addedContent', alwaysActive: null,
  })

  expect(state.character.globalLore).toEqual([
    entry(),
    expect.objectContaining({ comment: 'added', content: 'addedContent', alwaysActive: false }),
  ])
})
