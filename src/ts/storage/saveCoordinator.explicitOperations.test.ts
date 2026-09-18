import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
import { describe, expect, it, vi } from 'vitest'
import {
    PersistentRootModuleAppendRejectedError,
    canonicalJson,
    type PersistentConversationReplacementResult,
    type PersistentCharacterMutationState,
    type PersistentRootModuleAppend,
} from './saveCoordinator'
import type { Chat, Database, character, groupChat } from './database.svelte'
import type { PersistentDataStore, WorkingSetCommit } from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import { createPluginStorageStore } from '../plugins/pluginStorageStore'
import { createConversationSummaryStubFromChat } from './conversationResidency'
import {
    captureRoot,
    deferred,
    makeDatabase,
    makeStore,
    SaveCoordinator,
} from './saveCoordinator.testSupport'

describe('SaveCoordinator replacement commit boundary', () => {
    function replacementHarness(initial: Database = makeDatabase()) {
        let database = initial
        let revision = 7
        const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => {
            expect(expectedRevision).toBe(revision)
            return { revision: ++revision }
        })
        const store = makeStore(commit)
        vi.mocked(store.replaceFromDatabase).mockImplementation(async (_database, expectedRevision) => {
            expect(expectedRevision).toBe(revision)
            return { revision: ++revision }
        })
        const publish = vi.fn((value: Database) => { database = structuredClone(value) })
        const onBackgroundError = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0] ?? null,
            replaceDatabase: publish,
            onBackgroundError,
        })
        coordinator.initialize(revision)
        return { coordinator, store, commit, publish, onBackgroundError, database: () => database }
    }

    it('does not reserve the local write queue while replacement preparation is pending', async () => {
        const { coordinator, store, commit, database } = replacementHarness()
        const preparation = deferred<Database>()
        const replacing = coordinator.replacePreparedPersistentDatabase(
            () => preparation.promise, 'slow-preparation',
        )
        const settled = replacing.then(
            (value) => ({ value }),
            (error: unknown) => ({ error }),
        )
        expect(coordinator.hasDestructiveReplacementFence).toBe(false)
        database().username = 'Edit while preparing'
        coordinator.markPersistentDataDirty(10)
        const flushing = coordinator.flushPendingDataLocally('during-preparation')
        try {
            await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
            await flushing
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        } finally {
            preparation.resolve(makeDatabase())
            await Promise.allSettled([flushing, settled])
        }
        expect(await settled).toHaveProperty('error')
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(database().username).toBe('Edit while preparing')
    })

    it('fences new writers synchronously before a direct replacement enters its queue', async () => {
        const { coordinator, store } = replacementHarness()
        const replacing = coordinator.replacePersistentDatabase(makeDatabase(), 'guarded-replacement')
        try {
            expect(coordinator.hasDestructiveReplacementFence).toBe(true)
            expect(() => coordinator.assertPersistentMutationAllowed()).toThrow(/replacement is active/i)
            expect(() => coordinator.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        } finally {
            await replacing
        }
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.hasDestructiveReplacementFence).toBe(false)
    })

    it('rejects a changed preparation snapshot before replacing even without a dirty notification', async () => {
        const { coordinator, store, database } = replacementHarness()
        const preparation = deferred<Database>()
        const replacing = coordinator.replacePreparedPersistentDatabase(
            () => preparation.promise, 'unannounced-prepare-edit',
        )
        const rejected = expect(replacing).rejects.toThrow()
        database().username = 'Unannounced newer edit'
        preparation.resolve(makeDatabase())
        await rejected
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(database().username).toBe('Unannounced newer edit')
        await coordinator.flushPendingDataLocally('preserve-unannounced-edit')
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it.each(['empty', 'removed'] as const)('does not resave the newly selected character after replacing an %s selection', async (selection) => {
        const initial = makeDatabase()
        if (selection === 'empty') initial.characters = []
        const { coordinator, store, database } = replacementHarness(initial)
        const candidate = makeDatabase()
        candidate.characters[0].chaId = 'new-selected-character'
        candidate.characters[0].name = 'Imported selection'
        await coordinator.replacePersistentDatabase(candidate, 'new-selection')
        expect(database().characters[0].chaId).toBe('new-selected-character')
        await coordinator.flushPendingDataLocally('clean-after-replacement')
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(8)
    })

    it('commits once and retains only the refresh guard when projection fails', async () => {
        const { coordinator, store, publish, onBackgroundError } = replacementHarness()
        const failure = new Error('projection unavailable')
        publish.mockImplementationOnce(() => { throw failure })
        await expect(coordinator.replacePersistentDatabase(makeDatabase(), 'failed-projection')).resolves.toEqual({
            kind: 'committed', revision: 8, projection: 'refresh-required',
        })
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.hasDestructiveReplacementFence).toBe(false)
        expect(coordinator.pendingWorkingSetRefreshRevision).toBe(8)
        expect(onBackgroundError).toHaveBeenCalledWith(failure)
        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        await expect(coordinator.flushPendingDataLocally('no-rewrite')).rejects.toThrow(/replacement is active/i)
    })
})

describe('canonical JSON property safety', () => {
    it('preserves JSON-origin own proto keys at every nested level', () => {
        const source = JSON.parse(
            '{"zeta":0,"__proto__":{"nested":{"__proto__":false}},"alpha":""}',
        )

        const canonical = JSON.parse(canonicalJson(source)) as Record<string, unknown>
        const protoValue = canonical.__proto__ as Record<string, unknown>
        const nested = protoValue.nested as Record<string, unknown>

        expect(Object.keys(canonical)).toEqual(['__proto__', 'alpha', 'zeta'])
        expect(Object.hasOwn(canonical, '__proto__')).toBe(true)
        expect(Object.getPrototypeOf(canonical)).toBe(Object.prototype)
        expect(Object.hasOwn(nested, '__proto__')).toBe(true)
        expect(nested.__proto__).toBe(false)
        expect(Object.getPrototypeOf(nested)).toBe(Object.prototype)
        expect(canonical.alpha).toBe('')
        expect(canonical.zeta).toBe(0)
    })
})

