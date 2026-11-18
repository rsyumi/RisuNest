import { beforeEach, expect, it, vi } from 'vitest'
import { get, writable } from 'svelte/store'
import type { Chat } from './storage/database.svelte'
import type { ConversationBindingPatch } from './storage/conversationBinding'
import { cloneConversationMetadata, createMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
import { ActiveConversationSession } from './storage/activeConversationSession'
const state = vi.hoisted(() => ({
    db: null as any,
    navigation: 0,
    epoch: 0,
    session: null as unknown,
    selection: null as unknown,
    promotions: 0,
    released: 0,
    flushes: [] as string[],
    bindings: [] as { characterId: string; conversationId: string; patch: unknown; expected?: unknown }[],
    bindingGate: null as Promise<void> | null,
    toast: vi.fn(),
    error: vi.fn(),
}))
vi.mock('./stores.svelte', () => ({
    DBState: state, selectedCharID: writable(0),
    ReloadChatPointer: writable({}), ScriptDisplayRefreshPointer: writable(0),
}))
vi.mock('./alert', () => ({ alertToast: state.toast, alertError: state.error }))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => state.session,
    getPersistentNavigationGeneration: () => state.navigation,
    getPersistentStorageAuthorityEpoch: () => state.epoch,
    captureSelectedConversationTarget: () => state.selection,
    // Promotes the windowed selection to a complete conversation owned by a session, as the working set does.
    acquireCompleteConversation: async (reason: string, target: unknown) => {
        state.promotions++
        const owner = state.db.characters[0]
        const conversation = { ...cloneConversationMetadata(owner.chats[0]), message: [] } as Chat
        owner.chats[0] = conversation
        state.session = new ActiveConversationSession({
            characterId: owner.chaId, conversationId: conversation.id!, conversation, storeRevision: 1,
        })
        return { reason, session: state.session, target, release: () => { state.released++ } }
    },
    flushPendingData: async (reason: string) => { state.flushes.push(reason) },
    getPersistentDataRuntime: () => ({
        mutateConversationBinding: async (
            characterId: string,
            conversationId: string,
            patch: ConversationBindingPatch,
            publish: (committedPatch: ConversationBindingPatch) => void,
            expected?: ConversationBindingPatch,
        ) => {
            const committedPatch = structuredClone(patch)
            state.bindings.push({ characterId, conversationId, patch: committedPatch, ...(expected ? { expected: structuredClone(expected) } : {}) })
            if (state.bindingGate) await state.bindingGate
            publish(committedPatch)
        },
    }),
}))
import { bindPersona, bindPresetFromList, bindPromptPreset, captureChatBindingTarget, captureOnboardingTarget, chatBindingBlockedByGeneration, conversationMutationBlockedByGeneration, finishOnboarding, refreshAuthorNoteCopy, updateChatBinding } from './chatBindings.svelte'
import { authorNoteFingerprint } from './authorNotePresets'
import { doingChat } from './process/generationState'
import { activeRerollConversations } from './durableReroll'
import { DisplayReadRecord, registerDisplayRow } from './process/scriptVariableReads'
import { ReloadChatPointer } from './stores.svelte'
import { language } from 'src/lang'
beforeEach(() => {
    doingChat.set(false)
    activeRerollConversations.set([])
    state.toast.mockClear()
    state.error.mockClear()
    state.navigation = 0
    state.epoch = 0
    state.session = null
    state.selection = null
    state.promotions = 0
    state.released = 0
    state.flushes = []
    state.bindings = []
    state.bindingGate = null
    state.db = {
        selectedPersona: 0,
        username: 'Global persona',
        botPresets: [{ id: 'preset-a', name: 'A' }, { id: 'preset-b', name: 'B' }],
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
it('blocks row mutations for generation and the matching reroll until ownership ends', () => {
    activeRerollConversations.set(['chat'])
    expect(conversationMutationBlockedByGeneration('chat')).toBe(true)
    expect(conversationMutationBlockedByGeneration('other')).toBe(false)
    activeRerollConversations.set([])
    doingChat.set(true)
    expect(conversationMutationBlockedByGeneration('chat')).toBe(true)
    doingChat.set(false)
    expect(conversationMutationBlockedByGeneration('chat')).toBe(false)
    expect(state.toast).toHaveBeenCalledTimes(2)
})
it('rewrites a stale author note copy of the selected chat outside generation only while it is linked', async () => {
    state.db.authorNotePresets = [{ id: 'preset', name: 'Preset', text: 'Current' }]
    const chat = state.db.characters[0].chats[0]
    chat.note = 'stale'
    chat.authorNotePreset = { id: 'preset', fingerprint: authorNoteFingerprint('stale') }
    doingChat.set(true)
    await refreshAuthorNoteCopy()
    expect(state.bindings).toEqual([])
    doingChat.set(false)
    await refreshAuthorNoteCopy()
    await refreshAuthorNoteCopy()
    expect(state.bindings).toEqual([{
        characterId: 'character',
        conversationId: 'chat',
        patch: { note: 'Current', authorNotePreset: { id: 'preset', fingerprint: authorNoteFingerprint('Current') } },
        // The coordinator drops the copy if a write queued before it changed the note or link.
        expected: { note: 'stale', authorNotePreset: { id: 'preset', fingerprint: authorNoteFingerprint('stale') } },
    }])
    expect(state.flushes).toEqual(['chat-binding'])
    chat.note = 'edited by a script'
    state.db.authorNotePresets[0].text = 'Later'
    await refreshAuthorNoteCopy()
    expect(state.bindings).toHaveLength(1)
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

it('binds a prompt preset by ID through the coordinator and unbinds it with an empty ID', async () => {
    const target = captureChatBindingTarget()!
    await bindPromptPreset(target.conversation, 'preset-b')
    expect(target.conversation.promptPresetId).toBe('preset-b')
    await bindPromptPreset(target.conversation, null)
    expect(target.conversation.promptPresetId).toBe('')
    expect(state.bindings.map((binding) => binding.patch)).toEqual([{ promptPresetId: 'preset-b' }, { promptPresetId: '' }])
    await expect(bindPromptPreset(target.conversation, 'removed')).rejects.toThrow()
    expect(state.bindings).toHaveLength(2)
})
it('binds the preset picked in the list to the captured conversation only while it is still selected and idle', async () => {
    const target = captureChatBindingTarget()!
    expect(await bindPresetFromList(target, 'preset-a')).toBe(true)
    expect(state.db.characters[0].chats[0].promptPresetId).toBe('preset-a')
    expect(state.flushes).toEqual(['chat-binding'])

    doingChat.set(true)
    expect(await bindPresetFromList(target, null)).toBe(false)
    doingChat.set(false)
    expect(await bindPresetFromList(target, 'removed')).toBe(false)
    expect(state.error).toHaveBeenCalledWith(language.risuNest.presetBinding.saveFailed)
    state.navigation++
    expect(await bindPresetFromList(target, null)).toBe(false)
    expect(state.db.characters[0].chats[0].promptPresetId).toBe('preset-a')
})
it('blocks binding edits with a notice only while a response is generating', () => {
    state.toast.mockClear()
    expect(chatBindingBlockedByGeneration()).toBe(false)
    expect(state.toast).not.toHaveBeenCalled()
    doingChat.set(true)
    try {
        expect(chatBindingBlockedByGeneration()).toBe(true)
    } finally {
        doingChat.set(false)
    }
    expect(state.toast).toHaveBeenCalledWith(language.navigationBlockedWhileGenerating)
})

const windowedSelection = { characterId: 'character', conversationId: 'chat', navigationGeneration: 0, storeRevision: 1 }
const selectedChat = (): Chat => state.db.characters[0].chats[0]

it('writes onboarding answers through a complete-conversation lease and marks the chat done', async () => {
    state.selection = windowedSelection
    const row = registerDisplayRow(() => 3)
    const reads = new DisplayReadRecord()
    reads.note('route')
    row.track('body', reads)
    try {
        const target = captureOnboardingTarget()!
        await expect(finishOnboarding(target, new Map([['route', 'a'], ['tags', '["x","y"]']]))).resolves.toBe('applied')
        await vi.waitFor(() => expect(get(ReloadChatPointer)).toEqual({ 3: 1 }))
    } finally {
        row.dispose()
    }
    expect(selectedChat().scriptstate).toEqual({ $route: 'a', $tags: '["x","y"]' })
    expect(selectedChat().onboardingDone).toBe(true)
    expect(state.released).toBe(1)
    expect(state.flushes).toEqual(['chat-binding'])
    expect(state.bindings).toEqual([])
})

it('writes onboarding answers onto a chat that has no persistent selection', async () => {
    await expect(finishOnboarding(captureOnboardingTarget()!, new Map([['route', 'a']]))).resolves.toBe('applied')
    expect(selectedChat().scriptstate).toEqual({ $route: 'a' })
    expect(selectedChat().onboardingDone).toBe(true)
    expect(state.bindings).toEqual([
        { characterId: 'character', conversationId: 'chat', patch: { onboardingDone: true } },
    ])
    expect(state.promotions).toBe(0)
    expect(state.flushes).toEqual(['chat-binding'])
})

it('skips onboarding by marking only the chat done without a lease or variables', async () => {
    state.selection = windowedSelection
    await expect(finishOnboarding(captureOnboardingTarget()!, null)).resolves.toBe('applied')
    expect(state.promotions).toBe(0)
    expect(state.bindings).toEqual([
        { characterId: 'character', conversationId: 'chat', patch: { onboardingDone: true } },
    ])
    expect(selectedChat().onboardingDone).toBe(true)
    expect(selectedChat().scriptstate).toBeUndefined()
})

it('writes nothing for onboarding after navigating to another chat', async () => {
    state.selection = windowedSelection
    const target = captureOnboardingTarget()!
    const original = selectedChat()
    state.db.characters[0].chats.push(createMetadataOnlySelectedConversation({ id: 'other', name: 'Other', note: '', localLore: [] }))
    state.db.characters[0].chatPage = 1
    for (const answers of [new Map([['route', 'a']]), null]) {
        await expect(finishOnboarding(target, answers)).resolves.toBe('stale')
    }
    expect(state.promotions).toBe(0)
    expect(state.bindings).toEqual([])
    for (const chat of [original, state.db.characters[0].chats[1]]) {
        expect(chat.onboardingDone).toBeUndefined()
        expect(chat.scriptstate).toBeUndefined()
    }
})

it('writes nothing for onboarding after the library is replaced by one with the same IDs', async () => {
    state.selection = windowedSelection
    const target = captureOnboardingTarget()!
    state.db.characters[0] = {
        chaId: 'character', chatPage: 0,
        chats: [createMetadataOnlySelectedConversation({ id: 'chat', name: 'Replaced', note: '', localLore: [] })],
    }
    state.epoch++
    for (const answers of [new Map([['route', 'a']]), null]) {
        await expect(finishOnboarding(target, answers)).resolves.toBe('stale')
    }
    expect(state.promotions).toBe(0)
    expect(state.bindings).toEqual([])
    expect(selectedChat().onboardingDone).toBeUndefined()
    expect(selectedChat().scriptstate).toBeUndefined()
})

it('holds onboarding answers while a response is generating', async () => {
    state.selection = windowedSelection
    const target = captureOnboardingTarget()!
    doingChat.set(true)
    try {
        await expect(finishOnboarding(target, new Map([['route', 'a']]))).resolves.toBe('blocked')
        await expect(finishOnboarding(target, null)).resolves.toBe('blocked')
    } finally {
        doingChat.set(false)
    }
    expect(state.toast).toHaveBeenCalledWith(language.navigationBlockedWhileGenerating)
    expect(state.promotions).toBe(0)
    expect(state.bindings).toEqual([])
    expect(selectedChat().onboardingDone).toBeUndefined()
})
