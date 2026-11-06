import type { Chat } from './database.svelte'

/** Chat-level bindings that only touch conversation metadata. */
export type ConversationBindingPatch = Partial<Pick<
    Chat,
    'bindedPersona' | 'savedToggleValues' | 'useLocallySetGlobalVariables' | 'GLGlobalVariables' | 'onboardingDone'
    | 'note' | 'authorNotePreset' | 'modules'
>>

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

function sameValue(left: unknown, right: unknown): boolean {
    if (left === right) return true
    if (!left || !right || typeof left !== 'object' || typeof right !== 'object') return false
    if (Array.isArray(left) !== Array.isArray(right)) return false
    const leftKeys = Object.keys(left)
    return leftKeys.length === Object.keys(right).length && leftKeys.every((key) =>
        sameValue((left as Record<string, unknown>)[key], (right as Record<string, unknown>)[key]))
}

/** Whether a conversation record still holds every field of `expected`; `undefined` expects the field to be absent. */
export function conversationBindingHolds(target: object, expected: ConversationBindingPatch): boolean {
    return Object.entries(expected).every(([key, value]) =>
        sameValue((target as Record<string, unknown>)[key], value))
}