function makeGroupDeletionLease(database: Database, revision = 1) {
    const authoritative = structuredClone(database)
    return {
        revision,
        readRoot: vi.fn(async () => ({ revision, value: captureRoot(authoritative) })),
        queryCharacters: vi.fn(async ({ trash }: { trash: boolean }) => ({
            revision,
            items: trash
                ? []
                : authoritative.characters.map((item, configuredIndex) => ({
                    id: item.chaId,
                    name: item.name,
                    configuredIndex,
                    recentAt: 0,
                    trashed: false,
                    conversationCount: item.chats.length,
                    type: item.type,
                })),
        })),
        readCharacter: vi.fn(async (id: string) => {
            const character = authoritative.characters.find((item) => item.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = character
            return { revision, value: detail }
        }),
        release: vi.fn(async () => undefined),
    }
}

function publishGroupDeletion(database: Database, state: any): void {
    Object.assign(database, state.root)
    for (const detail of state.relatedCharacters ?? []) {
        const live = database.characters.find((item) => item.chaId === detail.chaId)
        if (live) Object.assign(live, detail)
    }
    database.characters = database.characters.filter(
        (item) => item.chaId !== state.characterId,
    )
}

describe('SaveCoordinator', () => {
    function makeConversationReplacementHarness() {
        const database = makeDatabase()
        database.characters[0].chats = [
            { id: 'chat-a', name: 'A', message: [{ role: 'user', data: 'a' }] },
            {
                id: 'chat-b', name: 'B',
                message: [{ role: 'user', data: 'b1' }, { role: 'char', data: 'b2' }],
            },
        ] as Chat[]
        const publishConversationReplacement = vi.fn(
            (result: PersistentConversationReplacementResult) => {
                const owner = database.characters.find(
                    (character) => character.chaId === result.characterId,
                )!
                const index = owner.chats.findIndex(
                    (chat) => chat.id === result.conversationId,
                )
                owner.chats[index] = structuredClone(result.conversation)
            },
        )
        const store = {
            readConversation: vi.fn(async (characterId: string, conversationId: string) => {
                const chat = database.characters
                    .find((character) => character.chaId === characterId)
                    ?.chats.find((candidate) => candidate.id === conversationId)
                return chat ? { revision: 7, value: structuredClone(chat) } : null
            }),
            commit: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) =>
                database.characters.find((character) => character.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            publishConversationReplacement,
            onLocalRevision,
        })
        coordinator.initialize(7, database)
        return {
            coordinator,
            database,
            onLocalRevision,
            publishConversationReplacement,
            store,
        }
    }

    it('captures conversation input before queueing and before asynchronous reads', async () => {
        const { coordinator, store, database, publishConversationReplacement } =
            makeConversationReplacementHarness()
        const replacement = structuredClone(database.characters[0].chats[1])
        replacement.name = 'Captured conversation'
        const captured = structuredClone(replacement)
        const readStarted = deferred<void>()
        const readFinished = deferred<void>()
        vi.mocked(store.readConversation).mockImplementationOnce(async () => {
            readStarted.resolve()
            await readFinished.promise
            return { revision: 7, value: structuredClone(database.characters[0].chats[1]) }
        })

        const replacing = coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'captured-conversation', replacement, { expectedRevision: 7 },
        )
        replacement.name = 'Changed while queued'
        replacement.message[0].data = 'Changed while queued'
        await readStarted.promise
        replacement.message.push({ role: 'user', data: 'Changed during read' })
        readFinished.resolve()
        await expect(replacing).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledOnce()
        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            conversations: [expect.objectContaining({
                messages: captured.message,
                conversation: expect.objectContaining({ name: captured.name }),
            })],
        })
        expect(publishConversationReplacement).toHaveBeenCalledWith({
            revision: 8, characterId: 'char-a', conversationId: 'chat-b', conversation: captured,
        })
        await coordinator.flushPendingDataLocally('after-captured-conversation')
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it('does not let a caller promote a queued conversation expected revision', async () => {
        const { coordinator, store, database } = makeConversationReplacementHarness()
        const options = { expectedRevision: 6 }
        const replacing = coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'captured-conversation-revision',
            structuredClone(database.characters[0].chats[1]), options,
        )
        options.expectedRevision = 7

        await expect(replacing).rejects.toBeInstanceOf(RevisionConflictError)
        expect(store.commit).not.toHaveBeenCalled()
        expect(store.readConversation).not.toHaveBeenCalled()
    })

    it('replaces one existing conversation without assigning configuredIndex', async () => {
        const { coordinator, store, database, publishConversationReplacement, onLocalRevision } =
            makeConversationReplacementHarness()
        const replacement = {
            ...structuredClone(database.characters[0].chats[1]),
            name: 'Updated metadata',
            localLore: [{ key: 'memory', content: 'retained' }],
            message: [{ role: 'char', data: 'replacement body' }],
        } as Chat

        await expect(coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', replacement, { expectedRevision: 7 },
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            conversations: [{
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'chat-b',
                start: 0,
                deleteCount: 2,
                messages: replacement.message,
                conversation: expect.objectContaining({
                    id: 'chat-b',
                    name: 'Updated metadata',
                    localLore: replacement.localLore,
                }),
            }],
        })
        expect((vi.mocked(store.commit).mock.calls[0][0] as WorkingSetCommit)
            .conversations![0]).not.toHaveProperty('configuredIndex')
        expect(publishConversationReplacement).toHaveBeenCalledWith({
            revision: 8,
            characterId: 'char-a',
            conversationId: 'chat-b',
            conversation: replacement,
        })
        expect(onLocalRevision).toHaveBeenCalledWith(8)
    })

    it('rejects stale and malformed conversation replacements before commit', async () => {
        const { coordinator, store, database, publishConversationReplacement } =
            makeConversationReplacementHarness()
        const replacement = structuredClone(database.characters[0].chats[1])

        await expect(coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', replacement, { expectedRevision: 6 },
        )).rejects.toBeInstanceOf(RevisionConflictError)
        await expect(coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', { ...replacement, id: 'other' } as Chat,
        )).rejects.toThrow(/ID must remain chat-b/)
        await expect(coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', { ...replacement, message: null } as any,
        )).rejects.toBeInstanceOf(TypeError)
        expect(store.commit).not.toHaveBeenCalled()
        expect(publishConversationReplacement).not.toHaveBeenCalled()
    })

    it('returns false for a missing conversation and does not publish commit failures', async () => {
        const missing = makeConversationReplacementHarness()
        await expect(missing.coordinator.replacePersistentConversation(
            'missing', 'chat-b', 'plugin-chat-set',
            structuredClone(missing.database.characters[0].chats[1]),
        )).resolves.toBe(false)
        expect(missing.store.commit).not.toHaveBeenCalled()

        const failing = makeConversationReplacementHarness()
        const error = new Error('commit failed')
        vi.mocked(failing.store.commit).mockRejectedValueOnce(error)
        await expect(failing.coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set',
            structuredClone(failing.database.characters[0].chats[1]),
        )).rejects.toBe(error)
        expect(failing.publishConversationReplacement).not.toHaveBeenCalled()
    })

    it('commits a frozen conversation once and leaves a later edit for the next flush', async () => {
        const { coordinator, store, database } = makeConversationReplacementHarness()
        const firstCommit = deferred<{ revision: number }>()
        vi.mocked(store.commit)
            .mockReturnValueOnce(firstCommit.promise)
            .mockResolvedValueOnce({ revision: 9 })
        const replacement = {
            ...structuredClone(database.characters[0].chats[1]),
            name: 'Plugin replacement',
        } as Chat

        const mutation = coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', replacement,
        )
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledOnce())
        database.characters[0].chats[1].name = 'Resident won'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 8 })

        await expect(mutation).resolves.toBe(true)
        expect(store.commit).toHaveBeenCalledOnce()
        expect(database.characters[0].chats[1].name).toBe('Resident won')
        expect(coordinator.hasPendingPersistenceWork).toBe(true)

        await coordinator.flushPendingData('later-conversation-edit')

        expect(store.commit).toHaveBeenCalledTimes(2)
        expect(vi.mocked(store.commit).mock.calls[1][0]).not.toHaveProperty('replaceCharacter')
        expect(vi.mocked(store.commit).mock.calls[1][0].conversations).toEqual([
            expect.objectContaining({
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'chat-b',
                conversation: expect.objectContaining({ name: 'Resident won' }),
            }),
        ])
        expect(database.characters[0].chats[0].name).toBe('A')
    })

    it('preserves unrelated dirty character fields and sibling chats after exact publication', async () => {
        const { coordinator, store, database } = makeConversationReplacementHarness()
        vi.mocked(store.commit).mockImplementationOnce(async () => {
            ;(database.characters[0] as character).desc = 'Concurrent character detail'
            database.characters[0].chats[0].name = 'Concurrent sibling chat'
            coordinator.markPersistentDataDirty(64)
            return { revision: 8 }
        }).mockResolvedValueOnce({ revision: 9 })
        const replacement = {
            ...structuredClone(database.characters[0].chats[1]),
            name: 'Exact target replacement',
        } as Chat

        await coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', replacement,
        )

        expect(coordinator.hasPendingPersistenceWork).toBe(true)
        await coordinator.flushPendingData('persist-unrelated-dirty-state')
        expect(store.commit).toHaveBeenCalledTimes(2)
        expect(vi.mocked(store.commit).mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            desc: 'Concurrent character detail',
            chats: [
                expect.objectContaining({ id: 'chat-a', name: 'Concurrent sibling chat' }),
                expect.objectContaining({ id: 'chat-b', name: 'Exact target replacement' }),
            ],
        })
    })

    it('keeps a nonresident selected sibling summary out of an unrelated root flush', async () => {
        const database = makeDatabase()
        const active = {
            id: 'chat-a', name: 'Active', message: [{ role: 'user', data: 'active' }],
        } as Chat
        let durableSibling = {
            id: 'chat-b', name: 'Sibling', message: [{ role: 'char', data: 'durable sibling' }],
        } as Chat
        database.characters[0].chats = [
            active,
            createConversationSummaryStubFromChat('char-a', durableSibling, 1),
        ]
        let revision = 7
        const commit = vi.fn(async (input: WorkingSetCommit) => {
            revision++
            const exact = input.conversations?.[0]
            if (exact?.type === 'replace-range' && exact.conversationId === 'chat-b') {
                durableSibling = {
                    ...durableSibling,
                    ...exact.conversation,
                    message: structuredClone(exact.messages),
                } as Chat
            }
            return { revision }
        })
        const store = {
            commit,
            readConversation: vi.fn(async (_characterId: string, conversationId: string) => ({
                revision,
                value: structuredClone(conversationId === 'chat-a' ? active : durableSibling),
            })),
            readCharacter: vi.fn(async () => ({
                revision,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({
                revision,
                items: [active, durableSibling].map((chat, configuredIndex) => ({
                    id: chat.id!,
                    characterId: 'char-a',
                    name: chat.name ?? '',
                    configuredIndex,
                    recentAt: 0,
                    messageCount: chat.message.length,
                })),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishConversationReplacement: (result) => {
                database.characters[0].chats[1] = createConversationSummaryStubFromChat(
                    result.characterId,
                    result.conversation,
                    1,
                )
            },
        })
        coordinator.initialize(7, database)
        const replacement = { ...durableSibling, name: 'Updated sibling' }

        await coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', replacement,
        )
        database.username = 'Unrelated root change'
        coordinator.markPersistentDataDirty(32)
        await coordinator.flushPendingData('unrelated-root-change')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toEqual({
            expectedRevision: 8,
            rootMutations: [{ type: 'set', key: 'username', value: 'Unrelated root change' }],
        })
    })

    it('publishes the committed exact conversation revision officially', async () => {
        const database = makeDatabase()
        database.characters[0].chats = [{
            id: 'chat-a', name: 'Before', message: [{ role: 'user', data: 'before' }],
        }] as Chat[]
        const publication = {
            publish: vi.fn(async () => undefined),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn(async () => publication)
        const store = {
            readConversation: vi.fn(async () => ({
                revision: 7,
                value: structuredClone(database.characters[0].chats[0]),
            })),
            commit: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishConversationReplacement: (result) => {
                database.characters[0].chats[0] = structuredClone(result.conversation)
            },
            officialPublisher: { pin },
        })
        coordinator.initialize(7, database)

        await coordinator.replacePersistentConversation(
            'char-a',
            'chat-a',
            'plugin-chat-set',
            { ...structuredClone(database.characters[0].chats[0]), name: 'After' },
        )

        expect(pin).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(8)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        await coordinator.publishCurrentOfficialRevision()
        expect(pin).toHaveBeenCalledWith(8)
        expect(publication.publish).toHaveBeenCalledOnce()
        expect(publication.dispose).toHaveBeenCalledOnce()
    })

    it('retains a failed next flush after a successful exact conversation commit', async () => {
        const { coordinator, store, database } = makeConversationReplacementHarness()
        let revision = 7
        vi.mocked(store.readConversation).mockImplementation(async (
            characterId: string,
            conversationId: string,
        ) => {
            const chat = database.characters
                .find((character) => character.chaId === characterId)
                ?.chats.find((candidate) => candidate.id === conversationId)
            return chat ? { revision, value: structuredClone(chat) } : null
        })
        const flushFailure = new Error('normal flush failed')
        vi.mocked(store.commit).mockImplementationOnce(async () => {
            revision = 8
            database.characters[0].chats[1].name = 'Resident winner'
            coordinator.markPersistentDataDirty(1)
            return { revision }
        }).mockRejectedValueOnce(flushFailure)
        const firstReplacement = {
            ...structuredClone(database.characters[0].chats[1]),
            name: 'First plugin replacement',
        } as Chat

        await expect(coordinator.replacePersistentConversation(
            'char-a', 'chat-b', 'plugin-chat-set', firstReplacement,
        )).resolves.toBe(true)
        expect(coordinator.hasPendingPersistenceWork).toBe(true)

        await expect(coordinator.flushPendingData('failed-later-edit')).rejects.toBe(flushFailure)
        expect(coordinator.hasPendingPersistenceWork).toBe(true)

        vi.mocked(store.commit).mockImplementation(async () => ({ revision: ++revision }))
        await coordinator.flushPendingData('retry-later-edit')

        expect(store.commit).toHaveBeenCalledTimes(3)
        expect(vi.mocked(store.commit).mock.calls[2][0].conversations?.[0]).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'chat-b',
            conversation: expect.objectContaining({ name: 'Resident winner' }),
        })
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it('does not let a caller promote a queued complete-character expected revision', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 7, value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 7, items: [] })),
            commit: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)
        const mutate = vi.fn((current) => current)
        const options = { expectedRevision: 6 }
        const replacing = coordinator.replacePersistentCompleteCharacter(
            'char-a', 'captured-character-revision', mutate, options,
        )
        options.expectedRevision = 7

        await expect(replacing).rejects.toBeInstanceOf(RevisionConflictError)
        expect(store.commit).not.toHaveBeenCalled()
        expect(store.readCharacter).not.toHaveBeenCalled()
        expect(mutate).not.toHaveBeenCalled()
    })

    it('fences complete character replacement with an expected revision', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 7,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 7, items: [] })),
            commit: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'plugin-character-set',
            (current) => current,
            { expectedRevision: 6 },
        )).rejects.toBeInstanceOf(RevisionConflictError)
        expect(store.readRoot).not.toHaveBeenCalled()
        expect(store.commit).not.toHaveBeenCalled()

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'plugin-character-set',
            (current) => ({ ...current, name: 'Updated' }),
            { expectedRevision: 7 },
        )).resolves.toBe(true)
        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            replaceCharacter: expect.objectContaining({ chaId: 'char-a', name: 'Updated' }),
        })
    })

    it('captures a root module and its asset identities before queueing and alias reads', async () => {
        const database = makeDatabase()
        database.modules = []
        const readStarted = deferred<void>()
        const readFinished = deferred<void>()
        const release = vi.fn(async () => undefined)
        const store = {
            commit: vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 })),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetAliasesByKeys: vi.fn(async () => {
                    readStarted.resolve()
                    await readFinished.promise
                    return { revision: 7, value: [] }
                }),
                release,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishRootWorkingSet: (root) => Object.assign(database, root),
        })
        coordinator.initialize(7)
        const input: PersistentRootModuleAppend = {
            module: {
                id: 'captured-module', name: 'Captured module', description: '',
                assets: [['portrait', 'assets/captured.png', 'png']],
            },
            assetAliases: [{
                kind: 'asset', key: 'assets/captured.png', objectHash: 'a'.repeat(64),
                size: 4, mime: 'image/png', name: 'captured.png', ext: 'png',
            }],
            ownerHead: { present: true, manifestHash: 'b'.repeat(64), entryCount: 1 },
        }
        const captured = structuredClone(input)
        const appending = coordinator.appendPersistentRootModule('captured-module', input)
        input.module.name = 'Changed while queued'
        input.assetAliases[0].objectHash = 'c'.repeat(64)
        await readStarted.promise
        input.module.assets![0][1] = 'assets/changed.png'
        input.ownerHead.manifestHash = 'd'.repeat(64)
        readFinished.resolve()
        await appending

        expect(store.commit).toHaveBeenCalledExactlyOnceWith({
            expectedRevision: 7,
            root: expect.objectContaining({ modules: [captured.module] }),
            assetAliases: captured.assetAliases,
            assetOwnerHeads: [{
                owner: { kind: 'root-module-assets', index: 0 }, ...captured.ownerHead,
            }],
        })
        expect(database.modules).toEqual([captured.module])
        expect(release).toHaveBeenCalledOnce()
        await coordinator.flushPendingDataLocally('clean-captured-module')
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it('atomically appends a root module while carrying every occurrence owner head', async () => {
        const database = makeDatabase()
        database.modules = [
            { id: 'duplicate', name: 'First', description: '', assets: [] },
            { id: 'duplicate', name: 'Second', description: '' },
        ]
        database.personas = [
            { name: 'With module', embeddedModule: { id: 'embedded', name: 'Embedded', assets: [] } },
            { name: 'Without module' },
        ] as Database['personas']
        const existingHeads = new Map([
            ['root-module-assets:0', {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: '1'.repeat(64),
                entryCount: 0,
            }],
            ['root-module-assets:1', {
                owner: { kind: 'root-module-assets', index: 1 },
                present: false,
                manifestHash: null,
                entryCount: 0,
            }],
            ['persona-embedded-module-assets:0', {
                owner: { kind: 'persona-embedded-module-assets', index: 0 },
                present: true,
                manifestHash: '2'.repeat(64),
                entryCount: 0,
            }],
        ])
        const readAssetOwnerHead = vi.fn(async (owner: { kind: string; index?: number }) => {
            const head = existingHeads.get(`${owner.kind}:${owner.index}`)
            return head ? { revision: 7, value: structuredClone(head) } : null
        })
        const release = vi.fn(async () => undefined)
        const commit = vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 }))
        const publishRootWorkingSet = vi.fn((root) => Object.assign(database, root))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead,
                readAssetAliasesByKeys: vi.fn(async () => ({ revision: 7, value: [] })),
                release,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet,
        })
        coordinator.initialize(7, database)
        const alias = {
            kind: 'asset' as const,
            key: `assets/${'a'.repeat(64)}.PNG`,
            objectHash: 'a'.repeat(64),
            size: 4,
            mime: '',
            name: '',
            ext: 'PNG',
        }

        await coordinator.appendPersistentRootModule('native-risum-import', {
            module: {
                id: 'new-id',
                name: 'Imported',
                description: '',
                assets: [['same', alias.key, 'PNG']],
            },
            assetAliases: [alias],
            ownerHead: {
                present: true,
                manifestHash: '3'.repeat(64),
                entryCount: 1,
            },
        })

        expect(readAssetOwnerHead.mock.calls.map(([owner]) => owner)).toEqual([
            { kind: 'root-module-assets', index: 0 },
            { kind: 'root-module-assets', index: 1 },
            { kind: 'persona-embedded-module-assets', index: 0 },
        ])
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            root: expect.objectContaining({
                modules: [
                    { id: 'duplicate', name: 'First', description: '', assets: [] },
                    { id: 'duplicate', name: 'Second', description: '' },
                    {
                        id: 'new-id',
                        name: 'Imported',
                        description: '',
                        assets: [['same', alias.key, 'PNG']],
                    },
                ],
            }),
            assetAliases: [alias],
            assetOwnerHeads: [
                existingHeads.get('root-module-assets:0'),
                existingHeads.get('root-module-assets:1'),
                existingHeads.get('persona-embedded-module-assets:0'),
                {
                    owner: { kind: 'root-module-assets', index: 2 },
                    present: true,
                    manifestHash: '3'.repeat(64),
                    entryCount: 1,
                },
            ],
        })
        expect(release).toHaveBeenCalledOnce()
        expect(publishRootWorkingSet).toHaveBeenCalledOnce()
        expect(database.modules.at(-1)).toEqual({
            id: 'new-id',
            name: 'Imported',
            description: '',
            assets: [['same', alias.key, 'PNG']],
        })
        expect(coordinator.revision).toBe(8)
    })

    it('retains a concurrent live root mutation while the module commit awaits', async () => {
        const database = makeDatabase()
        database.modules = []
        let resolveCommit!: (value: { revision: number }) => void
        const commit = vi.fn(() => new Promise<{ revision: number }>((resolve) => {
            resolveCommit = resolve
        }))
        const publishRootWorkingSet = vi.fn((root) => Object.assign(database, root))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet,
        })
        coordinator.initialize(7, database)

        const append = coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.username = 'Concurrent username'
        resolveCommit({ revision: 8 })
        await append

        expect(database.username).toBe('Concurrent username')
        expect(database.modules.at(-1)?.id).toBe('new-id')
    })

    it('returns local module commit success before deferred official publication', async () => {
        const database = makeDatabase()
        database.modules = []
        const scheduled: Array<() => void> = []
        const clock = {
            setTimeout: (callback: () => void) => {
                scheduled.push(callback)
                return callback
            },
            clearTimeout: vi.fn(),
        }
        const pin = vi.fn(async () => { throw new Error('offline') })
        const store = {
            commit: vi.fn(async () => ({ revision: 8 })),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet: (root) => Object.assign(database, root),
            officialPublisher: { pin },
            clock,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).resolves.toBeUndefined()

        expect(coordinator.revision).toBe(8)
        expect(database.modules.at(-1)?.id).toBe('new-id')
        expect(pin).not.toHaveBeenCalled()
        expect(scheduled).toHaveLength(1)
    })

    it.each([
        {
            name: 'revision acquisition',
            acquireRevision: async () => { throw new Error('lease unavailable') },
        },
        {
            name: 'root read',
            acquireRevision: async () => ({
                revision: 7,
                readRoot: vi.fn(async () => { throw new Error('root unavailable') }),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            }),
        },
        {
            name: 'revision release',
            acquireRevision: async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(makeDatabase()) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => { throw new Error('release unavailable') }),
            }),
        },
    ])('classifies $name failure before commit as a known rejected module append', async ({
        acquireRevision,
    }) => {
        const database = makeDatabase()
        database.modules = []
        const commit = vi.fn()
        const coordinator = new SaveCoordinator({
            store: { commit, acquireRevision } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toBeInstanceOf(PersistentRootModuleAppendRejectedError)

        expect(commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('classifies a synchronous precondition failure before queueing as a rejected module append', () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })

        expect(() => coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).toThrow(PersistentRootModuleAppendRejectedError)
    })

    it('classifies a revision conflict from commit as a rejected module append', async () => {
        const database = makeDatabase()
        database.modules = []
        const commitError = new RevisionConflictError(7, 8)
        const store = {
            commit: vi.fn(async () => { throw commitError }),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toMatchObject({
            name: 'PersistentRootModuleAppendRejectedError',
            message: commitError.message,
        })

        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('retains a post-commit callback revision conflict as the exact ambiguous error', async () => {
        const database = makeDatabase()
        database.modules = []
        const callbackError = new RevisionConflictError(7, 8)
        const store = {
            commit: vi.fn(async () => ({ revision: 8 })),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishRootWorkingSet: () => { throw callbackError },
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toBe(callbackError)

        expect(store.commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(8)
    })

    it('retains the exact ambiguous error when commit invocation rejects generically', async () => {
        const database = makeDatabase()
        database.modules = []
        const commitError = new Error('commit response lost')
        const store = {
            commit: vi.fn(async () => { throw commitError }),
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAlias: vi.fn(async () => null),
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await expect(coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toBe(commitError)

        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('preserves cancellation during an alias read and never starts commit', async () => {
        const database = makeDatabase()
        database.modules = []
        const aliasRead = deferred<{ revision: number; value: [] }>()
        const readAssetAliasesByKeys = vi.fn(() => aliasRead.promise)
        const release = vi.fn(async () => undefined)
        const commit = vi.fn()
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAliasesByKeys,
                release,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)
        const controller = new AbortController()
        const reason = new DOMException('cancelled during alias read', 'AbortError')

        const append = coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [{
                kind: 'asset',
                key: `assets/${'a'.repeat(64)}.bin`,
                objectHash: 'a'.repeat(64),
                size: 4,
                mime: '',
                name: '',
                ext: 'bin',
            }],
            ownerHead: { present: true, manifestHash: 'b'.repeat(64), entryCount: 1 },
        }, controller.signal)
        await vi.waitFor(() => expect(readAssetAliasesByKeys).toHaveBeenCalledOnce())
        controller.abort(reason)
        aliasRead.resolve({ revision: 7, value: [] })

        await expect(append).rejects.toBe(reason)
        expect(commit).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(7)
        expect(database.modules).toEqual([])
    })

    it('preserves an existing dirty-save debounce when cancellation is queued', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            database.modules = []
            const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit)
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(7, database)
            database.username = 'Pending dirty edit'
            coordinator.markPersistentDataDirty(1)
            const controller = new AbortController()
            const reason = new RevisionConflictError(7, 8)
            controller.abort(reason)

            await expect(coordinator.appendPersistentRootModule('native-risum-import', {
                module: { id: 'new-id', name: 'Imported', description: '' },
                assetAliases: [],
                ownerHead: { present: false, manifestHash: null, entryCount: 0 },
            }, controller.signal)).rejects.toBe(reason)

            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(
                expect.objectContaining({
                    expectedRevision: 7,
                    rootMutations: [{ type: 'set', key: 'username', value: 'Pending dirty edit' }],
                }),
            )
        } finally {
            vi.useRealTimers()
        }
    })

    it('omits matching aliases to retain metadata and rejects conflicting identities', async () => {
        const database = makeDatabase()
        database.modules = []
        const existing = {
            kind: 'asset' as const,
            key: `assets/${'a'.repeat(64)}.bin`,
            objectHash: 'a'.repeat(64),
            size: 4,
            mime: 'application/octet-stream',
            name: 'retained.bin',
            ext: 'future-extension-metadata',
        }
        const commit = vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 }))
        const readAssetAliasesByKeys = vi.fn(async () => ({ revision: 7, value: [existing] }))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAliasesByKeys,
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)
        const incoming = { ...existing, mime: '', name: '', ext: '.unsafe/path' }

        await coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'new-id', name: 'Imported', description: '' },
            assetAliases: [incoming, incoming],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })

        expect(readAssetAliasesByKeys).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].assetAliases).toEqual([])

        const conflictingReadAssetAliasesByKeys = vi.fn(async () => ({
            revision: 7,
            value: [{ ...existing, objectHash: 'b'.repeat(64) }],
        }))
        const conflictingCoordinator = new SaveCoordinator({
            store: {
                ...store,
                acquireRevision: vi.fn(async () => ({
                    revision: 7,
                    readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                    readAssetOwnerHead: vi.fn(async () => null),
                    readAssetAliasesByKeys: conflictingReadAssetAliasesByKeys,
                    release: vi.fn(async () => undefined),
                })),
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        conflictingCoordinator.initialize(7, database)

        await expect(conflictingCoordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'conflict', name: 'Conflict', description: '' },
            assetAliases: [
                incoming,
                ...Array.from({ length: 512 }, (_, index) => ({
                    ...incoming,
                    key: `assets/conflict-${index}.bin`,
                    objectHash: index.toString(16).padStart(64, '0'),
                })),
            ],
            ownerHead: { present: false, manifestHash: null, entryCount: 0 },
        })).rejects.toThrow(/alias conflicts/i)
        expect(conflictingReadAssetAliasesByKeys).toHaveBeenCalledOnce()
    })

    it('looks up 513 unique aliases in stable batches and commits only missing aliases', async () => {
        const database = makeDatabase()
        database.modules = []
        const aliases = Array.from({ length: 513 }, (_, index) => ({
            kind: 'asset' as const,
            key: `assets/${index.toString(16).padStart(64, '0')}.bin`,
            objectHash: index.toString(16).padStart(64, '0'),
            size: index,
            mime: '',
            name: '',
            ext: 'bin',
        }))
        const existing = {
            ...aliases[0],
            mime: 'application/octet-stream',
            name: 'retained.bin',
            ext: 'future-extension-metadata',
        }
        const commit = vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 }))
        const readAssetAliasesByKeys = vi.fn(async (_kind: 'asset', keys: string[]) => ({
            revision: 7,
            value: keys.includes(existing.key) ? [existing] : [],
        }))
        const store = {
            commit,
            acquireRevision: vi.fn(async () => ({
                revision: 7,
                readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
                readAssetOwnerHead: vi.fn(async () => null),
                readAssetAliasesByKeys,
                release: vi.fn(async () => undefined),
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7, database)

        await coordinator.appendPersistentRootModule('native-risum-import', {
            module: { id: 'batched', name: 'Batched', description: '' },
            assetAliases: [...aliases, aliases[0]],
            ownerHead: { present: true, manifestHash: 'f'.repeat(64), entryCount: 514 },
        })

        expect(readAssetAliasesByKeys.mock.calls.map(([, keys]) => keys)).toEqual([
            aliases.slice(0, 512).map((alias) => alias.key),
            [aliases[512].key],
        ])
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].assetAliases).toEqual(aliases.slice(1))
    })

    it('adopts a successful storage-only revision without changing dirty state or baselines', async () => {
        const database = makeDatabase()
        const commit = vi.fn()
        const onStorageOnlyRevision = vi.fn()
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onStorageOnlyRevision,
            onLocalRevision,
        })
        coordinator.initialize(4, database)
        const generation = coordinator.mutationGeneration
        const authorityEpoch = coordinator.storageAuthorityEpoch

        await coordinator.runStorageOnlyMutation(async (expectedRevision) => {
            expect(expectedRevision).toBe(4)
            return 5
        })

        expect(coordinator.revision).toBe(5)
        expect(coordinator.mutationGeneration).toBe(generation)
        expect(coordinator.storageAuthorityEpoch).toBe(authorityEpoch)
        expect(coordinator.pendingBytes).toBe(0)
        expect(onStorageOnlyRevision).toHaveBeenCalledWith(5)
        expect(onLocalRevision).toHaveBeenCalledWith(5)
        await coordinator.flushPendingData('storage-only-baseline')
        expect(commit).not.toHaveBeenCalled()
    })

    it('does not advance storage-only state when the mutation fails', async () => {
        const database = makeDatabase()
        const error = new Error('cold mutation failed')
        const onStorageOnlyRevision = vi.fn()
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onStorageOnlyRevision,
            onLocalRevision,
        })
        coordinator.initialize(4, database)
        const generation = coordinator.mutationGeneration

        await expect(coordinator.runStorageOnlyMutation(async () => {
            throw error
        })).rejects.toBe(error)

        expect(coordinator.revision).toBe(4)
        expect(coordinator.mutationGeneration).toBe(generation)
        expect(coordinator.pendingBytes).toBe(0)
        expect(onStorageOnlyRevision).not.toHaveBeenCalled()
        expect(onLocalRevision).not.toHaveBeenCalled()
    })

    it('serializes storage-only mutation after an ordinary save without losing either revision', async () => {
        const database = makeDatabase()
        const committed = deferred<{ revision: number }>()
        const commit = vi.fn(() => committed.promise)
        const storageMutation = vi.fn(async (expectedRevision: number) => expectedRevision + 1)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4, database)
        database.username = 'Ordinary save'
        coordinator.markPersistentDataDirty(1)

        const flushing = coordinator.flushPendingData('ordinary-before-cold')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        const storing = coordinator.runStorageOnlyMutation(storageMutation)
        expect(storageMutation).not.toHaveBeenCalled()

        committed.resolve({ revision: 5 })
        await flushing
        await storing

        expect(commit).toHaveBeenCalledWith(expect.objectContaining({ expectedRevision: 4 }))
        expect(storageMutation).toHaveBeenCalledWith(5)
        expect(coordinator.revision).toBe(6)
    })

    it('commits captured plugin storage mutations atomically without putting values in root', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { alpha: 'old', removed: true }
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.pluginCustomStorage.alpha = 'new'
        database.pluginCustomStorage.beta = { nested: true }
        delete database.pluginCustomStorage.removed
        database.username = 'Root changed too'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('plugin-storage')

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            rootMutations: [{ type: 'set', key: 'username', value: 'Root changed too' }],
            pluginStorage: [
                { type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'removed' },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'alpha', value: 'new' },
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'beta', value: { nested: true } },
            ],
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('root')
        expect(commit.mock.calls[0][0].rootMutations).not.toContainEqual(
            expect.objectContaining({ key: 'pluginCustomStorage' }),
        )
    })

    it('does not clear plugin storage when the scalable working set omits it', async () => {
        const database = makeDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.username = 'Scalable edit'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('scalable-plugin-storage')

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            rootMutations: [{ type: 'set', key: 'username', value: 'Scalable edit' }],
        })
    })

    it('captures plugin mutations before queueing and preserves the queued revision order', async () => {
        const database = makeDatabase()
        const firstStarted = deferred<void>()
        const firstFinished = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => ({
            revision: expectedRevision + 1,
        })).mockImplementationOnce(() => {
            firstStarted.resolve()
            return firstFinished.promise
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7)
        const first = coordinator.mutatePersistentPluginStorage('first-plugin-write', [
            { type: 'set', owner: 'plugin-first', key: 'first', value: true },
        ])
        await firstStarted.promise
        const mutations = [
            { type: 'set' as const, owner: 'plugin-second', key: 'second', value: { nested: ['captured'] } },
        ]
        const captured = structuredClone(mutations)
        const second = coordinator.mutatePersistentPluginStorage('second-plugin-write', mutations)
        mutations[0].owner = 'changed-owner'
        mutations[0].key = 'changed-key'
        mutations[0].value.nested[0] = 'changed-value'
        mutations.push({ type: 'set', owner: 'extra-owner', key: 'extra', value: { nested: [] } })
        firstFinished.resolve({ revision: 8 })
        await Promise.all([first, second])

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenNthCalledWith(2, {
            expectedRevision: 8,
            pluginStorage: captured,
        })
        expect(coordinator.revision).toBe(9)
        await coordinator.flushPendingDataLocally('after-captured-plugin-write')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('serializes explicit plugin mutations through revision CAS', async () => {
        const database = makeDatabase()
        const committed = deferred<{ revision: number }>()
        const commit = vi.fn(() => committed.promise)
        const onLocalRevision = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onLocalRevision,
        })
        coordinator.initialize(7)

        const mutation = coordinator.mutatePersistentPluginStorage('v3-plugin-storage', [
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'alpha', value: { large: true } },
        ])
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            pluginStorage: [
                { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'alpha', value: { large: true } },
            ],
        })
        expect(coordinator.revision).toBe(7)

        committed.resolve({ revision: 8 })
        await mutation

        expect(coordinator.revision).toBe(8)
        expect(onLocalRevision).toHaveBeenCalledWith(8)
    })

    it('persists an undefined plugin value as a deletion in a compatibility working set', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { alpha: 'existing' }
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
            },
        })
        coordinator.initialize(7, database)

        await coordinator.mutatePersistentPluginStorage('undefined-plugin-value', [
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'alpha', value: undefined },
        ])

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            pluginStorage: [{ type: 'delete', owner: UNOWNED_PLUGIN_OWNER, key: 'alpha' }],
        })
        expect(database.pluginCustomStorage).toEqual({})
        await expect(coordinator.flushPendingData('after-undefined')).resolves.toBeUndefined()
        expect(commit).toHaveBeenCalledOnce()
    })

    it('does not hydrate plugin values into an incomplete scalable working set', async () => {
        const database = makeDatabase()
        const publishPluginStorageWorkingSet = vi.fn()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            isIncompleteWorkingSet: () => true,
            publishPluginStorageWorkingSet,
        })
        coordinator.initialize(7, database)

        await coordinator.mutatePersistentPluginStorage('scalable-v3', [
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'large', value: 'external-only' },
        ])

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            pluginStorage: [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'large', value: 'external-only' }],
        })
        expect(publishPluginStorageWorkingSet).not.toHaveBeenCalled()
        expect(database).not.toHaveProperty('pluginCustomStorage')
    })

    it('publishes explicit V3 mutations into a hydrated compatibility working set', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { existing: true }
        const storagePrototype = Object.getPrototypeOf(database.pluginCustomStorage)
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
            },
        })
        coordinator.initialize(2, database)

        await coordinator.mutatePersistentPluginStorage('maximum-compatibility', [
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'added', value: 42 },
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '__proto__', value: 0 },
        ])

        expect(Object.keys(database.pluginCustomStorage)).toEqual([
            'existing',
            'added',
            '__proto__',
        ])
        expect(Object.hasOwn(database.pluginCustomStorage, '__proto__')).toBe(true)
        expect(database.pluginCustomStorage.__proto__).toBe(0)
        expect(Object.getPrototypeOf(database.pluginCustomStorage)).toBe(storagePrototype)
        await coordinator.flushPendingData('already-baselined')
        expect(commit).toHaveBeenCalledOnce()
    })

    it('rebases a later same-key V2 mutation over an in-flight V3 commit', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { shared: 'base' }
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
            },
        })
        coordinator.initialize(3, database)

        const v3Mutation = coordinator.mutatePersistentPluginStorage('v3-race', [
            { type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'shared', value: 'v3-first' },
        ])
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.pluginCustomStorage.shared = 'v2-later'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 4 })
        await v3Mutation

        expect(database.pluginCustomStorage.shared).toBe('v2-later')
        await coordinator.flushPendingData('persist-v2-winner')
        expect(commit).toHaveBeenLastCalledWith({
            expectedRevision: 4,
            pluginStorage: [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'shared', value: 'v2-later' }],
        })
    })

    it('keeps V3 reads coherent with a later same-key V2 mutation after commit publication', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = { shared: 'base' }
        const durableStorage: Record<string, unknown> = { shared: 'base' }
        let revision = 3
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn(async (input) => {
            const result = commit.mock.calls.length === 1
                ? await firstCommit.promise
                : { revision: input.expectedRevision + 1 }
            for (const mutation of input.pluginStorage ?? []) {
                if (mutation.type === 'clear') {
                    for (const key of Object.keys(durableStorage)) delete durableStorage[key]
                } else if (mutation.type === 'delete') {
                    delete durableStorage[mutation.key]
                } else {
                    durableStorage[mutation.key] = structuredClone(mutation.value)
                }
            }
            revision = result.revision
            return result
        })
        const store = {
            ...makeStore(commit),
            open: vi.fn(async () => undefined),
            queryPluginStorage: vi.fn(async () => ({
                revision,
                items: Object.keys(durableStorage).map((key) => ({
                    owner: UNOWNED_PLUGIN_OWNER,
                    key,
                    byteSize: 1,
                })),
            })),
            readPluginStorage: vi.fn(async (_owner: string, key: string) =>
                Object.prototype.hasOwnProperty.call(durableStorage, key)
                    ? { revision, value: structuredClone(durableStorage[key]) }
                    : null),
        } as unknown as PersistentDataStore
        let v3Storage: ReturnType<typeof createPluginStorageStore>
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPluginStorageWorkingSet: (storage) => {
                database.pluginCustomStorage = storage
                v3Storage?.invalidate()
            },
        })
        coordinator.initialize(3, database)
        v3Storage = createPluginStorageStore({
            store,
            getStorageAuthorityEpoch: () => coordinator.storageAuthorityEpoch,
            assertPersistentMutationAllowed: (epoch) => coordinator.assertPersistentMutationAllowed(epoch),
            mutate: (mutations) => coordinator.mutatePersistentPluginStorage(
                'overlapping-v3-v2',
                mutations,
            ),
        }, 100)

        const v3Mutation = v3Storage.forOwner(UNOWNED_PLUGIN_OWNER).setItem('shared', 'v3-first')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.pluginCustomStorage.shared = 'v2-later'
        v3Storage.synchronizeCommittedMutation({
            type: 'set',
            owner: UNOWNED_PLUGIN_OWNER,
            key: 'shared',
            value: 'v2-later',
        })
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 4 })
        await v3Mutation
        await coordinator.flushPendingData('persist-v2-winner')

        expect(durableStorage.shared).toBe('v2-later')
        expect(database.pluginCustomStorage.shared).toBe('v2-later')
        await expect(v3Storage.forOwner(UNOWNED_PLUGIN_OWNER).getItem('shared')).resolves.toBe('v2-later')
    })

    it('commits the complete preset array atomically with a changed root', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)
        database.username = 'Changed with presets'
        database.botPresets = [{ name: 'Resident preset', mainPrompt: 'complete' }] as Database['botPresets']
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('preset-change')

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 2,
            rootMutations: [{ type: 'set', key: 'username', value: 'Changed with presets' }],
            replacePresets: database.botPresets,
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('root')
        expect(vi.mocked(store.commit).mock.calls[0][0].rootMutations).not.toContainEqual(
            expect.objectContaining({ key: 'botPresets' }),
        )
    })

    it('never replaces persisted presets from a partial scalable working set', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Active only', mainPrompt: 'resident' }] as Database['botPresets']
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(5)
        database.username = 'Root edit with a partial preset working set'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('partial-preset-working-set')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 5,
            rootMutations: [
                {
                    type: 'set',
                   
                    key: 'username',
                    value: 'Root edit with a partial preset working set',
                },
            ],
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replacePresets')
    })

    it('serializes an explicit preset mutation and publishes only after its atomic commit', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Projected active' }] as Database['botPresets']
        const committed = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => committed.promise),
            readRoot: vi.fn(async () => ({
                revision: 3,
                value: captureRoot(database),
            })),
            queryPresets: vi.fn(async () => ({
                revision: 3,
                items: [
                    { id: '0', configuredIndex: 0, name: 'First' },
                    { id: '1', configuredIndex: 1, name: 'Second' },
                ],
            })),
            readPreset: vi.fn(async (id: string) => ({
                revision: 3,
                value: { name: id === '0' ? 'First' : 'Second', mainPrompt: id },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet,
        })
        coordinator.initialize(3)

        const mutation = coordinator.mutatePersistentPresets('rename', ({ root, presets }) => {
            root.botPresetsId = 1
            presets[1].name = 'Renamed'
        })
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledTimes(1))

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 3,
            root: expect.objectContaining({ botPresetsId: 1 }),
            replacePresets: [
                { name: 'First', mainPrompt: '0' },
                { name: 'Renamed', mainPrompt: '1' },
            ],
        })
        expect(publishPresetWorkingSet).not.toHaveBeenCalled()

        committed.resolve({ revision: 4 })
        await mutation

        expect(publishPresetWorkingSet).toHaveBeenCalledWith({
            revision: 4,
            root: expect.objectContaining({ botPresetsId: 1 }),
            presets: [
                { name: 'First', mainPrompt: '0' },
                { name: 'Renamed', mainPrompt: '1' },
            ],
        })
        expect(coordinator.revision).toBe(4)
    })

    it('does not publish an explicit preset mutation when the atomic commit fails', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn().mockRejectedValue(new Error('preset commit failed')),
            readRoot: vi.fn(async () => ({ revision: 8, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 8,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 8,
                value: { name: 'First', mainPrompt: 'full' },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet,
        })
        coordinator.initialize(8)

        await expect(coordinator.mutatePersistentPresets('rename', ({ presets }) => {
            presets[0].name = 'Never published'
        })).rejects.toThrow('preset commit failed')

        expect(publishPresetWorkingSet).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(8)
    })

    it('preserves and flushes a maximum preset edit made while explicit commit is pending', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Initial', mainPrompt: 'initial' }] as Database['botPresets']
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 20, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 20,
                items: [{ id: '0', configuredIndex: 0, name: 'Initial' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 20,
                value: { name: 'Initial', mainPrompt: 'initial' },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet,
        })
        coordinator.initialize(20)

        const mutation = coordinator.mutatePersistentPresets('explicit-rename', ({ presets }) => {
            presets[0].name = 'Explicit'
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.botPresets[0].name = 'Concurrent live edit'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 21 })

        await expect(mutation).rejects.toThrow('changed during preset mutation')

        expect(publishPresetWorkingSet).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toEqual({
            expectedRevision: 21,
            replacePresets: [{ name: 'Concurrent live edit', mainPrompt: 'initial' }],
        })
        expect(database.botPresets[0].name).toBe('Concurrent live edit')
        expect(coordinator.revision).toBe(22)
    })

    it('freezes a prepared database as soon as preparation settles while the queue is occupied', async () => {
        let database = makeDatabase()
        const store = makeStore(vi.fn())
        vi.mocked(store.replaceFromDatabase).mockResolvedValue({ revision: 8 })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (value) => { database = structuredClone(value) },
        })
        coordinator.initialize(7)
        const blocked = deferred<number>()
        const blockingStarted = deferred<void>()
        const blocking = coordinator.runStorageOnlyMutation(() => {
            blockingStarted.resolve()
            return blocked.promise
        })
        await blockingStarted.promise
        const preparation = deferred<Database>()
        const prepareStarted = deferred<void>()
        const prepared = makeDatabase()
        prepared.username = 'Prepared snapshot'
        const captured = structuredClone(prepared)
        const replacing = coordinator.replacePreparedPersistentDatabase(() => {
            prepareStarted.resolve()
            return preparation.promise
        }, 'freeze-settled-preparation')
        await prepareStarted.promise
        preparation.resolve(prepared)
        await preparation.promise
        prepared.username = 'Mutated after preparation'
        prepared.characters[0].name = 'Mutated nested result'
        blocked.resolve(7)
        await Promise.all([blocking, replacing])

        expect(store.replaceFromDatabase).toHaveBeenCalledExactlyOnceWith(captured, 7)
        expect(database).toEqual(captured)
        expect(store.commit).not.toHaveBeenCalled()
    })

    it.each(['direct', 'prepared'] as const)(
        'keeps %s replacement options fixed while an earlier write advances the revision',
        async (kind) => {
            const database = makeDatabase()
            const store = makeStore(vi.fn())
            vi.mocked(store.replaceFromDatabase).mockResolvedValue({ revision: 9 })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(7)
            const blocked = deferred<number>()
            const blockingStarted = deferred<void>()
            const blocking = coordinator.runStorageOnlyMutation(() => {
                blockingStarted.resolve()
                return blocked.promise
            })
            await blockingStarted.promise
            const options = { expectedRevision: 7 }
            const replacing = kind === 'direct'
                ? coordinator.replacePersistentDatabase(makeDatabase(), 'fixed-options', options)
                : coordinator.replacePreparedPersistentDatabase(
                    async () => makeDatabase(), 'fixed-options', options,
                )
            const rejected = expect(replacing).rejects.toBeInstanceOf(RevisionConflictError)
            options.expectedRevision = 8
            blocked.resolve(8)
            await Promise.all([blocking, rejected])

            expect(coordinator.revision).toBe(8)
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it.each(['throw', 'reject'] as const)(
        'preserves pending edits and the original error when preparation fails by %s',
        async (kind) => {
            const database = makeDatabase()
            const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit)
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(7)
            database.username = 'Pending local edit'
            coordinator.markPersistentDataDirty(25)
            const error = new Error('preparation failed')
            const prepare = () => {
                if (kind === 'throw') throw error
                return Promise.reject(error)
            }

            await expect(coordinator.replacePreparedPersistentDatabase(
                prepare, 'failed-preparation',
            )).rejects.toBe(error)
            expect(coordinator.revision).toBe(7)
            expect(coordinator.pendingBytes).toBe(25)
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            expect(commit).not.toHaveBeenCalled()
            await coordinator.flushPendingDataLocally('preserved-after-preparation-failure')
            expect(commit).toHaveBeenCalledExactlyOnceWith({
                expectedRevision: 7,
                rootMutations: [{ type: 'set', key: 'username', value: 'Pending local edit' }],
            })
        },
    )

    it('reports preparation failure without waiting for an occupied local write queue', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn())
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)
        const blocked = deferred<number>()
        const blockingStarted = deferred<void>()
        const blocking = coordinator.runStorageOnlyMutation(() => {
            blockingStarted.resolve()
            return blocked.promise
        })
        await blockingStarted.promise
        const preparation = deferred<Database>()
        const replacing = coordinator.replacePreparedPersistentDatabase(
            () => preparation.promise, 'stale-failed-preparation', { expectedRevision: 7 },
        )
        const failure = new Error('preparation failed before the queue became available')
        const rejected = expect(replacing).rejects.toBe(failure)
        preparation.reject(failure)
        await rejected
        await new Promise<void>((resolve) => setTimeout(resolve, 0))
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        blocked.resolve(8)
        await blocking
        expect(coordinator.revision).toBe(8)
        expect(store.commit).not.toHaveBeenCalled()
    })

    it('rejects an incomplete live replacement before cloning or capturing state', async () => {
        const database = makeDatabase()
        const candidate = makeDatabase() as Database & { cloneTrap?: unknown }
        Object.defineProperty(candidate, 'cloneTrap', {
            enumerable: true,
            get() {
                throw new Error('candidate was cloned')
            },
        })
        const captureRootValue = vi.fn(() => captureRoot(database))
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: captureRootValue,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            isIncompleteWorkingSet: (value) => value === candidate,
        })
        coordinator.initialize(1)
        captureRootValue.mockClear()

        await expect(coordinator.replacePersistentDatabase(candidate, 'unsafe-live-snapshot'))
            .rejects.toThrow('incomplete persistent working set')

        expect(captureRootValue).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('allows an explicitly authoritative complete replacement and publishes it normally', async () => {
        const database = makeDatabase()
        const candidate = makeDatabase()
        candidate.username = 'Imported complete database'
        const replaceDatabase = vi.fn()
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 2 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
            isIncompleteWorkingSet: () => true,
        })
        coordinator.initialize(1)
        const initializedAuthorityEpoch = coordinator.storageAuthorityEpoch

        await coordinator.replacePersistentDatabase(candidate, 'explicit-import', {
            authoritative: true,
        })

        expect(store.replaceFromDatabase).toHaveBeenCalledWith(candidate, 1)
        expect(replaceDatabase).toHaveBeenCalledWith(candidate)
        expect(coordinator.revision).toBe(2)
        expect(coordinator.storageAuthorityEpoch).toBe(initializedAuthorityEpoch + 1)

        coordinator.initialize(2, candidate)
        expect(coordinator.storageAuthorityEpoch).toBe(initializedAuthorityEpoch + 2)
    })

    it('rejects a replacement whose expected revision is already stale', async () => {
        const database = makeDatabase()
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(6)

        await expect(coordinator.replacePersistentDatabase(database, 'stale-snapshot', {
            authoritative: true,
            expectedRevision: 5,
        })).rejects.toBeInstanceOf(RevisionConflictError)

        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('rejects a replacement whose snapshot mutation generation is stale before cloning', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const candidate = makeDatabase() as Database & { cloneTrap?: unknown }
            Object.defineProperty(candidate, 'cloneTrap', {
                enumerable: true,
                get() {
                    throw new Error('candidate was cloned')
                },
            })
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit)
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)
            database.username = 'Later live edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.replacePersistentDatabase(candidate, 'stale-snapshot', {
                authoritative: true,
                expectedRevision: 6,
                expectedMutationGeneration: 0,
            })).rejects.toThrow('mutation generation 0')

            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(
                expect.objectContaining({
                    expectedRevision: 6,
                    rootMutations: [{ type: 'set', key: 'username', value: 'Later live edit' }],
                }),
            )
        } finally {
            vi.useRealTimers()
        }
    })

    it('revalidates the snapshot mutation generation after async replacement preparation', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const prepared = deferred<Database>()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)

            const replacement = coordinator.replacePreparedPersistentDatabase(
                () => prepared.promise,
                'deferred-snapshot',
                {
                    authoritative: true,
                    expectedRevision: 6,
                    expectedMutationGeneration: 0,
                },
            )
            await Promise.resolve()
            database.username = 'Edit during preparation'
            coordinator.markPersistentDataDirty(1)
            prepared.resolve(makeDatabase())

            await expect(replacement).rejects.toBeInstanceOf(RevisionConflictError)
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            expect(database.username).toBe('Edit during preparation')
            expect(store.commit).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('re-arms pending dirty data after a replacement store conflict', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = {
                commit,
                replaceFromDatabase: vi.fn().mockRejectedValue(
                    new RevisionConflictError(6, 7),
                ),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)
            database.username = 'Retryable edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.replacePersistentDatabase(
                makeDatabase(),
                'conflicting-replacement',
            )).rejects.toBeInstanceOf(RevisionConflictError)

            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(
                expect.objectContaining({
                    expectedRevision: 6,
                    rootMutations: [{ type: 'set', key: 'username', value: 'Retryable edit' }],
                }),
            )
        } finally {
            vi.useRealTimers()
        }
    })

    it('publishes the frozen character detail rather than a retained mutation callback value', async () => {
        const database = makeDatabase()
        const commitStarted = deferred<void>()
        const commitFinished = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => {
                commitStarted.resolve()
                return commitFinished.promise
            }),
            readRoot: vi.fn(async () => ({ revision: 4, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 4,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            readConversation: vi.fn(),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn((result) => {
            Object.assign(database, result.root)
            Object.assign(database.characters[0], structuredClone(result.character))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(4)
        let retained: PersistentCharacterMutationState | undefined
        const mutating = coordinator.mutatePersistentCharacterDetail(
            'char-a', 'frozen-character-detail', (state) => {
                state.character.name = 'Committed name'
                state.root.username = 'Committed root'
                retained = state
            },
        )
        await commitStarted.promise
        retained!.character.name = 'Uncommitted callback edit'
        retained!.root.username = 'Uncommitted callback root'
        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            root: expect.objectContaining({ username: 'Committed root' }),
            character: expect.objectContaining({ name: 'Committed name' }),
        })
        commitFinished.resolve({ revision: 5 })
        await expect(mutating).resolves.toBe(true)

        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 5,
            root: expect.objectContaining({ username: 'Committed root' }),
            character: expect.objectContaining({ name: 'Committed name' }),
        }))
        expect(database.characters[0].name).toBe('Committed name')
        expect(database.username).toBe('Committed root')
        await coordinator.flushPendingDataLocally('after-frozen-character-detail')
        expect(store.commit).toHaveBeenCalledOnce()
        expect(store.readConversation).not.toHaveBeenCalled()
    })

    it('commits a stable-ID character detail mutation without replacing its conversations', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a', 'unrelated']
        const committed = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => committed.promise),
            readRoot: vi.fn(async () => ({
                revision: 4,
                value: captureRoot(database),
            })),
            readCharacter: vi.fn(async () => ({
                revision: 4,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(4)

        const mutation = coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'trash-character',
            ({ root, character }) => {
                character.trashTime = 123
                root.characterOrder = root.characterOrder.filter((entry) => entry !== 'char-a')
            },
        )
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledOnce())

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            root: expect.objectContaining({ characterOrder: ['unrelated'] }),
            character: expect.objectContaining({
                chaId: 'char-a',
                trashTime: 123,
            }),
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(publishCharacterMutation).not.toHaveBeenCalled()

        committed.resolve({ revision: 5 })
        await expect(mutation).resolves.toBe(true)

        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 5,
            characterId: 'char-a',
            kind: 'detail',
            character: expect.objectContaining({ trashTime: 123 }),
        }))
    })

    it('replaces exactly one complete character from authoritative detail and conversations', async () => {
        const database = makeDatabase()
        const storedChat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'preserved' }],
        }
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 7,
                value: { type: 'character', chaId: 'char-a', name: 'Cold stub' },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 7,
                items: [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Stored chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 7, value: storedChat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(7)

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'character-detail-replace',
            (current) => ({ ...current, name: 'Restored' }),
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Restored',
                chats: [storedChat],
            }),
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 8,
            characterId: 'char-a',
            kind: 'replace',
            character: expect.objectContaining({ chats: [storedChat] }),
        }))
    })

    it('does not publish a stable-ID character mutation when its commit fails', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn().mockRejectedValue(new Error('character commit failed')),
            readRoot: vi.fn(async () => ({ revision: 9, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 9,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(9)

        await expect(coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'delete-character',
            () => ({ delete: true }),
        )).rejects.toThrow('character commit failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(9)
    })

    it('pages group details and commits permanent deletion with every changed group once', async () => {
        const groupA = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        } as groupChat
        const groupB = {
            ...structuredClone(groupA),
            chaId: 'group-b',
            characters: ['char-b', 'char-a'],
            characterTalks: [0.4, 0.6],
            characterActive: [true, false],
        } as groupChat
        const groupTrash = {
            ...structuredClone(groupA),
            chaId: 'group-trash',
            characters: ['char-a'],
            characterTalks: [0.9],
            characterActive: [true],
            trashTime: 100,
        } as groupChat
        const unreferenced = {
            ...structuredClone(groupA),
            chaId: 'group-unreferenced',
            characters: ['char-b'],
            characterTalks: [0.7],
            characterActive: [true],
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characterOrder: ['group-a', 'char-a', 'group-b', 'group-trash'],
            characters: [groupA, groupB, groupTrash, unreferenced, target],
        } as Database
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const readCharacter = vi.fn(async (id: string) => ({
            revision: 1,
            value: structuredClone(database.characters.find((character) => character.chaId === id)),
        }))
        const lease = {
            revision: 1,
            readRoot: vi.fn(async () => ({ revision: 1, value: captureRoot(database) })),
            queryCharacters: vi.fn(async ({ trash, cursor }: { trash: boolean; cursor?: string }) => {
                if (trash) return {
                    revision: 1,
                    items: [{ id: 'group-trash', type: 'group' }],
                }
                if (!cursor) return {
                    revision: 1,
                    items: [
                        { id: 'group-a', type: 'group' },
                        { id: 'group-unreferenced', type: 'group' },
                    ],
                    nextCursor: 'next-page',
                }
                return {
                    revision: 1,
                    items: [
                        { id: 'group-b', type: 'group' },
                        { id: 'char-a', type: 'character' },
                    ],
                }
            }),
            readCharacter,
            release: vi.fn(async () => undefined),
        }
        const store = {
            commit,
            acquireRevision: vi.fn(async () => lease),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn((state) => {
            Object.assign(database, state.root)
            database.characters = database.characters.filter(
                (character) => character.chaId !== state.characterId,
            )
            for (const detail of state.relatedCharacters ?? []) {
                const live = database.characters.find(
                    (character) => character.chaId === detail.chaId,
                )
                if (live) Object.assign(live, detail)
            }
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'permanent-delete',
        )).resolves.toBe(true)

        expect(store.acquireRevision).toHaveBeenCalledWith(1)
        expect(lease.queryCharacters).toHaveBeenCalledTimes(3)
        expect(readCharacter.mock.calls.map(([id]) => id)).toEqual([
            'char-a',
            'group-a',
            'group-unreferenced',
            'group-b',
            'group-trash',
        ])
        expect(lease.release).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 1,
            root: expect.objectContaining({
                characterOrder: ['group-a', 'group-b', 'group-trash'],
            }),
            deleteCharacterId: 'char-a',
            characterDetails: [
                expect.objectContaining({
                    chaId: 'group-a',
                    characters: ['char-b'],
                    characterTalks: [0.75],
                    characterActive: [true],
                }),
                expect.objectContaining({
                    chaId: 'group-b',
                    characters: ['char-b'],
                    characterTalks: [0.4],
                    characterActive: [true],
                }),
                expect.objectContaining({
                    chaId: 'group-trash',
                    characters: [],
                    characterTalks: [],
                    characterActive: [],
                }),
            ],
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 2,
            characterId: 'char-a',
            kind: 'delete',
            relatedCharacters: expect.arrayContaining([
                expect.objectContaining({ chaId: 'group-a' }),
                expect.objectContaining({ chaId: 'group-b' }),
                expect.objectContaining({ chaId: 'group-trash' }),
            ]),
        }))
        expect(database.characters.map((character) => character.chaId)).toEqual([
            'group-a',
            'group-b',
            'group-trash',
            'group-unreferenced',
        ])
        expect(groupA.characters).toEqual(['char-b'])
        expect(groupB.characters).toEqual(['char-b'])
        expect(groupTrash.characters).toEqual([])
        expect(unreferenced.characters).toEqual(['char-b'])
    })

    it('adopts a selected related group baseline without a trailing full-character commit', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characterOrder: ['group-a', 'char-a'],
            characters: [group, target],
        } as Database
        const lease = makeGroupDeletionLease(database)
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => group,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => publishGroupDeletion(database, state),
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'selected-group-delete',
        )).resolves.toBe(true)
        await coordinator.flushPendingData('after-selected-group-delete')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 1,
            deleteCharacterId: 'char-a',
            characterDetails: [expect.objectContaining({
                chaId: 'group-a',
                characters: ['char-b'],
            })],
        })
        expect(group.characters).toEqual(['char-b'])
        expect(coordinator.revision).toBe(2)
    })

    it('serializes a newer scoped upsert after a pending character deletion', async () => {
        const database = makeDatabase()
        const target = structuredClone(database.characters[0])
        const selected = structuredClone(target)
        selected.chaId = 'char-b'
        selected.name = 'Beta'
        database.characterOrder = ['char-a', 'char-b']
        database.characters.push(selected)
        const lease = makeGroupDeletionLease(database)
        const firstCommit = deferred<void>()
        let revision = 1
        let durable: character | groupChat | null = structuredClone(target)
        const commit = vi.fn(async (input: WorkingSetCommit) => {
            if (commit.mock.calls.length === 1) await firstCommit.promise
            if (input.deleteCharacterId === 'char-a') durable = null
            if (input.addCharacter) durable = structuredClone(input.addCharacter)
            return { revision: ++revision }
        })
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                readRoot: vi.fn(async () => ({ revision, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => {
                    if (!durable) return null
                    const { chats: _chats, ...detail } = durable
                    return { revision, value: structuredClone(detail) }
                }),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => selected,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (result) => {
                if (result.kind === 'delete') {
                    publishGroupDeletion(database, result)
                } else if (result.kind === 'add' && result.character) {
                    database.characters.push(structuredClone(result.character as character))
                }
            },
        })
        coordinator.initialize(1, database)

        const deletion = coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'pending-delete',
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        const newer = { ...target, desc: 'Newer scoped value after deletion' } as character
        const upsert = coordinator.upsertPersistentCompleteCharacter(
            'char-a',
            'queued-upsert-after-delete',
            () => newer,
        )
        firstCommit.resolve()

        await expect(deletion).resolves.toBe(true)
        await expect(upsert).resolves.toBe(true)

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 1,
            deleteCharacterId: 'char-a',
        })
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 2,
            addCharacter: expect.objectContaining({
                chaId: 'char-a',
                desc: 'Newer scoped value after deletion',
            }),
        })
        expect(database.characters.find((item) => item.chaId === 'char-a')).toMatchObject({
            desc: 'Newer scoped value after deletion',
        })
        expect(coordinator.revision).toBe(3)
    })

    it('preserves and follows up a pending-commit edit to the selected related group', async () => {
            const group = {
                type: 'group',
                chaId: 'group-a',
                name: 'Group',
                additionalText: 'Initial',
                characters: ['char-a', 'char-b'],
                characterTalks: [0.25, 0.75],
                characterActive: [false, true],
                chats: [],
            } as groupChat
            const target = makeDatabase().characters[0]
            const other = {
                ...structuredClone(target),
                chaId: 'char-b',
                name: 'Beta',
            } as character
            const database = {
                ...makeDatabase(),
                characterOrder: ['group-a', 'char-a', 'char-b'],
                characters: [group, target, other],
            } as Database
            const lease = makeGroupDeletionLease(database)
            const atomicCommit = deferred<{ revision: number }>()
            const commit = vi.fn()
                .mockImplementationOnce(() => atomicCommit.promise)
                .mockImplementationOnce(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const coordinator = new SaveCoordinator({
                store: {
                    acquireRevision: vi.fn(async () => lease),
                    commit,
                } as unknown as PersistentDataStore,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => group,
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
                publishCharacterMutation: (state) => publishGroupDeletion(database, state),
            })
            coordinator.initialize(1)

            const deletion = coordinator.deletePersistentCharacterWithGroupReferences(
                'char-a',
                'pending-related-group-edit',
            )
            await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
            group.additionalText = 'Live edit while atomic commit is pending'
            coordinator.markPersistentDataDirty(1)
            atomicCommit.resolve({ revision: 2 })

            await expect(deletion).resolves.toBe(true)
            await coordinator.flushPendingData('after-related-live-edit')

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 2,
                replaceCharacter: expect.objectContaining({
                    chaId: 'group-a',
                    additionalText: 'Live edit while atomic commit is pending',
                    characters: ['char-b'],
                    characterTalks: [0.75],
                    characterActive: [true],
                }),
            })
            expect(group.additionalText).toBe('Live edit while atomic commit is pending')
            expect(group.characters).toEqual(['char-b'])
            expect(coordinator.revision).toBe(3)
            expect(coordinator.pendingBytes).toBe(0)
    })

    it('reconstructs selected group conversation stubs for the next normal flush', async () => {
        const selectedConversation = {
            id: 'selected-chat',
            name: 'Selected chat',
            message: [{ role: 'user', data: 'selected body', chatId: 'selected-message' }],
        } as groupChat['chats'][number]
        const omittedConversation = {
            id: 'omitted-chat',
            name: 'Omitted chat',
            note: 'Authoritative note',
            localLore: [{ key: 'authoritative lore', content: 'keep' }],
            message: [{ role: 'char', data: 'omitted body', chatId: 'omitted-message' }],
        } as groupChat['chats'][number]
        const omittedStub = createConversationSummaryStubFromChat(
            'group-a',
            omittedConversation,
            1,
        )
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            additionalText: 'Initial',
            characters: ['char-a', 'char-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [selectedConversation, omittedStub],
            chatPage: 0,
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characterOrder: ['group-a', 'char-a'],
            characters: [group, target],
        } as Database
        const lease = makeGroupDeletionLease(database)
        const atomicCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => atomicCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const queryConversations = vi.fn(async () => ({
            revision: 2,
            items: [selectedConversation, omittedConversation].map((conversation, configuredIndex) => ({
                id: conversation.id!,
                characterId: 'group-a',
                name: conversation.name,
                configuredIndex,
                recentAt: 0,
                messageCount: conversation.message.length,
            })),
        }))
        const readConversation = vi.fn(async (_characterId: string, conversationId: string) => ({
            revision: 2,
            value: structuredClone(
                conversationId === selectedConversation.id
                    ? selectedConversation
                    : omittedConversation,
            ),
        }))
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
                queryConversations,
                readConversation,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => group,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => publishGroupDeletion(database, state),
        })
        coordinator.initialize(1)

        const deletion = coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'pending-stubbed-group-edit',
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        group.additionalText = 'Live group edit'
        coordinator.markPersistentDataDirty(1)
        atomicCommit.resolve({ revision: 2 })

        await expect(deletion).resolves.toBe(true)
        await coordinator.flushPendingData('after-stubbed-group-edit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].replaceCharacter.chats).toEqual([
            selectedConversation,
            {
                ...omittedConversation,
                name: omittedStub.name,
                folderId: omittedStub.folderId,
                bindedPersona: omittedStub.bindedPersona,
                lastDate: omittedStub.lastDate,
            },
        ])
        expect(queryConversations).toHaveBeenCalledOnce()
        expect(readConversation).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(3)
    })

    it.each(['stale-lease', 'read-failure'] as const)(
        'releases a failed permanent-delete lease and does not commit for %s',
        async (failure) => {
            const database = makeDatabase()
            const lease = makeGroupDeletionLease(database)
            if (failure === 'stale-lease') lease.revision = 0
            if (failure === 'read-failure') {
                lease.readRoot.mockRejectedValueOnce(new Error('read failed'))
            }
            const commit = vi.fn()
            const coordinator = new SaveCoordinator({
                store: {
                    acquireRevision: vi.fn(async () => lease),
                    commit,
                } as unknown as PersistentDataStore,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => null,
                captureCharacter: () => null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(1)

            await expect(coordinator.deletePersistentCharacterWithGroupReferences(
                'char-a',
                `failed-delete-${failure}`,
            )).rejects.toThrow()

            expect(lease.release).toHaveBeenCalledOnce()
            expect(commit).not.toHaveBeenCalled()
            expect(coordinator.revision).toBe(1)
        },
    )

    it('retries a transient permanent-delete lease release before committing', async () => {
        const database = makeDatabase()
        const lease = makeGroupDeletionLease(database)
        lease.release
            .mockRejectedValueOnce(new Error('release unavailable'))
            .mockResolvedValueOnce(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => publishGroupDeletion(database, state),
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'transient-release-delete',
        )).resolves.toBe(true)

        expect(lease.release).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledOnce()
    })

    it('preserves a permanent-delete read failure when both release attempts fail', async () => {
        const database = makeDatabase()
        const lease = makeGroupDeletionLease(database)
        const primaryError = new Error('read failed')
        lease.readRoot.mockRejectedValueOnce(primaryError)
        lease.release.mockRejectedValue(new Error('release unavailable'))
        const commit = vi.fn()
        const coordinator = new SaveCoordinator({
            store: {
                acquireRevision: vi.fn(async () => lease),
                commit,
            } as unknown as PersistentDataStore,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)

        await expect(coordinator.deletePersistentCharacterWithGroupReferences(
            'char-a',
            'failed-read-and-release-delete',
        )).rejects.toBe(primaryError)

        expect(lease.release).toHaveBeenCalledTimes(2)
        expect(commit).not.toHaveBeenCalled()
    })

    it.each(['detail', 'replace', 'upsert'] as const)(
        'rejects an async %s mutation when its resident character changes before commit',
        async (operation) => {
            const database = makeDatabase()
            const mutationStarted = deferred<void>()
            const mutationGate = deferred<void>()
            const store = {
                commit: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            let mutation: Promise<boolean>
            if (operation === 'detail') {
                mutation = coordinator.mutatePersistentCharacterDetail(
                    'char-a',
                    'async-detail',
                    async ({ character }) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        character.name = 'Explicit detail'
                    },
                )
            } else if (operation === 'replace') {
                mutation = coordinator.replacePersistentCompleteCharacter(
                    'char-a',
                    'async-replace',
                    async (character) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        return { ...character, name: 'Explicit replacement' }
                    },
                )
            } else {
                mutation = coordinator.upsertPersistentCompleteCharacter(
                    'char-a',
                    'async-upsert',
                    async (character) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        return { ...character!, name: 'Explicit upsert' }
                    },
                )
            }
            await mutationStarted.promise
            ;(database.characters[0] as character).desc = 'Later resident edit'
            coordinator.markPersistentDataDirty(1)
            mutationGate.resolve()

            await expect(mutation).rejects.toThrow('Resident character changed')
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it.each(['detail', 'replace', 'upsert'] as const)(
        'rejects a %s mutation when its resident character changes during authoritative reads',
        async (operation) => {
            const database = makeDatabase()
            const characterRead = deferred<{
                revision: number
                value: { type: 'character'; chaId: string; name: string }
            }>()
            const store = {
                commit: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(() => characterRead.promise),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            let mutation: Promise<boolean>
            if (operation === 'detail') {
                mutation = coordinator.mutatePersistentCharacterDetail(
                    'char-a',
                    'read-race-detail',
                    ({ character }) => {
                        character.name = 'Explicit detail'
                    },
                )
            } else if (operation === 'replace') {
                mutation = coordinator.replacePersistentCompleteCharacter(
                    'char-a',
                    'read-race-replace',
                    (character) => ({ ...character, name: 'Explicit replacement' }),
                )
            } else {
                mutation = coordinator.upsertPersistentCompleteCharacter(
                    'char-a',
                    'read-race-upsert',
                    (character) => ({ ...character!, name: 'Explicit upsert' }),
                )
            }
            await vi.waitFor(() => expect(store.readCharacter).toHaveBeenCalledOnce())
            ;(database.characters[0] as character).desc = 'Edit during authoritative read'
            coordinator.markPersistentDataDirty(1)
            characterRead.resolve({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })

            await expect(mutation).rejects.toThrow('Resident character changed')
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it('commits a frozen character once and leaves a later edit for the next flush', async () => {
        const database = makeDatabase()
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) =>
                database.characters.find((character) => character.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(10, database)

        const mutation = coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'pending-replace',
            (character) => ({ ...character, name: 'Explicit replacement' }),
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        ;(database.characters[0] as character).desc = 'Later resident edit'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 11 })

        await expect(mutation).resolves.toBe(true)
        expect(commit).toHaveBeenCalledOnce()
        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(11)
        expect(coordinator.hasPendingPersistenceWork).toBe(true)

        await coordinator.flushPendingData('later-character-edit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 10,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Explicit replacement',
            }),
        })
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 11,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Alpha',
                desc: 'Later resident edit',
            }),
        })
        expect(coordinator.revision).toBe(12)
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it('serializes a newer scoped write to a non-selected resident character', async () => {
        const database = makeDatabase()
        const selected = structuredClone(database.characters[0])
        selected.chaId = 'char-b'
        selected.name = 'Beta'
        database.characters.push(selected)
        let revision = 10
        let durable = structuredClone(database.characters[0])
        const firstCommit = deferred<void>()
        const commit = vi.fn(async (input: WorkingSetCommit) => {
            if (commit.mock.calls.length === 1) await firstCommit.promise
            if (input.replaceCharacter) durable = structuredClone(input.replaceCharacter)
            return { revision: ++revision }
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => {
                const { chats: _chats, ...detail } = durable
                return { revision, value: structuredClone(detail) }
            }),
            queryConversations: vi.fn(async () => ({ revision, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => selected,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (result) => {
                const index = database.characters.findIndex(
                    (item) => item.chaId === result.characterId,
                )
                if (index >= 0 && result.character) {
                    database.characters[index] = structuredClone(result.character as character)
                }
            },
        })
        coordinator.initialize(10, database)

        const first = coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'first-non-selected-write',
            (current) => ({ ...current, name: 'First scoped value' }),
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        const newer = coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'newer-non-selected-write',
            (current) => ({ ...current, name: 'Newer scoped value' }),
        )
        firstCommit.resolve()

        await expect(first).resolves.toBe(true)
        await expect(newer).resolves.toBe(true)

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'First scoped value',
        })
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Newer scoped value',
        })
        expect(database.characters[0].name).toBe('Newer scoped value')
        expect(coordinator.revision).toBe(12)
    })

    it('blocks a selected-target switch while its explicit commit is pending', async () => {
        const database = makeDatabase()
        const other = structuredClone(database.characters[0])
        other.chaId = 'char-b'
        other.name = 'Beta'
        database.characters.push(other)
        let selected = database.characters[0]
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn().mockImplementationOnce(() => firstCommit.promise)
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => selected,
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(10, database)

        const mutation = coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'switch-during-commit',
            (current) => ({ ...current, name: 'Explicit replacement' }),
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        expect(() => coordinator.runSelectedConversationTransition(() => {
            selected = other
        })).toThrow(/pending persistence/i)
        expect(selected.chaId).toBe('char-a')
        firstCommit.resolve({ revision: 11 })

        await expect(mutation).resolves.toBe(true)
        expect(publishCharacterMutation).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(11)
    })

    it('publishes the latest normal flush after a concurrent character edit', async () => {
        const database = makeDatabase()
        let coordinator!: SaveCoordinator
        let revision = 10
        const commit = vi.fn(async () => {
            revision++
            if (revision === 11) {
                ;(database.characters[0] as character).desc = 'Later resident edit'
                coordinator.markPersistentDataDirty(1)
            }
            return { revision }
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
        } as unknown as PersistentDataStore
        const publishedRevisions: number[] = []
        const pin = vi.fn(async (pinnedRevision: number) => ({
            publish: async () => {
                publishedRevisions.push(pinnedRevision)
            },
            dispose: async () => undefined,
        }))
        coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) =>
                database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            officialPublisher: { pin },
        })
        coordinator.initialize(10, database)

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'concurrent-character-edit',
            (current) => ({ ...current, name: 'Explicit replacement' }),
        )).resolves.toBe(true)
        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(11)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)

        await coordinator.flushPendingDataLocally('later-character-edit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(12)
        expect(pin).not.toHaveBeenCalled()
        await coordinator.publishCurrentOfficialRevision()
        expect(pin).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledWith(12)
        expect(publishedRevisions).toEqual([12])
    })
    it('materializes a detached authoritative snapshot without publishing it', async () => {
        const database = makeDatabase()
        const snapshot = makeDatabase()
        snapshot.username = 'Authoritative snapshot'
        const store = {
            materializeDatabase: vi.fn(async (revision?: number) => {
                expect(revision).toBe(12)
                return snapshot
            }),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(12)

        const materialized = await coordinator.materializePersistentDatabaseSnapshot(
            'explicit-compatibility-snapshot',
        )

        expect(materialized).toEqual(snapshot)
        expect(materialized).not.toBe(snapshot)
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(12)
    })

    it('flushes and captures one atomic revision and mutation generation token', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        database.username = 'Flushed before pin'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.capturePersistentMutationToken(
            'sync-conflict-safety-export',
        )).resolves.toEqual({
            revision: 13,
            mutationGeneration: 1,
        })
    })

    it('pins a normal exit revision locally without publishing a pending account revision', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const pin = vi.fn(async () => {
            throw new Error('offline')
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            officialPublisher: { pin },
        })
        coordinator.initialize(12)
        database.username = 'Local edit'
        coordinator.markPersistentDataDirty(1)

        const token = await coordinator.capturePersistentMutationToken(
            'normal-exit-fence',
            { publishOfficial: false },
        )
        const fence = await coordinator.acquireDestructiveReplacementFence(token)

        expect(token.revision).toBe(13)
        expect(coordinator.revision).toBe(13)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        expect(pin).not.toHaveBeenCalled()
        coordinator.releaseDestructiveReplacementFence(fence)
    })

    it('rejects a stale destructive replacement token after flushing the newer live edit', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        const token = await coordinator.capturePersistentMutationToken('native-restore-start')
        database.username = 'Live edit during native parse'
        coordinator.markPersistentDataDirty(1)

        await expect(
            coordinator.acquireDestructiveReplacementFence(token),
        ).rejects.toThrow(/revision|mutation generation/i)

        expect(coordinator.revision).toBe(13)
        expect(() => coordinator.markPersistentDataDirty(1)).not.toThrow()
    })

    it('blocks ordinary saves until the owning destructive fence is released', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage ?? {},
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        const token = await coordinator.capturePersistentMutationToken('native-restore-start')
        const fence = await coordinator.acquireDestructiveReplacementFence(token)

        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(
            /replacement is active/i,
        )
        expect(() => coordinator.flushPendingData('ordinary-save')).toThrow(
            /replacement is active/i,
        )
        expect(() => coordinator.replacePersistentDatabase(
            makeDatabase(),
            'ordinary-replacement',
            { authoritative: true },
        )).toThrow(/replacement is active/i)

        coordinator.initialize(13, database)
        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        expect(coordinator.mutationGeneration).toBe(0)

        database.username = 'Edit after authoritative publication'
        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        expect(coordinator.mutationGeneration).toBe(0)
        expect(() => coordinator.flushPendingData('ordinary-save')).toThrow(
            /replacement is active/i,
        )

        coordinator.releaseDestructiveReplacementFence(fence)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('post-fence-edit')
        expect(store.commit).toHaveBeenCalledWith(
            expect.objectContaining({
                expectedRevision: 13,
                rootMutations: [
                    { type: 'set', key: 'username', value: 'Edit after authoritative publication' },
                ],
            }),
        )
    })

    it('blocks mutation entry synchronously while the exact fence is still acquiring', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage ?? {},
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        const token = await coordinator.capturePersistentMutationToken('native-restore-start')

        const acquiring = coordinator.acquireDestructiveReplacementFence(token)
        expect(() => coordinator.mutatePersistentPresets(
            'queued-preset-mutation',
            () => undefined,
        )).toThrow(/replacement is active/i)
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.username = 'Must not be applied'
        }).toThrow(/replacement is active/i)
        expect(() => coordinator.markPersistentDataDirty(2 * 1024 * 1024)).toThrow(/replacement is active/i)

        const owner = await acquiring
        expect(coordinator.revision).toBe(12)
        expect(database.username).toBe('Fixture')
        expect(store.commit).not.toHaveBeenCalled()
        coordinator.releaseDestructiveReplacementFence(owner)
        expect(() => coordinator.assertPersistentMutationAllowed()).not.toThrow()
    })

    it('returns the snapshot revision atomically before a queued replacement advances it', async () => {
        const database = makeDatabase()
        const snapshot = makeDatabase()
        snapshot.username = 'Revision 12 snapshot'
        const store = {
            materializeDatabase: vi.fn(async () => snapshot),
            queryPluginStorage: vi.fn(async () => ({
                revision: 12,
                items: [
                    { owner: 'plugin-a', key: 'shared', byteSize: 1 },
                    { owner: 'plugin-b', key: 'shared', byteSize: 1 },
                ],
            })),
            readPluginStorage: vi.fn(async (owner: string) => ({
                revision: 12,
                value: owner === 'plugin-a' ? 'a' : 'b',
            })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 13 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)

        const materializing = coordinator.materializePersistentDatabaseSnapshotWithRevision(
            'versioned-snapshot',
            { includePluginStorageValues: true },
        )
        const replacing = coordinator.replacePersistentDatabase(
            makeDatabase(),
            'queued-replacement',
            { authoritative: true },
        )

        await expect(materializing).resolves.toEqual({
            revision: 12,
            mutationGeneration: 0,
            database: snapshot,
            pluginStorageValues: [
                { owner: 'plugin-a', key: 'shared', value: 'a' },
                { owner: 'plugin-b', key: 'shared', value: 'b' },
            ],
        })
        await replacing
        expect(coordinator.revision).toBe(13)
    })

    it('reads one detached complete character without publishing it', async () => {
        const database = makeDatabase()
        const chat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readCharacter: vi.fn(async () => ({
                revision: 13,
                value: { type: 'character', chaId: 'char-a', name: 'Stored detail' },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 13,
                items: [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Stored chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 13, value: chat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(13)

        const character = await coordinator.readPersistentCompleteCharacter(
            'char-a',
            'mcp-character-read',
        )

        expect(character).toEqual(expect.objectContaining({
            chaId: 'char-a',
            chats: [chat],
        }))
        expect(publishCharacterMutation).not.toHaveBeenCalled()
    })

    it('reads one detached authoritative conversation without changing selection', async () => {
        const database = makeDatabase()
        const chat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readConversation: vi.fn(async () => ({ revision: 17, value: chat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(17)

        const conversation = await coordinator.readPersistentConversation(
            'char-a',
            'chat-a',
            'mcp-conversation-read',
        )

        expect(conversation).toEqual(chat)
        expect(conversation).not.toBe(chat)
        expect(store.readConversation).toHaveBeenCalledWith('char-a', 'chat-a')
        expect(publishCharacterMutation).not.toHaveBeenCalled()
    })

    it('reads one ordered-position conversation when configured indexes have gaps', async () => {
        const database = makeDatabase()
        const chat = { id: 'chat-c', name: 'Third', note: '', localLore: [], message: [] }
        const store = {
            queryConversations: vi.fn(async () => ({
                revision: 18,
                items: [{
                    id: 'chat-c',
                    characterId: 'char-a',
                    name: 'Third',
                    configuredIndex: 7,
                    recentAt: 0,
                    messageCount: 0,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 18, value: chat })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(18)

        const conversation = await coordinator.readPersistentConversationAt(
            'char-a',
            2,
            'mcp-selected-conversation-read',
        )

        expect(conversation).toEqual(chat)
        expect(store.queryConversations).toHaveBeenCalledWith({
            characterId: 'char-a',
            order: 'configured',
            limit: 1,
            cursor: '2',
        })
        expect(store.readConversation).toHaveBeenCalledOnce()
    })

    it('reads character detail and its selected conversation in one serialized revision', async () => {
        const database = makeDatabase()
        const firstCharacterRead = deferred<{
            revision: number
            value: Database['characters'][number]
        }>()
        const character = {
            ...database.characters[0],
            chatPage: 1,
        }
        const chat = {
            id: 'chat-b',
            name: 'Selected',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readCharacter: vi.fn()
                .mockImplementationOnce(() => firstCharacterRead.promise)
                .mockResolvedValueOnce({ revision: 19, value: character }),
            queryConversations: vi.fn(async () => ({
                revision: 19,
                items: [{
                    id: 'chat-b',
                    characterId: 'char-a',
                    name: 'Selected',
                    configuredIndex: 4,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 19, value: chat })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(19)

        const selectedRead = coordinator.readPersistentSelectedConversation(
            'char-a',
            'mcp-selected-conversation-read',
        )
        await vi.waitFor(() => expect(store.readCharacter).toHaveBeenCalledOnce())
        const laterRead = coordinator.readPersistentCharacterDetail(
            'char-a',
            'queued-character-read',
        )
        firstCharacterRead.resolve({ revision: 19, value: character })

        const selected = await selectedRead
        await laterRead

        expect(selected).toEqual({ character, conversation: chat })
        expect(selected?.character).not.toBe(character)
        expect(selected?.conversation).not.toBe(chat)
        expect(store.queryConversations).toHaveBeenCalledWith({
            characterId: 'char-a',
            order: 'configured',
            limit: 1,
            cursor: '1',
        })
        expect(vi.mocked(store.queryConversations).mock.invocationCallOrder[0])
            .toBeLessThan(vi.mocked(store.readCharacter).mock.invocationCallOrder[1])
    })

    it('distinguishes an existing character without a selected conversation', async () => {
        const database = makeDatabase()
        const character = { ...database.characters[0], chatPage: 0 }
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 20, value: character })),
            queryConversations: vi.fn(async () => ({ revision: 20, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(20)

        await expect(coordinator.readPersistentSelectedConversation(
            'char-a',
            'mcp-empty-conversation-read',
        )).resolves.toEqual({
            character,
            conversation: null,
        })
    })

    it('returns null only when the selected character is missing', async () => {
        const database = makeDatabase()
        const store = {
            readCharacter: vi.fn(async () => null),
            queryConversations: vi.fn(),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(21)

        await expect(coordinator.readPersistentSelectedConversation(
            'missing',
            'mcp-missing-character-read',
        )).resolves.toBeNull()
        expect(store.queryConversations).not.toHaveBeenCalled()
    })

    it('returns an existing empty group with a null selected conversation', async () => {
        const database = makeDatabase()
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            chatPage: 0,
            chats: [],
            characters: [],
        } as unknown as Database['characters'][number]
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 22, value: group })),
            queryConversations: vi.fn(async () => ({ revision: 22, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(22)

        await expect(coordinator.readPersistentSelectedConversation(
            'group-a',
            'mcp-empty-group-read',
        )).resolves.toEqual({ character: group, conversation: null })
    })

    it('atomically adds an absent complete character before publishing it', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const added = {
            type: 'character',
            chaId: 'temp-char',
            name: 'Temporary',
            chats: [],
        } as Database['characters'][number]
        const store = {
            readRoot: vi.fn(async () => ({ revision: 14, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(14)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'temp-char',
            'multiuser-temp-character',
            (current) => {
                expect(current).toBeNull()
                return added
            },
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 14,
            root: expect.objectContaining({ characterOrder: ['char-a', 'temp-char'] }),
            addCharacter: added,
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 15,
            characterId: 'temp-char',
            kind: 'add',
            character: added,
        }))
    })

    it('captures character insertion and asset options before asynchronous creation', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const store = {
            readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async (_input: WorkingSetCommit) => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => {
                database.characters.push(state.character as Database['characters'][number])
                Object.assign(database, state.root)
            },
        })
        coordinator.initialize(7)
        const options = {
            includeInCharacterOrder: true,
            assetAliases: [{
                kind: 'asset' as const, key: 'assets/captured.png', objectHash: 'a'.repeat(64),
                size: 4, mime: 'image/png', name: 'captured.png', ext: 'png',
            }],
            assetOwnerHeads: [{
                owner: { kind: 'character-additional-assets' as const, characterId: 'captured-char' },
                present: true as const, manifestHash: 'b'.repeat(64), entryCount: 1,
            }],
        }
        const captured = structuredClone(options)
        const creationStarted = deferred<void>()
        const creationFinished = deferred<void>()
        const creating = coordinator.upsertPersistentCompleteCharacter(
            'captured-char', 'captured-character-assets', async () => {
                creationStarted.resolve()
                await creationFinished.promise
                return {
                    type: 'character', chaId: 'captured-char', name: 'Captured', chats: [],
                } as Database['characters'][number]
            }, options,
        )
        options.includeInCharacterOrder = false
        options.assetAliases[0].objectHash = 'c'.repeat(64)
        await creationStarted.promise
        options.assetOwnerHeads[0].manifestHash = 'd'.repeat(64)
        options.assetAliases.push({ ...options.assetAliases[0], key: 'assets/unrequested.png' })
        creationFinished.resolve()
        await expect(creating).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledExactlyOnceWith({
            expectedRevision: 7,
            root: expect.objectContaining({ characterOrder: ['char-a', 'captured-char'] }),
            addCharacter: expect.objectContaining({ chaId: 'captured-char' }),
            assetAliases: captured.assetAliases,
            assetOwnerHeads: captured.assetOwnerHeads,
        })
        expect(database.characterOrder).toEqual(['char-a', 'captured-char'])
        await coordinator.flushPendingDataLocally('clean-captured-assets')
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it('commits prepared character assets and their owner head before publishing the character', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const added = {
            type: 'character',
            chaId: 'prepared-char',
            name: 'Prepared',
            chats: [],
            additionalAssets: [['portrait', 'prepared/portrait.png', 'png']],
        } as Database['characters'][number]
        const assetAliases = [{
            kind: 'asset' as const,
            key: 'prepared/portrait.png',
            objectHash: 'a'.repeat(64),
            size: 4,
            mime: 'image/png',
            name: 'portrait.png',
            ext: 'png',
        }]
        const assetOwnerHeads = [{
            owner: {
                kind: 'character-additional-assets' as const,
                characterId: 'prepared-char',
            },
            present: true as const,
            manifestHash: 'b'.repeat(64),
            entryCount: 1,
        }]
        const events: string[] = []
        const store = {
            readRoot: vi.fn(async () => ({ revision: 23, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async () => {
                events.push('commit')
                expect(database.characters.map((item) => item.chaId)).toEqual(['char-a'])
                return { revision: 24 }
            }),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn((state) => {
            events.push('publish')
            database.characters.push(state.character as Database['characters'][number])
            Object.assign(database, state.root)
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(23)

        await coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => added,
            { assetAliases, assetOwnerHeads },
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 23,
            root: expect.objectContaining({ characterOrder: ['char-a', 'prepared-char'] }),
            addCharacter: added,
            assetAliases,
            assetOwnerHeads,
        })
        expect(events).toEqual(['commit', 'publish'])
        expect(database.characters.map((item) => item.chaId)).toEqual([
            'char-a',
            'prepared-char',
        ])
    })

    it('uses replacement instead of adding a duplicate complete character ID', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 15, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 15,
                value: { type: 'character', chaId: 'char-a', name: 'Existing' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 15, items: [] })),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(15)

        await coordinator.upsertPersistentCompleteCharacter(
            'char-a',
            'multiuser-existing-character',
            (current) => ({ ...current!, name: 'Updated' }),
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 15,
            replaceCharacter: expect.objectContaining({ chaId: 'char-a', name: 'Updated' }),
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('addCharacter')
    })

    it('does not publish an absent character when its atomic add fails', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 16, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn().mockRejectedValue(new Error('add failed')),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(16)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'temp-char',
            'multiuser-temp-character',
            () => ({ type: 'character', chaId: 'temp-char', name: 'Temporary', chats: [] } as any),
        )).rejects.toThrow('add failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(16)
    })

    it('leaves the working set unchanged when prepared character activation fails', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const before = structuredClone(database)
        const store = {
            readRoot: vi.fn(async () => ({ revision: 17, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn().mockRejectedValue(new Error('activation failed')),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(17)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => ({
                type: 'character',
                chaId: 'prepared-char',
                name: 'Prepared',
                chats: [],
                additionalAssets: [['portrait', 'prepared/portrait.png', 'png']],
            } as Database['characters'][number]),
            {
                assetAliases: [{
                    kind: 'asset',
                    key: 'prepared/portrait.png',
                    objectHash: 'a'.repeat(64),
                    size: 4,
                    mime: 'image/png',
                    name: 'portrait.png',
                    ext: 'png',
                }],
                assetOwnerHeads: [{
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: 'prepared-char',
                    },
                    present: true,
                    manifestHash: 'b'.repeat(64),
                    entryCount: 1,
                }],
            },
        )).rejects.toThrow('activation failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(database).toEqual(before)
        expect(coordinator.revision).toBe(17)
    })

    it('rejects an Inlay alias before prepared character activation', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 18, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async () => ({ revision: 19 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(18)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => ({
                type: 'character',
                chaId: 'prepared-char',
                name: 'Prepared',
                chats: [],
            } as Database['characters'][number]),
            {
                assetAliases: [{
                    kind: 'inlay',
                    key: 'prepared/portrait.png',
                    objectHash: 'a'.repeat(64),
                    size: 4,
                    mime: 'image/png',
                    name: 'portrait.png',
                    ext: 'png',
                    inlayType: 'image',
                }],
            } as any,
        )).rejects.toThrow('Prepared character aliases must be ordinary assets')

        expect(store.commit).not.toHaveBeenCalled()
    })

    it('rejects an owner head for another character before activation', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 20, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async () => ({ revision: 21 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(20)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'prepared-char',
            'prepared-native-character',
            () => ({
                type: 'character',
                chaId: 'prepared-char',
                name: 'Prepared',
                chats: [],
            } as Database['characters'][number]),
            {
                assetOwnerHeads: [{
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: 'other-char',
                    },
                    present: true,
                    manifestHash: 'b'.repeat(64),
                    entryCount: 1,
                }],
            },
        )).rejects.toThrow(
            'Prepared character owner heads must belong to prepared-char',
        )

        expect(store.commit).not.toHaveBeenCalled()
    })

    it('can add an absent sentinel without adding it to character order', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const store = {
            readRoot: vi.fn(async () => ({ revision: 19, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(19)

        await coordinator.upsertPersistentCompleteCharacter(
            '§temp',
            'multiuser-sentinel',
            () => ({ type: 'character', chaId: '§temp', name: 'Temporary', chats: [] } as any),
            { includeInCharacterOrder: false },
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 19,
            addCharacter: expect.objectContaining({ chaId: '§temp' }),
        })
    })

    it('rebases concurrent live root edits while reading complete presets', async () => {
        const database = makeDatabase()
        database.botPresetsId = 0
        const presetRead = deferred<{
            revision: number
            value: Database['botPresets'][number]
        }>()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 6, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 6,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(() => presetRead.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(6)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.botPresetsId = 1
        })
        await vi.waitFor(() => expect(store.readPreset).toHaveBeenCalledOnce())
        database.username = 'Concurrent root edit'
        coordinator.markPersistentDataDirty(1)
        presetRead.resolve({ revision: 6, value: { name: 'First' } as Database['botPresets'][number] })
        await mutation

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 6,
            root: expect.objectContaining({
                botPresetsId: 1,
                username: 'Concurrent root edit',
            }),
            replacePresets: [{ name: 'First' }],
        })
        expect(database.username).toBe('Concurrent root edit')
    })

    it('preserves a same-field live root edit made during preset reads', async () => {
        const database = makeDatabase()
        database.mainPrompt = 'Initial prompt'
        const presetRead = deferred<{
            revision: number
            value: Database['botPresets'][number]
        }>()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 8, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 8,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(() => presetRead.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(8)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.mainPrompt = 'Preset prompt'
        })
        await vi.waitFor(() => expect(store.readPreset).toHaveBeenCalledOnce())
        database.mainPrompt = 'Later live prompt'
        coordinator.markPersistentDataDirty(1)
        presetRead.resolve({
            revision: 8,
            value: { name: 'First' } as Database['botPresets'][number],
        })
        await mutation

        expect(store.commit).toHaveBeenCalledWith(expect.objectContaining({
            expectedRevision: 8,
            root: expect.objectContaining({ mainPrompt: 'Later live prompt' }),
        }))
        expect(database.mainPrompt).toBe('Later live prompt')
    })

    it('preserves and later flushes live root edits made while a preset commit is pending', async () => {
        const database = makeDatabase()
        database.botPresetsId = 0
        const presetCommit = deferred<{ revision: number }>()
        const commit = vi.fn(({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) return presetCommit.promise
            return Promise.resolve({ revision: expectedRevision + 1 })
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 6, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 6,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 6,
                value: { name: 'First' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(6)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.botPresetsId = 1
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.username = 'Edit during preset commit'
        coordinator.markPersistentDataDirty(1)
        presetCommit.resolve({ revision: 7 })
        await mutation

        expect(database).toMatchObject({
            botPresetsId: 1,
            username: 'Edit during preset commit',
        })
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 6,
            root: {
                botPresetsId: 1,
                username: 'Fixture',
            },
        })

        await coordinator.flushPendingData('after-preset-commit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 7,
            rootMutations: [{ type: 'set', key: 'username', value: 'Edit during preset commit' }],
        })
        expect(commit.mock.calls[1][0]).not.toHaveProperty('replacePresets')
        expect(coordinator.revision).toBe(8)
    })

    it.each([
        ['scalable', false],
        ['maximum', true],
    ])('preserves a later same-field root edit during a pending %s preset commit', async (
        _profile,
        capturesCompletePresets,
    ) => {
        const database = makeDatabase()
        database.mainPrompt = 'initial'
        database.botPresets = [{ name: 'First' }] as Database['botPresets']
        const presetCommit = deferred<{ revision: number }>()
        const commit = vi.fn(({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) return presetCommit.promise
            return Promise.resolve({ revision: expectedRevision + 1 })
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 30, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 30,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 30,
                value: { name: 'First' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => capturesCompletePresets ? database.botPresets : null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(30)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.mainPrompt = 'preset selection'
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.mainPrompt = 'later user edit'
        coordinator.markPersistentDataDirty(1)
        presetCommit.resolve({ revision: 31 })
        await mutation

        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 30,
            root: expect.objectContaining({ mainPrompt: 'preset selection' }),
        })
        expect(database.mainPrompt).toBe('later user edit')

        await coordinator.flushPendingData('after-preset-same-field-edit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 31,
            rootMutations: [{ type: 'set', key: 'mainPrompt', value: 'later user edit' }],
        })
        expect(coordinator.revision).toBe(32)
    })

})
