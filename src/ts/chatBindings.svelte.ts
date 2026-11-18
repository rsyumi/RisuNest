import { get } from 'svelte/store'
import { v4 } from 'uuid'
import { DBState, selectedCharID } from './stores.svelte'
import { alertError, alertToast } from './alert'
import { language } from 'src/lang'
import { doingChat } from './process/generationState'
import { activeRerollConversations } from './durableReroll'
import type { Chat } from './storage/database.svelte'
import { cloneConversationMetadata } from './storage/selectedConversationLifecycle'
import { isConversationSummaryStub } from './storage/conversationResidency'
import {
    applyConversationBindingPatch,
    conversationBindingHolds,
    type ConversationBindingPatch,
} from './storage/conversationBinding'
import {
    getPersistentDataRuntime,
    getActiveConversationSession,
    getPersistentNavigationGeneration,
    getPersistentStorageAuthorityEpoch,
    captureSelectedConversationTarget,
    acquireCompleteConversation,
    flushPendingData,
} from './storage/persistentDataRuntime.svelte'
import { SelectedConversationPromotionStaleError } from './storage/activeWorkingSet.svelte'
import { requestScriptDisplayRefresh } from './process/scriptDisplayRefresh'
import { authorNotePresetLinkPatch, linkedAuthorNotePreset } from './authorNotePresets'

/** A binding edit during a generation would discard its response, so it waits for it. */
export function chatBindingBlockedByGeneration(): boolean {
    const character = DBState.db.characters[get(selectedCharID)]
    return conversationMutationBlockedByGeneration(character?.chats[character.chatPage]?.id)
}

export function conversationMutationBlockedByGeneration(conversationId?: string): boolean {
    if (!get(doingChat) && (!conversationId || !get(activeRerollConversations).includes(conversationId))) return false
    alertToast(language.navigationBlockedWhileGenerating)
    return true
}

export function captureChatBindingTarget() {
    const character = DBState.db.characters[get(selectedCharID)]
    const conversation = character?.chats[character.chatPage]
    if (!conversation || isConversationSummaryStub(conversation)) return null
    const navigation = getPersistentNavigationGeneration()
    const currentConversation = () =>
        DBState.db.characters
            .find((item) => item.chaId === character.chaId)
            ?.chats.find((chat) => chat.id === conversation.id)
    return {
        get conversation() {
            return currentConversation() ?? conversation
        },
        isCurrent: () => {
            const selected = DBState.db.characters[get(selectedCharID)]
            const current = selected?.chats[selected.chatPage]
            return (
                navigation === getPersistentNavigationGeneration() &&
                selected?.chaId === character.chaId &&
                current?.id === conversation.id &&
                !isConversationSummaryStub(current)
            )
        },
    }
}

/**
 * Persists persona or toggle bindings of a conversation. A complete selected conversation goes
 * through its session; anything else (a summary stub or the windowed selected shell) is committed
 * as metadata by the save coordinator, which also advances the baseline so the in-memory record
 * stays clean without reading message bodies. With `expected`, the patch is dropped unless the
 * conversation still holds those fields now and, for a coordinator commit, when it is stored.
 */
export async function updateChatBinding(
    conversation: Chat,
    patch: ConversationBindingPatch,
    expected?: ConversationBindingPatch,
): Promise<void> {
    if (expected && !conversationBindingHolds(conversation, expected)) return
    const owner = DBState.db.characters.find((character) => character.chats.includes(conversation))
    const session = getActiveConversationSession()
    if (owner && session?.matchesConversation(owner.chaId, conversation)) {
        const expectedMetadata = cloneConversationMetadata(conversation)
        session.applyOperation({
            expectedVersion: session.version,
            expectedMetadata,
            metadata: { ...expectedMetadata, ...patch },
        })
        return
    }
    if (!owner || !conversation.id) {
        applyConversationBindingPatch(conversation, patch)
        return
    }
    const characterId = owner.chaId
    const conversationId = conversation.id
    await getPersistentDataRuntime().mutateConversationBinding(
        characterId,
        conversationId,
        patch,
        (committedPatch) => {
            const current = DBState.db.characters
                .find((character) => character.chaId === characterId)
                ?.chats.find((chat) => chat.id === conversationId)
            if (current) applyConversationBindingPatch(current, committedPatch)
        },
        expected,
    )
}

export async function bindPersona(conversation: Chat, index: number): Promise<void> {
    const persona = index < 0 ? undefined : DBState.db.personas[index]
    if (index >= 0 && !persona) throw new Error('Persona is no longer available')
    if (persona) persona.id ||= v4()
    await updateChatBinding(conversation, { bindedPersona: persona?.id ?? '' })
}

/** Binds a prompt preset to the conversation by its ID, or unbinds it with null. */
export async function bindPromptPreset(conversation: Chat, presetId: string | null): Promise<void> {
    if (presetId !== null && !DBState.db.botPresets?.some((preset) => preset?.id === presetId)) {
        throw new Error('Preset is no longer available')
    }
    await updateChatBinding(conversation, { promptPresetId: presetId ?? '' })
}

