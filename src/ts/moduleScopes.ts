import { get } from 'svelte/store'
import { DBState, selectedCharID } from './stores.svelte'
import { alertError } from './alert'
import { backgroundErrorMessage } from './storage/backgroundErrorMessage'
import { mutateCharacterDetailFields } from './storage/persistentDataRuntime.svelte'
import {
    captureChatBindingTarget,
    conversationMutationBlockedByGeneration,
    saveChatBinding,
    updateChatBinding,
} from './chatBindings.svelte'
import { withModule, type ModuleScope } from './process/moduleSelection'

// Module scope edits from the module menu. A windowed chat cannot be saved after a direct write
// to its metadata or character detail, so chat and character scopes go through the binding and
// detail mutations, as the sidebar toggles do.

let queue: Promise<unknown> = Promise.resolve()

// One edit at a time, each reading the list when it runs, so quick taps do not overwrite each other.
function serialize<T>(task: () => Promise<T>): Promise<T> {
    const run = queue.then(task, task)
    queue = run.catch(() => {})
    return run
}

function reportFailure(error: unknown) {
    alertError(backgroundErrorMessage(error))
}

/** Turns one scope of a module on or off for the selected character and chat. Resolves whether the edit was made. */
export function setModuleScope(scope: ModuleScope, moduleId: string, enabled: boolean): Promise<boolean> {
    if (scope === 'global') {
        DBState.db.enabledModules = withModule(DBState.db.enabledModules, moduleId, enabled)
        return Promise.resolve(true)
    }
    if (scope === 'character') {
        const owner = DBState.db.characters[get(selectedCharID)]
        if (!owner) return Promise.resolve(false)
        return serialize(async () => {
            try {
                const saved = await mutateCharacterDetailFields(owner.chaId, 'toggle-character-module', ({ character }) => {
                    character.modules = withModule(character.modules, moduleId, enabled)
                })
                // A character outside the persistent store keeps the in-memory write.
                if (!saved) owner.modules = withModule(owner.modules, moduleId, enabled)
                return true
            } catch (error) {
                reportFailure(error)
                return false
            }
        })
    }
    const target = captureChatBindingTarget()
    if (!target) return Promise.resolve(false)
    return serialize(async () => {
        try {
            const conversation = target.conversation
            // Checked when the edit runs, since a generation can start while it waits behind earlier edits.
            if (conversationMutationBlockedByGeneration(conversation.id)) return false
            await updateChatBinding(conversation, { modules: withModule(conversation.modules, moduleId, enabled) })
            await saveChatBinding()
            return true
        } catch (error) {
            reportFailure(error)
            return false
        }
    })
}
