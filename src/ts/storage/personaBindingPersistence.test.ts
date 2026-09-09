import { expect, it, vi } from 'vitest'
import { captureRoot, makeDatabase, SaveCoordinator } from './saveCoordinator.testSupport'
import type { PersistentDataStore } from './persistentDataStore'
import { createConversationSummaryStub } from './conversationResidency'

it('writes an inactive persona binding using only metadata and advances the matching baseline', async () => {
    const database = makeDatabase()
    const character = database.characters[0]
    const other = createConversationSummaryStub({
        id: 'other',
        characterId: character.chaId,
        name: 'Synthetic',
        configuredIndex: 1,
        recentAt: 0,
        messageCount: 10_000,
    })
    character.chats.push(other)
    const readConversation = vi.fn(() => {
        throw new Error('Binding must not read message bodies')
    })
    const commit = vi.fn(async () => ({ revision: 2 }))
    const store = {
        readConversation,
        commit,
        readConversationMetadata: vi.fn(async () => ({
            revision: 1,
            value: {
                characterId: character.chaId,
                conversationId: 'other',
                totalMessages: 10_000,
                conversation: { id: 'other', name: 'Synthetic', note: '', localLore: [] },
            },
        })),
    } as unknown as PersistentDataStore
    const coordinator = new SaveCoordinator({
        store,
        captureRoot: () => captureRoot(database),
        captureSelectedCharacter: () => character,
        replaceDatabase: () => undefined,
    })
    coordinator.initialize(1)
    await coordinator.mutateConversationPersonaBinding(character.chaId, 'other', 'persona', () => {
        other.bindedPersona = 'persona'
    })
    await coordinator.flushPendingData('verify-clean-binding')
    expect(readConversation).not.toHaveBeenCalled()
    expect(commit).toHaveBeenCalledTimes(1)
    expect(commit).toHaveBeenCalledWith(
        expect.objectContaining({
            conversations: [
                expect.objectContaining({
                    start: 10_000,
                    deleteCount: 0,
                    messages: [],
                    conversation: expect.objectContaining({ bindedPersona: 'persona' }),
                }),
            ],
        }),
    )
})
