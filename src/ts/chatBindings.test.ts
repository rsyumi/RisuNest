import { beforeEach, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'
import type { Chat } from './storage/database.svelte'
import { createMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
const state = vi.hoisted(() => ({ db: null as any, navigation: 0 }))
vi.mock('./stores.svelte', () => ({ DBState: state, selectedCharID: writable(0) }))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => null,
    getPersistentNavigationGeneration: () => state.navigation,
    flushPendingData: async () => {},
    getPersistentDataRuntime: () => {
        throw new Error('No full conversation access')
    },
}))
import { bindPersona, captureChatBindingTarget, updateChatBinding } from './chatBindings.svelte'
beforeEach(() => {
    state.navigation = 0
    state.db = {
        selectedPersona: 0,
        username: 'Global persona',
        personas: [
            { id: 'global', name: 'Global persona' },
            { id: 'bound', name: 'Bound persona' },
        ],
        characters: [
            {
                chaId: 'character',
                chatPage: 0,
                chats: [
                    createMetadataOnlySelectedConversation({
                        id: 'chat',
                        name: 'Synthetic',
                        note: '',
                        localLore: [],
                    }),
                ],
            },
        ],
    }
})
it('updates persona and toggle metadata without reading message bodies or changing the global persona', async () => {
    const target = captureChatBindingTarget()!
    expect(() => target.conversation.message).toThrow('metadata-only')
    await bindPersona(target.conversation, 1)
    updateChatBinding(target.conversation, { savedToggleValues: {} })
    expect(target.conversation.bindedPersona).toBe('bound')
    expect(target.conversation.savedToggleValues).toEqual({})
    expect(state.db.selectedPersona).toBe(0)
    expect(state.db.username).toBe('Global persona')
    await bindPersona(target.conversation, -1)
    expect(target.conversation.bindedPersona).toBe('')
})
it('invalidates the captured picker target even after A to B to A navigation', () => {
    const target = captureChatBindingTarget()!
    state.navigation += 2
    expect(target.isCurrent()).toBe(false)
})
it('binds the latest metadata when hydration replaces the same selected conversation', async () => {
    const target = captureChatBindingTarget()!
    const original = target.conversation
    const replacement = createMetadataOnlySelectedConversation({
        id: 'chat',
        name: 'Synthetic',
        note: '',
        localLore: [],
    })
    state.db.characters[0] = { ...state.db.characters[0], chats: [replacement] }
    expect(target.isCurrent()).toBe(true)
    await bindPersona(target.conversation, 1)
    expect(replacement.bindedPersona).toBe('bound')
    expect(original.bindedPersona).toBeUndefined()
})
it('retains an imported persona ID and assigns a missing local ID only once', async () => {
    const chat: Chat = state.db.characters[0].chats[0]
    state.db.personas[1].id = undefined
    await bindPersona(chat, 1)
    const id = chat.bindedPersona
    await bindPersona(chat, 1)
    expect(chat.bindedPersona).toBe(id)
    expect(state.db.personas[0].id).toBe('global')
})
