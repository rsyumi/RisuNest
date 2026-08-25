import {
    changeChar,
    characterFormatUpdate,
    commitDetachedCharacter,
    createBlankChar,
} from 'src/ts/characters'
import type { character } from 'src/ts/storage/database.svelte'
import {
    deactivateActiveWorkingSet,
    markPersistentDataDirty,
} from 'src/ts/storage/persistentDataRuntime.svelte'
import { DBState, selectedCharID } from 'src/ts/stores.svelte'
import { findCharacterIndexbyId } from 'src/ts/util'

const PLAYGROUND_CHARACTER_ID = '§playground'

export async function clearCharacterSelection(): Promise<boolean> {
    try {
        if (!await deactivateActiveWorkingSet()) return false
        selectedCharID.set(-1)
        return true
    } catch {
        return false
    }
}

function configurePlaygroundCharacter(value: character): character {
    value.utilityBot = true
    value.name = 'assistant'
    value.firstMessage = '{{none}}'
    return characterFormatUpdate(value) as character
}

export async function activatePlaygroundCharacter(): Promise<boolean> {
    let characterIndex = findCharacterIndexbyId(PLAYGROUND_CHARACTER_ID)
    if (characterIndex === -1) {
        const value = createBlankChar()
        value.chaId = PLAYGROUND_CHARACTER_ID
        await commitDetachedCharacter(
            configurePlaygroundCharacter(value),
            'create-playground-character',
        )
        characterIndex = findCharacterIndexbyId(PLAYGROUND_CHARACTER_ID)
    }
    if (characterIndex === -1 || !await changeChar(characterIndex)) return false

    characterIndex = findCharacterIndexbyId(PLAYGROUND_CHARACTER_ID)
    if (characterIndex === -1) return false
    const characterValue = DBState.db.characters[characterIndex] as character
    const updated = configurePlaygroundCharacter(characterValue)
    markPersistentDataDirty(new TextEncoder().encode(JSON.stringify(updated)).byteLength)
    return true
}
