import { expect, it, vi } from 'vitest'
import { captureRoot, deferred, makeDatabase, SaveCoordinator } from './saveCoordinator.testSupport'
import { applyConversationBindingPatch, type ConversationBindingPatch } from './conversationBinding'
import type { PersistentDataStore } from './persistentDataStore'
import { createConversationSummaryStub } from './conversationResidency'

function setup() {
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
    let revision = 1
    const commit = vi.fn(async () => ({ revision: ++revision }))
    const store = {
        readConversation,
        commit,
        readConversationMetadata: vi.fn(async () => ({
            revision,
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
    return { character, other, readConversation, commit, coordinator, store }
}

it('freezes binding values and explicit deletions through reads, commit and publication', async () => {
    const { character, other, readConversation, commit, coordinator, store } = setup()
    const readStarted = deferred<void>()
    const readFinished = deferred<void>()
    vi.mocked(store.readConversationMetadata).mockImplementationOnce(async () => {
        readStarted.resolve()
        await readFinished.promise
        return {
            revision: 1,
            value: {
                characterId: character.chaId, conversationId: 'other', totalMessages: 10_000,
                conversation: {
                    id: 'other', name: 'Synthetic', note: '', localLore: [], bindedPersona: 'old',
                    savedToggleValues: { old: 'value' },
                },
            },
        }
    })
    const commitStarted = deferred<void>()
    const commitFinished = deferred<{ revision: number }>()
    commit.mockImplementationOnce(() => {
        commitStarted.resolve()
        return commitFinished.promise
    })
    const patch: ConversationBindingPatch = {
        bindedPersona: undefined,
        savedToggleValues: { toggle_a: 'captured' },
    }
    const captured = structuredClone(patch)
    const publish = vi.fn((committedPatch: ConversationBindingPatch) => {
        if (committedPatch) applyConversationBindingPatch(other, committedPatch)
    })
    const binding = coordinator.mutateConversationBinding(character.chaId, 'other', patch, publish)
    patch.bindedPersona = 'Changed while queued'
    await readStarted.promise
    patch.savedToggleValues!.toggle_a = 'Changed during read'
    readFinished.resolve()
    await commitStarted.promise
    patch.savedToggleValues!.extra = 'Changed during commit'
    commitFinished.resolve({ revision: 2 })
    await binding

    const written = vi.mocked(store.commit).mock.calls[0][0].conversations![0]
    expect(written).toMatchObject({
        start: 10_000, deleteCount: 0, messages: [],
        conversation: { savedToggleValues: captured.savedToggleValues },
    })
    expect('conversation' in written && written.conversation).not.toHaveProperty('bindedPersona')
    expect(publish).toHaveBeenCalledExactlyOnceWith(captured)
    expect(other.savedToggleValues).toEqual({ toggle_a: 'captured' })
    expect(other).not.toHaveProperty('bindedPersona')
    await coordinator.flushPendingDataLocally('clean-frozen-binding')
    expect(commit).toHaveBeenCalledOnce()
    expect(readConversation).not.toHaveBeenCalled()
    other.savedToggleValues!.toggle_a = 'Later UI edit'
    expect(written).toMatchObject({
        conversation: { savedToggleValues: { toggle_a: 'captured' } },
    })
})

it('writes an inactive persona binding using only metadata and advances the matching baseline', async () => {
    const { character, other, readConversation, commit, coordinator } = setup()
    await coordinator.mutateConversationBinding(character.chaId, 'other', { bindedPersona: 'persona' }, () => {
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

it('stores toggle bindings as metadata and drops the field again when the binding is removed', async () => {
    const { character, other, readConversation, commit, coordinator } = setup()
    const values = { toggle_a: '1' }
    await coordinator.mutateConversationBinding(character.chaId, 'other', { savedToggleValues: values }, () => {
        other.savedToggleValues = values
    })
    await coordinator.mutateConversationBinding(
        character.chaId,
        'other',
        { savedToggleValues: undefined },
        () => {
            delete other.savedToggleValues
        },
    )
    await coordinator.flushPendingData('verify-clean-binding')
    expect(readConversation).not.toHaveBeenCalled()
    expect(commit).toHaveBeenCalledTimes(2)
    const committed = commit.mock.calls.map(
        (call) => (call as unknown as [{ conversations: { conversation: object }[] }])[0].conversations[0].conversation,
    )
    expect(committed[0]).toEqual(expect.objectContaining({ savedToggleValues: { toggle_a: '1' } }))
    expect('savedToggleValues' in committed[1]).toBe(false)
})
