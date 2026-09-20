import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    getDatabase: vi.fn(),
    readCharacterSummary: vi.fn(),
    readConversationMetadata: vi.fn(),
}))

vi.mock('./database.svelte', () => ({ getDatabase: mocks.getDatabase }))
vi.mock('./persistentDataStoreFactory', () => ({
    getPersistentDataStore: () => ({
        readCharacterSummary: mocks.readCharacterSummary,
        readConversationMetadata: mocks.readConversationMetadata,
    }),
}))

import type { DataHealthFinding } from './dataHealth'
import { dataHealthOwnerKey } from './dataHealthPresentation'
import { resolveDataHealthFindingNames } from './dataHealthNames'

const finding = (kind: string, id: string): DataHealthFinding => ({
    code: 'reference-missing',
    severity: 'degraded',
    owner: { kind, id },
    locator: null,
    target: null,
    detail: '',
})

describe('resolveDataHealthFindingNames', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.getDatabase.mockReturnValue({
            modules: [{ id: 'module-1', name: 'Weather' }],
            characters: [{
                chaId: 'character-1',
                name: 'Mari',
                chats: [{ id: 'chat-1', name: 'First chat' }],
            }],
        })
    })

    it('resolves modules and resident conversations without opening message bodies', async () => {
        const names = await resolveDataHealthFindingNames([
            finding('module', 'module-1'),
            finding('conversation', 'character-1/chat-1'),
        ])
        expect(names.get(dataHealthOwnerKey('module', 'module-1'))).toEqual({
            ownerName: 'Weather',
        })
        expect(names.get(dataHealthOwnerKey('conversation', 'character-1/chat-1'))).toEqual({
            ownerName: 'Mari / First chat',
            characterName: 'Mari',
            conversationName: 'First chat',
        })
        expect(mocks.readConversationMetadata).not.toHaveBeenCalled()
    })

    it('reads only stored summaries when a conversation is not resident', async () => {
        mocks.getDatabase.mockReturnValue({ modules: [], characters: [] })
        mocks.readCharacterSummary.mockResolvedValue({ name: 'Mari' })
        mocks.readConversationMetadata.mockResolvedValue({
            value: { conversation: { name: 'Stored chat' } },
        })
        const names = await resolveDataHealthFindingNames([
            finding('conversation', 'character-1/chat-1'),
        ])
        expect(names.get(dataHealthOwnerKey('conversation', 'character-1/chat-1'))).toEqual({
            ownerName: 'Mari / Stored chat',
            characterName: 'Mari',
            conversationName: 'Stored chat',
        })
        expect(mocks.readCharacterSummary).toHaveBeenCalledWith('character-1')
        expect(mocks.readConversationMetadata).toHaveBeenCalledWith('character-1', 'chat-1')
    })
})
