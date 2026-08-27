import { describe, expect, it, vi } from 'vitest'

import {
    isAutoSuggestionRequestIdentityCurrent,
    readConversationSuggestions,
    writeConversationSuggestions,
} from './autoSuggestionMetadata'
import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database } from './storage/database.svelte'
import { createMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
import type { character } from './storage/database.svelte'

describe('auto suggestion metadata', () => {
    it('rejects a same-conversation response after navigation leaves and returns', () => {
        const request = {
            characterIndex: 0,
            chatPage: 0,
            navigationGeneration: 4,
        }

        expect(isAutoSuggestionRequestIdentityCurrent(request, {
            characterIndex: 0,
            chatPage: 0,
            navigationGeneration: 6,
        })).toBe(false)
        expect(isAutoSuggestionRequestIdentityCurrent(request, { ...request })).toBe(true)
    })

    it('reads and updates suggestions on a metadata-only selected conversation', () => {
        const conversation = createMetadataOnlySelectedConversation({
            id: 'conversation-a',
            name: 'Conversation',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'must not be read' }],
            suggestMessages: ['existing'],
        })
        const owner = {
            type: 'character',
            chaId: 'character-a',
            chatPage: 0,
            chats: [conversation],
        } as unknown as character

        expect(readConversationSuggestions(owner)).toEqual(['existing'])
        writeConversationSuggestions(owner.chats[0], null, ['updated'])
        expect(readConversationSuggestions(owner)).toEqual(['updated'])
    })

    it('records selected suggestion metadata through the active session', () => {
        const conversation = {
            id: 'conversation-a',
            name: 'Conversation',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'hello' }],
        } as Chat
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: conversation.id,
            conversation,
            storeRevision: 7,
            onMutation,
        })

        writeConversationSuggestions(conversation, session, ['first', 'second'])

        expect(conversation.suggestMessages).toEqual(['first', 'second'])
        expect(session.version).toBe(1)
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            previousVersion: 0,
            sessionVersion: 1,
            commands: ['update-metadata'],
        }))
    })
})
