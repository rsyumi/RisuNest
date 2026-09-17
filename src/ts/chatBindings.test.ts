import { beforeEach, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'
import type { Chat } from './storage/database.svelte'
import type { ConversationBindingPatch } from './storage/conversationBinding'
import { createMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
const state = vi.hoisted(() => ({
    db: null as any,
    navigation: 0,
    bindings: [] as { characterId: string; conversationId: string; patch: unknown }[],
    bindingGate: null as Promise<void> | null,
}))
vi.mock('./stores.svelte', () => ({ DBState: state, selectedCharID: writable(0) }))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => null,
    getPersistentNavigationGeneration: () => state.navigation,
    flushPendingData: async () => {},
    getPersistentDataRuntime: () => ({
        mutateConversationBinding: async (
            characterId: string,
            conversationId: string,
            patch: ConversationBindingPatch,
            publish: (committedPatch: ConversationBindingPatch) => void,
        ) => {
            const committedPatch = structuredClone(patch)
            state.bindings.push({ characterId, conversationId, patch: committedPatch })
            if (state.bindingGate) await state.bindingGate
            publish(committedPatch)
        },
    }),
}))
import { bindPersona, captureChatBindingTarget, updateChatBinding } from './chatBindings.svelte'
beforeEach(() => {
    state.navigation = 0
    state.bindings = []
    state.bindingGate = null
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
it('commits persona and toggle metadata through the coordinator without reading message bodies or changing the global persona', async () => {
    const target = captureChatBindingTarget()!
    expect(() => target.conversation.message).toThrow('metadata-only')
    await bindPersona(target.conversation, 1)
    await updateChatBinding(target.conversation, { savedToggleValues: {} })
    expect(target.conversation.bindedPersona).toBe('bound')
    expect(target.conversation.savedToggleValues).toEqual({})
    expect(state.bindings).toEqual([
        { characterId: 'character', conversationId: 'chat', patch: { bindedPersona: 'bound' } },
        { characterId: 'character', conversationId: 'chat', patch: { savedToggleValues: {} } },
    ])
    expect(state.db.selectedPersona).toBe(0)
    expect(state.db.username).toBe('Global persona')
    await bindPersona(target.conversation, -1)
    expect(target.conversation.bindedPersona).toBe('')
    await updateChatBinding(target.conversation, { savedToggleValues: undefined })
    expect('savedToggleValues' in target.conversation).toBe(false)
})
it('publishes the committed binding patch instead of the caller value changed during saving', async () => {
    let finish!: () => void
    state.bindingGate = new Promise<void>((resolve) => { finish = resolve })
    const target = captureChatBindingTarget()!
    const patch: ConversationBindingPatch = {
        bindedPersona: undefined, savedToggleValues: { toggle_a: 'captured' },
    }
    const updating = updateChatBinding(target.conversation, patch)
    patch.bindedPersona = 'Uncommitted persona'
    patch.savedToggleValues!.toggle_a = 'Uncommitted toggle'
    finish()
    await updating

    expect('bindedPersona' in target.conversation).toBe(false)
    expect(target.conversation.savedToggleValues).toEqual({ toggle_a: 'captured' })
})
it('uses the captured binding target when the original working-set records are replaced', async () => {
    let finish!: () => void
    state.bindingGate = new Promise<void>((resolve) => { finish = resolve })
    const owner = state.db.characters[0]
    const original = owner.chats[0]
    const updating = updateChatBinding(original, { bindedPersona: 'captured-persona' })
    owner.chaId = 'detached-owner'
    original.id = 'detached-chat'
    const replacement = createMetadataOnlySelectedConversation({
        id: 'chat', name: 'Synthetic', note: '', localLore: [],
    })
    state.db.characters[0] = { chaId: 'character', chatPage: 0, chats: [replacement] }
    finish()
    await updating

    expect(replacement.bindedPersona).toBe('captured-persona')
    expect(original.bindedPersona).toBeUndefined()
    expect(state.bindings).toEqual([{
        characterId: 'character', conversationId: 'chat', patch: { bindedPersona: 'captured-persona' },
    }])
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
