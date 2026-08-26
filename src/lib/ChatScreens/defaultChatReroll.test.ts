import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from '../../ts/storage/activeConversationSession'
import type { Chat, Database } from '../../ts/storage/database.svelte'
import { captureConversationMutationTarget } from '../../ts/conversationMutations'
import { handleDefaultChatUnreroll } from './defaultChatReroll'

function createConversation(): Chat {
    return {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: [
            { role: 'user', data: 'input' },
            {
                role: 'char',
                data: 'current precomputed response',
                generationInfo: { generationId: 'generation-a' },
            },
        ],
    }
}

function createCharacter(conversation: Chat): Database['characters'][number] {
    return {
        type: 'character',
        chaId: 'character-a',
        chatPage: 0,
        chats: [conversation],
    } as Database['characters'][number]
}

describe('DefaultChatScreen unreroll handler', () => {
    it('applies a precomputed unreroll before requiring local reroll history', () => {
        const conversation = createConversation()
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })
        const target = captureConversationMutationTarget(character, conversation, session)
        const preUnreroll = vi.fn(() => 'previous precomputed response')

        const result = handleDefaultChatUnreroll({
            target,
            history: null,
            preUnreroll,
        })

        expect(result).toEqual({ type: 'precomputed' })
        expect(preUnreroll).toHaveBeenCalledWith('generation-a')
        expect(conversation.message.at(-1)?.data).toBe('previous precomputed response')
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail'],
        }))
    })
})
