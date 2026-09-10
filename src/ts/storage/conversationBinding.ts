import type { Chat } from './database.svelte'

/** Chat-level bindings that only touch conversation metadata. */
export type ConversationBindingPatch = Partial<Pick<Chat, 'bindedPersona' | 'savedToggleValues'>>

/** Writes the patch onto a conversation record; `undefined` removes the field instead of storing it. */
export function applyConversationBindingPatch<T extends object>(
    target: T,
    patch: ConversationBindingPatch,
): T {
    for (const [key, value] of Object.entries(patch)) {
        if (value === undefined) delete (target as Record<string, unknown>)[key]
        else (target as Record<string, unknown>)[key] = value
    }
    return target
}
