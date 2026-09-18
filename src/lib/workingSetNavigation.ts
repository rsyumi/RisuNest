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
import { alertStore, DBState, selectedCharID } from 'src/ts/stores.svelte'
import { findCharacterIndexbyId } from 'src/ts/util'
import { get } from 'svelte/store'
import { doingChat } from 'src/ts/process/generationState'
import { language } from 'src/lang'

const PLAYGROUND_CHARACTER_ID = '§playground'

// Leaving a conversation is refused while a response is being generated. The
// control that triggered it would otherwise look broken, so say why. The toast
// is published through the store directly because importing src/ts/alert here
// would pull the whole database module graph into every navigation entry point.
function reportBlockedNavigation(): false {
    if (get(doingChat)) {
        alertStore.set({
            type: 'toast',
            msg: language.navigationBlockedWhileGenerating,
        })
    }
    return false
}

export async function clearCharacterSelection(): Promise<boolean> {
    try {
        if (!await deactivateActiveWorkingSet()) return reportBlockedNavigation()
        selectedCharID.set(-1)
        return true
    } catch (error) {
        const { alertError } = await import('src/ts/alert')
        alertError(error instanceof Error ? error : String(error))
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
    if (characterIndex === -1 || !await changeChar(characterIndex)) {
        return reportBlockedNavigation()
    }

    characterIndex = findCharacterIndexbyId(PLAYGROUND_CHARACTER_ID)
    if (characterIndex === -1) return false
    const characterValue = DBState.db.characters[characterIndex] as character
    const updated = configurePlaygroundCharacter(characterValue)
    markPersistentDataDirty(new TextEncoder().encode(JSON.stringify(updated)).byteLength)
    return true
}
