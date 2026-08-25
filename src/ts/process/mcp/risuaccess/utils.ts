import { get } from 'svelte/store'
import { readPersistentCharacterDetail } from 'src/ts/storage/persistentDataRuntime.svelte'
import type { CharacterDetail } from 'src/ts/storage/persistentDataStore'
import { DBState, selectedCharID } from 'src/ts/stores.svelte'

export function resolveCharacterId(id: string): string | null {
  const catalog = DBState.db.characters ?? []
  const summary = id
    ? catalog.find((candidate) => candidate.chaId === id)
      ?? catalog.find((candidate) => candidate.name === id)
    : catalog[get(selectedCharID)]
  return summary?.chaId ?? null
}

export async function getCharacter(id: string): Promise<CharacterDetail | null> {
  const characterId = resolveCharacterId(id)
  if (!characterId) return null
  return readPersistentCharacterDetail(characterId, 'risuaccess-character-detail-read')
}
