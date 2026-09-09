import { get } from 'svelte/store'
import { v4 } from 'uuid'
import { DBState, selectedCharID } from './stores.svelte'
import type { Chat } from './storage/database.svelte'
import { cloneConversationMetadata } from './storage/selectedConversationLifecycle'
import { isConversationSummaryStub } from './storage/conversationResidency'
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
    return {
        conversation,
        isCurrent: () =>
            navigation === getPersistentNavigationGeneration() &&
            DBState.db.characters[get(selectedCharID)]?.chaId === character.chaId &&
            character.chats[character.chatPage]?.id === conversation.id,
    }
}

export function updateChatBinding(
    conversation: Chat,
    patch: Partial<Pick<Chat, 'bindedPersona' | 'savedToggleValues'>>,
): void {
    const session = getActiveConversationSession()
    const character = DBState.db.characters[get(selectedCharID)]
    if (character && session?.matchesConversation(character.chaId, conversation)) {
        const expectedMetadata = cloneConversationMetadata(conversation)
        session.applyOperation({
            expectedVersion: session.version,
            expectedMetadata,
            metadata: { ...expectedMetadata, ...patch },
        })
    } else {
        // Selected metadata-only shells are observed without accessing messages.
        Object.assign(conversation, patch)
    }
}

export async function bindPersona(conversation: Chat, index: number): Promise<void> {
    const persona = index < 0 ? undefined : DBState.db.personas[index]
    if (index >= 0 && !persona) throw new Error('Persona is no longer available')
    if (persona) persona.id ||= v4()
    const personaId = persona?.id ?? ''
    if (isConversationSummaryStub(conversation)) {
        const owner = DBState.db.characters.find((character) => character.chats.includes(conversation))
        if (!owner || !conversation.id) return
        const previous = conversation.bindedPersona
        await getPersistentDataRuntime().mutateConversationPersonaBinding(
            owner.chaId,
            conversation.id,
            personaId,
            () => {
                const current = DBState.db.characters
                    .find((character) => character.chaId === owner.chaId)
                    ?.chats.find((chat) => chat.id === conversation.id)
                if (current && current.bindedPersona === previous)
                    updateChatBinding(current, { bindedPersona: personaId })
            },
        )
    } else updateChatBinding(conversation, { bindedPersona: personaId })
}

export async function saveChatBinding(): Promise<void> {
    await flushPendingData('chat-binding')
}
