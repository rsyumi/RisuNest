import { describe, expect, it } from 'vitest'

import {
    readConversationSuggestions,
    writeConversationSuggestions,
} from './autoSuggestionMetadata'
import { createMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'
import type { character } from './storage/database.svelte'

describe('auto suggestion metadata', () => {
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
        writeConversationSuggestions(owner, 0, ['updated'])
        expect(readConversationSuggestions(owner)).toEqual(['updated'])
    })
})