type ChatBindingTarget = NonNullable<ReturnType<typeof captureChatBindingTarget>>

/**
 * Binds the preset picked in the preset list to the captured conversation. Returns false to keep
 * the list open when the conversation is no longer selected, a response is generating, or saving failed.
 */
export async function bindPresetFromList(target: ChatBindingTarget, presetId: string | null): Promise<boolean> {
    if (!target.isCurrent() || chatBindingBlockedByGeneration()) return false
    try {
        await bindPromptPreset(target.conversation, presetId)
        await saveChatBinding()
        return true
    } catch {
        alertError(language.risuNest.presetBinding.saveFailed)
        return false
    }
}

export async function saveChatBinding(): Promise<void> {
    await flushPendingData('chat-binding')
}

/** Rewrites the stored note of the selected conversation to its linked preset, for readers that do not resolve presets. */
export async function refreshAuthorNoteCopy(): Promise<void> {
    const target = captureChatBindingTarget()
    if (!target || get(doingChat)) return
    const conversation = target.conversation
    if (conversation.id && get(activeRerollConversations).includes(conversation.id)) return
    const preset = linkedAuthorNotePreset(DBState.db.authorNotePresets, conversation)
    if (!preset || conversation.note === preset.text) return
    // An edit queued before this copy wins: the copy commits only over the note and link read here.
    const read = { note: conversation.note, authorNotePreset: conversation.authorNotePreset }
    await updateChatBinding(conversation, authorNotePresetLinkPatch(preset), read)
    await saveChatBinding()
}

export interface OnboardingTarget {
    readonly characterId: string
    readonly conversationId: string | undefined
    readonly conversation: Chat
    isCurrent(): boolean
}

/**
 * Captures the selected conversation for a character onboarding. It stays current only while
 * the same character and conversation are selected and the library has not been replaced, even
 * by one that reuses the same IDs.
 */
export function captureOnboardingTarget(): OnboardingTarget | null {
    const character = DBState.db.characters[get(selectedCharID)]
    const binding = captureChatBindingTarget()
    if (!character || !binding) return null
    const conversationId = binding.conversation.id
    const authorityEpoch = getPersistentStorageAuthorityEpoch()
    return {
        characterId: character.chaId,
        conversationId,
        get conversation() {
            return binding.conversation
        },
        isCurrent: () => binding.isCurrent() && getPersistentStorageAuthorityEpoch() === authorityEpoch,
    }
}

export type OnboardingFinishResult = 'applied' | 'blocked' | 'stale'

/**
 * Writes the answers to the conversation's chat variables and marks its onboarding as done.
 * `null` answers (Skip) only mark it done. Nothing is written once the target is no longer
 * current, and a running generation blocks the write until it ends.
 */
export async function finishOnboarding(
    target: OnboardingTarget,
    answers: ReadonlyMap<string, string> | null,
): Promise<OnboardingFinishResult> {
    if (conversationMutationBlockedByGeneration(target.conversationId)) return 'blocked'
    if (!target.isCurrent()) return 'stale'
    if (!answers?.size) {
        await updateChatBinding(target.conversation, { onboardingDone: true })
        await saveChatBinding()
        return 'applied'
    }
    const selection = captureSelectedConversationTarget()
    if (!selection) {
        // Without a persistent selection there is no lease to take; a matching session still commits.
        const conversation = target.conversation
        const session = getActiveConversationSession()
        if (session?.isActive && !session.isTransactionActive && session.matchesConversation(target.characterId, conversation)) {
            requestScriptDisplayRefresh({ keys: session.updateChatVariables(answers) })
        } else {
            conversation.scriptstate ??= {}
            for (const [key, value] of answers) conversation.scriptstate['$' + key] = value
        }
        await updateChatBinding(conversation, { onboardingDone: true })
        await saveChatBinding()
        return 'applied'
    }
    if (selection.characterId !== target.characterId || selection.conversationId !== target.conversationId) {
        return 'stale'
    }
    let lease: Awaited<ReturnType<typeof acquireCompleteConversation>>
    try {
        lease = await acquireCompleteConversation('onboarding', selection)
    } catch (error) {
        if (error instanceof SelectedConversationPromotionStaleError) return 'stale'
        throw error
    }
    try {
        if (conversationMutationBlockedByGeneration(target.conversationId)) return 'blocked'
        const conversation = target.conversation
        if (!target.isCurrent() || !lease.session.matchesConversation(target.characterId, conversation)) {
            return 'stale'
        }
        requestScriptDisplayRefresh({ keys: lease.session.updateChatVariables(answers) })
        await updateChatBinding(conversation, { onboardingDone: true })
        await saveChatBinding()
        return 'applied'
    } finally {
        lease.release()
    }
}
