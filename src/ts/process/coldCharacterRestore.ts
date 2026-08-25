import type { character, groupChat } from '../storage/database.svelte'
import type { CharacterDetail } from '../storage/persistentDataStore'
import { canonicalJson } from '../storage/saveCoordinator'
import {
    readPersistentCharacterDetail,
    replacePersistentCompleteCharacter,
} from '../storage/persistentDataRuntime.svelte'
import { getColdStorageItem } from './coldstorage.svelte'

type RestoredCharacter = CharacterDetail | character | groupChat

class ColdRestoreSupersededError extends Error {}

export async function restoreColdPersistentCharacter(
    characterId: string,
    options: {
        errorMessage: string
        isCurrent(): boolean
    },
): Promise<RestoredCharacter | null> {
    if (!options.isCurrent()) return null
    const detail = await readPersistentCharacterDetail(
        characterId,
        'cold-character-inspection',
    )
    if (!options.isCurrent()) return null
    if (!detail?.coldstorage) return detail
    const coldData = await getColdStorageItem(detail.coldstorage)
    if (!options.isCurrent()) return null
    if (!coldData?.character || coldData.character.chaId !== characterId) {
        throw new Error(options.errorMessage)
    }
    let currentCharacter: character | groupChat | null = null
    let replaced: boolean
    try {
        replaced = await replacePersistentCompleteCharacter(
            characterId,
            'cold-character-restore',
            (current) => {
                const { chats: _chats, ...currentDetail } = current
                if (canonicalJson(currentDetail) !== canonicalJson(detail)) {
                    currentCharacter = current
                    throw new ColdRestoreSupersededError()
                }
                return coldData.character
            },
        )
    } catch (error) {
        if (error instanceof ColdRestoreSupersededError) return currentCharacter
        throw error
    }
    if (!options.isCurrent()) return null
    if (!replaced) throw new Error(options.errorMessage)
    return coldData.character
}
