import { DBState } from './stores.svelte'
import { alertError } from './alert'
import { backgroundErrorMessage } from './storage/backgroundErrorMessage'
import type { character } from './storage/database.svelte'
import { mutateCharacterDetailFields } from './storage/persistentDataRuntime.svelte'
import { applyToggleValues, type ToggleValues } from './toggleBindings'
import {
    captureChatBindingTarget,
    conversationMutationBlockedByGeneration,
    updateChatBinding,
} from './chatBindings.svelte'

// Sidebar toggle edits of the selected chat and character. A windowed chat
// cannot be saved after a direct write to its metadata or character detail,
// so these go through the explicit binding and detail mutations instead.

function reportFailure(error: unknown) {
    alertError(backgroundErrorMessage(error))
}

export async function setToggleValue(key: string, value: string): Promise<void> {
    try {
        const conversation = captureChatBindingTarget()?.conversation
        const local = !!conversation?.useLocallySetGlobalVariables
        if (conversation && (local || conversation.GLGlobalVariables?.[key] !== undefined)) {
            if (conversationMutationBlockedByGeneration(conversation.id)) return
            const values = { ...conversation.GLGlobalVariables }
            if (local) values[key] = value
            else delete values[key]
            await updateChatBinding(conversation, { GLGlobalVariables: values })
            if (local) return
        }
        DBState.db.globalChatVariables[key] = value
    } catch (error) {
        reportFailure(error)
    }
}

/** Applies preset values outside a toggle binding. Outside local mode they replace the chat's local overrides, as setToggleValue does. */
export async function applyGlobalToggleValues(values: ToggleValues, keys: readonly string[]): Promise<boolean> {
    try {
        const conversation = captureChatBindingTarget()?.conversation
        const replaced = [...keys, ...Object.keys(values)]
            .filter((key) => key.startsWith('toggle_') && conversation?.GLGlobalVariables?.[key] !== undefined)
        if (conversation && !conversation.useLocallySetGlobalVariables && replaced.length > 0) {
            if (conversationMutationBlockedByGeneration(conversation.id)) return false
            const overrides = { ...conversation.GLGlobalVariables }
            for (const key of replaced) delete overrides[key]
            await updateChatBinding(conversation, { GLGlobalVariables: overrides })
        }
        applyToggleValues(DBState.db.globalChatVariables, values, keys)
        return true
    } catch (error) {
        reportFailure(error)
        return false
    }
}

export async function applyLocalToggleValues(values: ToggleValues, keys: readonly string[]): Promise<boolean> {
    try {
        const conversation = captureChatBindingTarget()?.conversation
        if (!conversation || conversationMutationBlockedByGeneration(conversation.id)) return false
        const overrides = { ...conversation.GLGlobalVariables }
        applyToggleValues(overrides, values, keys)
        await updateChatBinding(conversation, { GLGlobalVariables: overrides })
        return true
    } catch (error) {
        reportFailure(error)
        return false
    }
}

export async function removeLocalToggleValue(key: string): Promise<void> {
    try {
        const conversation = captureChatBindingTarget()?.conversation
        if (conversation?.GLGlobalVariables?.[key] === undefined) return
        if (conversationMutationBlockedByGeneration(conversation.id)) return
        const values = { ...conversation.GLGlobalVariables }
        delete values[key]
        await updateChatBinding(conversation, { GLGlobalVariables: values })
    } catch (error) {
        reportFailure(error)
    }
}

export async function setLocalToggleMode(enabled: boolean): Promise<void> {
    try {
        const conversation = captureChatBindingTarget()?.conversation
        if (!conversation || conversationMutationBlockedByGeneration(conversation.id)) return
        await updateChatBinding(conversation, { useLocallySetGlobalVariables: enabled })
    } catch (error) {
        reportFailure(error)
    }
}

export async function setCharacterMemory(owner: character, enabled: boolean): Promise<void> {
    try {
        const saved = await mutateCharacterDetailFields(owner.chaId, 'toggle-character-memory', ({ character }) => {
            character.supaMemory = enabled
        })
        // A character outside the persistent store keeps the in-memory write.
        if (!saved) owner.supaMemory = enabled
    } catch (error) {
        reportFailure(error)
    }
}
