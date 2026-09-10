import { get } from 'svelte/store'
import { v4 } from 'uuid'
import { DBState, selectedCharID } from './stores.svelte'
import type { Chat } from './storage/database.svelte'
import { cloneConversationMetadata } from './storage/selectedConversationLifecycle'
import { isConversationSummaryStub } from './storage/conversationResidency'
import {
    applyConversationBindingPatch,
    type ConversationBindingPatch,
} from './storage/conversationBinding'
import {
    getPersistentDataRuntime,
    getActiveConversationSession,
    getPersistentNavigationGeneration,
    flushPendingData,
} from './storage/persistentDataRuntime.svelte'

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
 * stays clean without reading message bodies.
 */
export async function updateChatBinding(
    conversation: Chat,
    patch: ConversationBindingPatch,
): Promise<void> {
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
    await getPersistentDataRuntime().mutateConversationBinding(
        owner.chaId,
        conversation.id,
        patch,
        () => {
            const current = DBState.db.characters
                .find((character) => character.chaId === owner.chaId)
                ?.chats.find((chat) => chat.id === conversation.id)
            if (current) applyConversationBindingPatch(current, patch)
        },
    )
}

export async function bindPersona(conversation: Chat, index: number): Promise<void> {
    const persona = index < 0 ? undefined : DBState.db.personas[index]
    if (index >= 0 && !persona) throw new Error('Persona is no longer available')
    if (persona) persona.id ||= v4()
    await updateChatBinding(conversation, { bindedPersona: persona?.id ?? '' })
}

export async function saveChatBinding(): Promise<void> {
    await flushPendingData('chat-binding')
}
