import { untrack } from 'svelte'
import type { Database, character, groupChat } from './database.svelte'

interface PersistentSaveObserverDependencies {
    readDatabase(): Database
    readSelectedCharacter(): character | groupChat | null
    markDirty(estimatedChangedBytes: number): void
}

/** Subscribe to arbitrary compatibility mutations without copying their values. */
function subscribeDeep(value: unknown): void {
    if (value === null || typeof value !== 'object') return
    if (Array.isArray(value)) {
        for (let index = 0; index < value.length; index++) subscribeDeep(value[index])
        return
    }
    for (const key in value as Record<string, unknown>) {
        subscribeDeep((value as Record<string, unknown>)[key])
    }
}

export function observePersistentSaveChanges(
    dependencies: PersistentSaveObserverDependencies,
): () => void {
    return $effect.root(() => {
        $effect(() => {
            const database = dependencies.readDatabase()
            for (const key in database) {
                if (key !== 'characters') {
                    $effect(() => {
                        subscribeDeep(database[key])
                        untrack(() => dependencies.markDirty(0))
                    })
                }
            }
            // A deep observer knows that something changed, not the byte size
            // of that change. The complete root size would force an immediate
            // save for every keystroke whenever an unchanged payload is large.
            untrack(() => dependencies.markDirty(0))
        })
        $effect(() => {
            const character = dependencies.readSelectedCharacter()
            if (character) {
                for (const key in character) {
                    if (key !== 'chats') subscribeDeep(character[key])
                }
            }
            // Coordinator reads must not expand either observer's dependencies.
            untrack(() => dependencies.markDirty(0))
        })
        $effect(() => {
            const chats = dependencies.readSelectedCharacter()?.chats ?? []
            // The parent observes collection identity/length. Per-slot effects
            // follow replacements and reorders without walking sibling history
            // when one conversation changes.
            for (let index = 0; index < chats.length; index++) {
                $effect(() => {
                    const chat = chats[index]
                    for (const key in chat) {
                        if (key !== 'message') subscribeDeep(chat[key])
                    }
                    untrack(() => dependencies.markDirty(0))
                })
                $effect(() => {
                    const chat = chats[index]
                    // Metadata-only selected shells deliberately expose a
                    // non-enumerable message getter which must not be invoked.
                    for (const key in chat) {
                        if (key === 'message') {
                            const messages = chat.message
                            if (Array.isArray(messages)) {
                                for (let index = 0; index < messages.length; index++) {
                                    $effect(() => {
                                        subscribeDeep(messages[index])
                                        untrack(() => dependencies.markDirty(0))
                                    })
                                }
                            } else subscribeDeep(messages)
                        }
                    }
                    untrack(() => dependencies.markDirty(0))
                })
            }
            untrack(() => dependencies.markDirty(0))
        })
    })
}
