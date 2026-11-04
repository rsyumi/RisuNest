import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
import { describe, expect, it, vi } from 'vitest'
import { createPersistentStoreHarness } from '../../../tests/support/persistentStoreHarness'
import type { Chat, Database, character } from './database.svelte'
import { RevisionConflictError } from './persistentDataStore'
import { PayloadTooLargeError, UnsaveableValueError } from './nativePersistenceValue'
import { createLeaseStub, createStoreStub } from './persistentDataStore.testSupport'
import { canonicalJson, PersistentMutationFencedError } from './saveCoordinator'
import { createPersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import {
    ActiveConversationSession,
    cloneConversationMetadata,
} from './activeConversationSession'
import {
    captureRoot,
    committedChanges,
    deferred,
    makeDatabase,
    makeStore,
    SaveCoordinator,
} from './saveCoordinator.testSupport'

describe('SaveCoordinator', () => {
    it('rejects an uncovered owned append without acknowledging it and allows a later retry', async () => {
        const database = makeChattyDatabase()
        let available = false
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => available ? database.characters[0] : null,
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const session = new ActiveConversationSession({
            characterId: 'char-a', conversationId: 'two',
            conversation: database.characters[0].chats[1], storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        session.transaction((transaction) => transaction.append({ role: 'user', data: 'Unsaved synthetic turn' }))
        await expect(coordinator.flushPendingData('missing-capture')).rejects.toThrow(
            'Pending conversation mutations could not be persisted',
        )
        expect(commit).not.toHaveBeenCalled()
        expect(onPersisted).not.toHaveBeenCalled()
        expect(coordinator.hasPendingPersistenceWork).toBe(true)
        available = true
        await coordinator.flushPendingData('restored-capture')
        expect(commit).toHaveBeenCalledWith(expect.objectContaining({
            replaceCharacter: expect.objectContaining({
                chats: expect.arrayContaining([expect.objectContaining({
                    id: 'two', message: expect.arrayContaining([{ role: 'user', data: 'Unsaved synthetic turn' }]),
                })]),
            }),
        }))
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it.each(['unchanged', 'other-conversation', 'unrelated-detail'] as const)('acknowledges captured session evidence with all-resident capture for %s edits', async (kind) => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({expectedRevision}) => ({revision: expectedRevision + 1}))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit), captureRoot: () => captureRoot(database),
            captureCharacters: () => database.characters,
            captureSelectedCharacter: () => database.characters[0], replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const session = new ActiveConversationSession({characterId: 'char-a', conversationId: 'two',
            conversation, storeRevision: 2, onMutation: (event) => coordinator.recordActiveConversationMutation(event)})
        session.transaction((transaction) => transaction.edit(transaction.locate(0), {...conversation.message[0]}))
        if (kind === 'other-conversation') database.characters[0].chats[0].name = 'Changed metadata'
        if (kind === 'unrelated-detail') database.characters[0].name = 'Changed detail'
        await coordinator.flushPendingDataLocally('captured-session-evidence')
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it('does not acknowledge all-resident session evidence whose metadata no longer matches the capture', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({expectedRevision}) => ({revision: expectedRevision + 1}))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({store: makeStore(commit), captureRoot: () => captureRoot(database),
            captureCharacters: () => database.characters, captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined, onConversationMutationPersisted: onPersisted})
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const session = new ActiveConversationSession({characterId: 'char-a', conversationId: 'two', conversation,
            storeRevision: 2, onMutation: (event) => coordinator.recordActiveConversationMutation(event)})
        session.transaction((transaction) => transaction.edit(transaction.locate(0), {...conversation.message[0]}))
        conversation.name = 'Changed after evidence'
        await expect(coordinator.flushPendingDataLocally('uncovered-session-metadata')).rejects.toThrow('Pending conversation mutations could not be persisted')
        expect(commit).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({unitMutations: [
            {key: '["conversation","char-a","two","name"]', type: 'set', value: 'Changed after evidence'},
        ]}))
        expect(onPersisted).not.toHaveBeenCalled()
        expect(session.persistedVersion).toBe(0)
        expect(coordinator.hasPendingPersistenceWork).toBe(true)
    })

    it('retains covered all-resident session evidence until its independent detail commit succeeds', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn().mockRejectedValueOnce(new Error('synthetic write failure'))
            .mockImplementation(async ({expectedRevision}) => ({revision: expectedRevision + 1}))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({store: makeStore(commit), captureRoot: () => captureRoot(database),
            captureCharacters: () => database.characters, captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined, onConversationMutationPersisted: onPersisted})
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const session = new ActiveConversationSession({characterId: 'char-a', conversationId: 'two', conversation,
            storeRevision: 2, onMutation: (event) => coordinator.recordActiveConversationMutation(event)})
        session.transaction((transaction) => transaction.edit(transaction.locate(0), {...conversation.message[0]}))
        database.characters[0].name = 'Independent detail'
        await expect(coordinator.flushPendingDataLocally('failed-session-evidence')).rejects.toThrow('synthetic write failure')
        expect(onPersisted).not.toHaveBeenCalled()
        expect(coordinator.hasPendingPersistenceWork).toBe(true)
        await coordinator.flushPendingDataLocally('retry-session-evidence')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({unitMutations: [
            {key: '["character","char-a","name"]', type: 'set', value: 'Independent detail'},
        ], conversations: [{type: 'replace-range', characterId: 'char-a', conversationId: 'two', start: 0, deleteCount: 1,
            messages: [{role: 'user', data: 'hello two'}]}]})
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it('rejects top-level values that JSON cannot serialize', () => {
        expect(() => canonicalJson(undefined)).toThrow(TypeError)
    })

    it('initializes large plugin baselines once and retains exact later mutation detection', async () => {
        const database = makeDatabase()
        const payload = 'synthetic-large-plugin:'.repeat(50000)
        database.pluginCustomStorage = { payload, nested: { count: 1 } }
        const store = makeStore(vi.fn(async () => ({ revision: 2 })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        const parse = vi.spyOn(JSON, 'parse')
        const stringify = vi.spyOn(JSON, 'stringify')
        let parses: number, encodes: number
        try {
            coordinator.initialize(1, database)
            parses = parse.mock.calls.filter(([value]) =>
                value.includes('synthetic-large-plugin:'),
            ).length
            encodes = stringify.mock.calls.filter(([value]) => value === payload).length
        } finally {
            parse.mockRestore()
            stringify.mockRestore()
        }
        expect(parses).toBe(0)
        expect(encodes).toBe(1)
        await coordinator.flushPendingData('clean')
        expect(store.commit).not.toHaveBeenCalled()
        database.pluginCustomStorage.nested.count = 2
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('changed')
        expect(store.commit).toHaveBeenCalledWith(
            expect.objectContaining({
                pluginStorage: [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'nested', value: { count: 2 } }],
            }),
        )
        expect(database.pluginCustomStorage.payload).toBe(payload)
    })

    function makeAdditionDatabase() {
        const database = makeDatabase()
        const added = structuredClone(database.characters[0])
        added.chaId = 'char-added'
        added.name = 'Added'
        return { database, added }
    }

    it('does not commit when the persistent working copy is clean', async () => {
        const database = makeDatabase()
        const store = createStoreStub({ commit: vi.fn() })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)

        await coordinator.flushPendingData('test')

        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(4)
    })

    it('SaveCoordinator flush does not traverse inactive characters', async () => {
        const database = makeDatabase()
        const inactive = makeDatabase().characters[0]
        Object.defineProperty(inactive, 'chats', {
            enumerable: true,
            get: () => {
                throw new Error('inactive character was traversed')
            },
        })
        database.characters.push(inactive)
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })

        coordinator.initialize(1)
        await coordinator.flushPendingData('clean')
    })

    it('initializes baselines from the supplied authoritative database', async () => {
        let database = makeDatabase()
        database.username = 'Stale live state'
        const authoritative = makeDatabase()
        authoritative.username = 'Authoritative state'
        authoritative.characters[0].name = 'Authoritative character'
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
        })

        coordinator.initialize(6, authoritative)
        database = structuredClone(authoritative)
        await coordinator.flushPendingData('clean')

        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(6)
    })

    it('returns the exact shared promise for concurrent flushes', async () => {
        const database = makeDatabase()
        const pending = deferred<{ revision: number }>()
        const store = makeStore(vi.fn(() => pending.promise))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(10)

        const first = coordinator.flushPendingData('first')
        const second = coordinator.flushPendingData('second')

        expect(second).toBe(first)
        pending.resolve({ revision: 2 })
        await first
    })

    it('reports the exact shared flush promise and clears it after completion', async () => {
        const database = makeDatabase()
        const pending = deferred<{ revision: number }>()
        const reported: Array<Promise<void> | null> = []
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(() => pending.promise)),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
        })
        coordinator.initialize(1)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(1)

        const first = coordinator.flushPendingData('first')
        const second = coordinator.flushPendingData('second')
        pending.resolve({ revision: 2 })
        await first
        await Promise.resolve()

        expect(second).toBe(first)
        expect(reported).toEqual([first, null])
    })

    it('debounces for 500 ms and flushes immediately at the pending byte limit', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Debounced'

            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(499)
            expect(store.commit).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(1)
            expect(store.commit).toHaveBeenCalledTimes(1)

            database.username = 'Immediate'
            coordinator.markPersistentDataDirty(1_048_576)
            await vi.advanceTimersByTimeAsync(0)
            expect(store.commit).toHaveBeenCalledTimes(2)
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('treats byte estimates as absolute pending size instead of summing them', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Large snapshots'

            coordinator.markPersistentDataDirty(600_000)
            coordinator.markPersistentDataDirty(600_000)

            expect(coordinator.pendingBytes).toBe(600_000)
            await vi.advanceTimersByTimeAsync(0)
            expect(store.commit).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).toHaveBeenCalledTimes(1)
        } finally {
            vi.useRealTimers()
        }
    })

    it('starts one immediate flush while estimates stay above the byte limit', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const gate = deferred<{ revision: number }>()
            const commit = vi.fn().mockImplementationOnce(() => gate.promise)
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Huge'

            coordinator.markPersistentDataDirty(2_097_152)
            await vi.advanceTimersByTimeAsync(0)
            expect(commit).toHaveBeenCalledTimes(1)
            coordinator.markPersistentDataDirty(2_097_152)
            coordinator.markPersistentDataDirty(2_097_152)
            await vi.advanceTimersByTimeAsync(0)
            expect(commit).toHaveBeenCalledTimes(1)

            gate.resolve({ revision: 2 })
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledTimes(1)
            expect(coordinator.pendingBytes).toBe(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('clamps invalid estimated byte counts to zero while retaining dirty work', async () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        coordinator.markPersistentDataDirty(-1)
        coordinator.markPersistentDataDirty(Number.NaN)
        coordinator.markPersistentDataDirty(Number.POSITIVE_INFINITY)

        expect(coordinator.pendingBytes).toBe(0)
        await coordinator.flushPendingData('cleanup')
    })

    it('commits only changed root and complete selected-character fields', async () => {
        const database = makeDatabase()
        database.characters[0].chats = [
            { id: 'one', name: 'One', message: [], localLore: [], note: '' },
            { id: 'two', name: 'Two', message: [], localLore: [], note: '' },
        ]
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = makeStore(commit)
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.username = 'Root changed'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('root')
        const rootOnly = committedChanges(commit.mock.calls[0][0])
        expect(rootOnly.expectedRevision).toBe(2)
        expect(rootOnly.rootChanges).toEqual({ username: 'Root changed' })
        expect(rootOnly).not.toHaveProperty('replaceCharacter')

        database.characters[0].chats[1].name = 'Inactive renamed'
        database.characters[0].chats.reverse()
        database.characters[0].chats.push({
            id: 'three',
            name: 'Three',
            message: [],
            localLore: [],
            note: '',
        })
        database.characters[0].chats.splice(1, 1)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('character')
        expect(committedChanges(commit.mock.calls[1][0]).rootChanges).toEqual({})
        expect(commit.mock.calls[1][0].replaceCharacter.chats.map((chat: { id: string }) => chat.id)).toEqual([
            'two',
            'three',
        ])

        database.username = 'Combined root'
        database.characters[0].name = 'Combined character'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('combined')
        const combined = committedChanges(commit.mock.calls[2][0])
        expect(combined).toMatchObject({ expectedRevision: 4, replaceCharacter: { name: 'Combined character' } })
        expect(combined.rootChanges).toEqual({ username: 'Combined root' })
    })

    function makeChattyDatabase() {
        const database = makeDatabase()
        database.characters[0].chats = [
            {
                id: 'one',
                name: 'One',
                message: [{ role: 'user', data: 'hello one' }],
                localLore: [],
                note: '',
            },
            {
                id: 'two',
                name: 'Two',
                message: [
                    { role: 'user', data: 'hello two' },
                    { role: 'char', data: 'reply two' },
                ],
                localLore: [],
                note: '',
            },
        ]
        return database
    }

    it('commits session-owned replacement ranges in order and acknowledges the exact version', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.transaction((transaction) => {
            transaction.edit(
                transaction.locate(0),
                { role: 'user', data: 'session edit' },
            )
            transaction.append({ role: 'char', data: 'session append' })
        })
        await coordinator.flushPendingData('session-ranges')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].conversations).toEqual([
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 0,
                deleteCount: 1,
                messages: [{ role: 'user', data: 'session edit' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'char', data: 'session append' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
        ])
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(onPersisted).toHaveBeenCalledWith(expect.objectContaining({
            characterId: 'char-a',
            conversationId: 'two',
            sessionVersion: 2,
            revision: 3,
        }))
    })

    it('persists one atomic multi-range and metadata operation through one working-set commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        const expectedMetadata = cloneConversationMetadata(conversation)

        session.applyOperation({
            expectedVersion: 0,
            expectedMetadata,
            metadata: {
                ...expectedMetadata,
                scriptstate: { '$counter': '2' },
            },
            ranges: [
                {
                    position: session.positionAt(0),
                    deleteCount: 1,
                    messages: [{ role: 'user', data: 'parsed first' }],
                },
                {
                    position: session.positionAt(1),
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'parsed second' }],
                },
            ],
        })
        await coordinator.flushPendingData('atomic-multi-range')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            conversations: [
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'two',
                    start: 0,
                    deleteCount: 1,
                    messages: [{ role: 'user', data: 'parsed first' }],
                    conversation: expect.objectContaining({
                        scriptstate: { '$counter': '2' },
                    }),
                },
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'two',
                    start: 1,
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'parsed second' }],
                    conversation: expect.objectContaining({
                        scriptstate: { '$counter': '2' },
                    }),
                },
            ],
        })
        expect(session.persistedVersion).toBe(2)
        expect(session.storeRevision).toBe(3)
    })

    it('commits and acknowledges an ordered session command whose final value is unchanged', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.edit(session.locate(0), { role: 'user', data: 'hello two' })
        await coordinator.flushPendingData('unchanged-session-command')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].conversations).toEqual([{
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            start: 0,
            deleteCount: 1,
            messages: [{ role: 'user', data: 'hello two' }],
            conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
        }])
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(3)
    })

    it.each([false, true])('acknowledges a session append covered by legacy interaction and message-ID normalization (all-resident: %s)', async (allResident) => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const onPersisted = vi.fn((event) => {
            session.acknowledgePersisted(
                event.sessionToken,
                event.sessionVersion,
                event.revision,
            )
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacters: allResident ? () => database.characters : undefined,
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        database.characters[0].lastInteraction = 123
        database.characters[0].chats[1].message =
            database.characters[0].chats[1].message.map((message, index) => {
                message.chatId ??= `normalized-${index}`
                return message
            })
        coordinator.markPersistentDataDirty(1)
        session.append({
            role: 'user',
            data: 'session append',
            chatId: 'session-output',
        })
        await coordinator.flushPendingData('mixed-session-and-legacy')

        expect(commit).toHaveBeenCalledOnce()
        if (allResident) {
            expect(commit.mock.calls[0][0].replaceCharacter).toBeUndefined()
            expect(commit.mock.calls[0][0].unitMutations).toEqual([
                {key: '["character","char-a","lastInteraction"]', type: 'set', value: 123},
            ])
            expect(commit.mock.calls[0][0].conversations).toEqual([
                {type: 'replace-range', characterId: 'char-a', conversationId: 'two', start: 0, deleteCount: 2,
                    messages: [
                        {role: 'user', data: 'hello two', chatId: 'normalized-0'},
                        {role: 'char', data: 'reply two', chatId: 'normalized-1'},
                        {role: 'user', data: 'session append', chatId: 'session-output'},
                    ]},
            ])
        } else {
            expect(commit.mock.calls[0][0].conversations).toBeUndefined()
            expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
                chaId: 'char-a',
                lastInteraction: 123,
                chats: [
                    expect.anything(),
                    {
                        id: 'two',
                        name: 'Two',
                        localLore: [],
                        note: '',
                        message: [
                            expect.objectContaining({ chatId: 'normalized-0' }),
                            expect.objectContaining({ chatId: 'normalized-1' }),
                            expect.objectContaining({
                                data: 'session append',
                                chatId: 'session-output',
                            }),
                        ],
                    },
                ],
            })
        }
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(3)

        session.append({
            role: 'char',
            data: 'second session append',
            chatId: 'second-session-output',
        })
        database.characters[0].name = 'Legacy follow-up'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('legacy-follow-up')
        expect(onPersisted).toHaveBeenCalledTimes(2)
        expect(session.persistedVersion).toBe(2)
        expect(session.storeRevision).toBe(4)
    })

    it('retains a session-owned range without acknowledgement until a failed save retries', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('range write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        let now = 1_000
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            now: () => now,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.append({ role: 'user', data: 'retry exact range' })
        await expect(coordinator.flushPendingData('first-attempt')).rejects.toThrow(
            'range write failed',
        )
        expect(session.persistedVersion).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.pinCount('dirty')).toBe(1)
        expect(session.residentBytes).toBe(1)
        expect(coordinator.pendingBytes).toBeGreaterThanOrEqual(0)
        now = 3_500
        expect(coordinator.pendingConversationMutationDiagnostics).toEqual({ pendingEvents: 1, unacknowledgedMs: 2_500 })

        await coordinator.flushPendingData('retry-attempt')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].conversations).toEqual(
            commit.mock.calls[0][0].conversations,
        )
        expect(session.persistedVersion).toBe(1)
        expect(session.storeRevision).toBe(3)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.pinCount('dirty')).toBe(0)
        expect(session.residentBytes).toBe(0)
        expect(coordinator.pendingConversationMutationDiagnostics).toEqual({ pendingEvents: 0, unacknowledgedMs: undefined })
    })

    it('reports pending conversation events with the age of the oldest until they are acknowledged', async () => {
        const database = makeChattyDatabase()
        let session!: ActiveConversationSession
        let now = 1_000
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            now: () => now,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(event.sessionToken, event.sessionVersion, event.revision)
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.append({ role: 'user', data: 'first unsaved' })
        now = 1_400
        session.append({ role: 'char', data: 'second unsaved' })
        now = 2_000
        expect(coordinator.pendingConversationMutationDiagnostics).toEqual({ pendingEvents: 2, unacknowledgedMs: 1_000 })

        await coordinator.flushPendingData('diagnostic-ages')
        expect(coordinator.pendingConversationMutationDiagnostics).toEqual({ pendingEvents: 0, unacknowledgedMs: undefined })
    })

    function makeAcknowledgedSession(
        store: ConstructorParameters<typeof SaveCoordinator>[0]['store'],
        database: Database,
        revision: number,
    ) {
        const persisted: Array<[version: number, revision: number]> = []
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                persisted.push([event.sessionVersion, event.revision])
                session.acknowledgePersisted(event.sessionToken, event.sessionVersion, event.revision)
            },
        })
        coordinator.initialize(revision)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: revision,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        return { coordinator, session, persisted }
    }

    it('folds a reply streamed while the store is blocked into one catch-up range under the commit limit', async () => {
        const database = makeChattyDatabase()
        database.characters[0].chats[1].note = 'n'.repeat(2048)
        const storage = await createPersistentStoreHarness(structuredClone(database))
        const realCommit = storage.store.commit.bind(storage.store)
        const blocked = deferred<void>()
        const entered = deferred<void>()
        const committedRanges: number[] = []
        vi.spyOn(storage.store, 'commit').mockImplementation(async (input) => {
            const bytes = new TextEncoder().encode(JSON.stringify(input)).byteLength
            if (committedRanges.length === 0) {
                entered.resolve()
                await blocked.promise
            }
            if (bytes > 32 * 1024) throw new PayloadTooLargeError('commit', bytes)
            committedRanges.push(input.conversations?.length ?? 0)
            return realCommit(input)
        })
        const { coordinator, session, persisted } = makeAcknowledgedSession(storage.store, database, storage.revision)

        session.append({ role: 'char', data: '' })
        const flushing = coordinator.flushPendingDataLocally('blocked-stream')
        await entered.promise
        let reply = ''
        for (let update = 1; update <= 40; update++) {
            reply += 'x'.repeat(100)
            session.edit(session.locate(2), { role: 'char', data: reply })
            if (update === 20) session.updateChatVariables(new Map([['progress', 'half']]))
        }
        expect(coordinator.pendingConversationMutationDiagnostics.pendingEvents).toBe(42)
        blocked.resolve()
        await flushing

        expect(committedRanges).toEqual([1, 1])
        const stored = (await storage.store.readConversation('char-a', 'two'))?.value
        expect(stored?.message.at(-1)?.data).toBe(reply)
        expect(stored).toMatchObject({ note: 'n'.repeat(2048), scriptstate: { $progress: 'half' } })
        expect(persisted).toEqual([
            [1, storage.revision + 1],
            ...Array.from({ length: 41 }, (_, index) => [index + 2, storage.revision + 2]),
        ])
        expect(session.persistedVersion).toBe(42)
        expect(session.residencyFallbackActive).toBe(false)
        expect(session.pinCount('dirty')).toBe(0)
    })

    it('merges a reply streamed around a failed save into one retried range', async () => {
        const database = makeChattyDatabase()
        const blocked = deferred<void>()
        const entered = deferred<void>()
        const commit = vi.fn()
            .mockImplementationOnce(async () => {
                entered.resolve()
                await blocked.promise
                throw new Error('synthetic write failure')
            })
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const { coordinator, session, persisted } = makeAcknowledgedSession(makeStore(commit), database, 2)

        session.append({ role: 'char', data: 'reply' })
        const failing = coordinator.flushPendingDataLocally('failing-store')
        await entered.promise
        session.edit(session.locate(2), { role: 'char', data: 'reply continued' })
        blocked.resolve()
        await expect(failing).rejects.toThrow('synthetic write failure')
        session.edit(session.locate(2), { role: 'char', data: 'reply continued further' })
        await coordinator.flushPendingDataLocally('retry')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].conversations).toEqual([expect.objectContaining({
            start: 2,
            deleteCount: 0,
            messages: [{ role: 'char', data: 'reply continued further' }],
        })])
        expect(persisted.map(([version]) => version)).toEqual([1, 2, 3])
    })

    it('starts a new pending entry for an edit outside the streamed reply, a second append and a delete', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const { coordinator, session, persisted } = makeAcknowledgedSession(makeStore(commit), database, 2)

        session.append({ role: 'char', data: 'reply' })
        session.edit(session.locate(2), { role: 'char', data: 'reply streamed' })
        session.edit(session.locate(0), { role: 'user', data: 'edited first' })
        session.edit(session.locate(2), { role: 'char', data: 'reply final' })
        session.append({ role: 'user', data: 'next' })
        session.edit(session.locate(3), { role: 'user', data: 'next edited' })
        session.delete(session.locate(3))
        await coordinator.flushPendingDataLocally('non-foldable')

        expect(commit.mock.calls[0][0].conversations.map(
            ({ start, deleteCount, messages }: { start: number; deleteCount: number; messages: unknown[] }) =>
                ({ start, deleteCount, messages }),
        )).toEqual([
            { start: 2, deleteCount: 0, messages: [{ role: 'char', data: 'reply streamed' }] },
            { start: 0, deleteCount: 1, messages: [{ role: 'user', data: 'edited first' }] },
            { start: 2, deleteCount: 1, messages: [{ role: 'char', data: 'reply final' }] },
            { start: 3, deleteCount: 0, messages: [{ role: 'user', data: 'next edited' }] },
            { start: 3, deleteCount: 1, messages: [] },
        ])
        expect(persisted.map(([version]) => version)).toEqual([1, 2, 3, 4, 5, 6, 7])
    })

    it('keeps the stored recency when a later streamed update stops naming lastDate', async () => {
        const database = makeChattyDatabase()
        const conversation = database.characters[0].chats[1]
        conversation.lastDate = 100
        const storage = await createPersistentStoreHarness(structuredClone(database))
        const { coordinator, session } = makeAcknowledgedSession(storage.store, database, storage.revision)
        const initial = cloneConversationMetadata(conversation)
        const dated = { ...initial, lastDate: 500 }
        const { lastDate: _lastDate, ...undated } = dated

        session.applyOperation({
            expectedVersion: 0,
            expectedMetadata: initial,
            metadata: dated,
            range: { position: session.positionAt(2), deleteCount: 0, messages: [{ role: 'char', data: 'reply' }] },
        })
        session.applyOperation({
            expectedVersion: 1,
            expectedMetadata: dated,
            metadata: undated,
            range: { position: session.positionAt(2), deleteCount: 1, messages: [{ role: 'char', data: 'reply continued' }] },
        })
        await coordinator.flushPendingDataLocally('recency')

        const summaries = await storage.store.queryConversations({ characterId: 'char-a', order: 'configured', limit: 10 })
        expect(summaries.items.find((summary) => summary.id === 'two')?.recentAt).toBe(500)
        expect((await storage.store.readConversation('char-a', 'two'))?.value.message.at(-1)?.data).toBe('reply continued')
    })

    it('pins only covered session commands while their store commit is in flight', async () => {
        const database = makeChattyDatabase()
        let finishCommit!: () => void
        const commitGate = new Promise<void>((resolve) => {
            finishCommit = resolve
        })
        const commit = vi.fn(async ({ expectedRevision }) => {
            await commitGate
            return { revision: expectedRevision + 1 }
        })
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: database.characters[0].chats[1],
            storeRevision: 2,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.append({ role: 'user', data: 'pending exact range' })
        const flushing = coordinator.flushPendingData('pending-save-pin')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        expect(session.pinCount('dirty')).toBe(1)
        expect(session.pinCount('pending-save')).toBe(1)

        finishCommit()
        await flushing

        expect(session.pinCount('dirty')).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.persistedVersion).toBe(1)
    })

    it('retains then acknowledges a session command captured after a legacy structural append', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            .mockRejectedValueOnce(new Error('fallback write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: (event) =>
                session.beginPersistence(event.sessionVersion),
            onConversationMutationPersisted: (event) => {
                session.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            },
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const baselineMessageCount = conversation.message.length
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            measureMessage: () => 1,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        conversation.message.push({ role: 'char', data: 'legacy direct append' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('legacy-structural-baseline')

        session.append({ role: 'user', data: 'session append' })
        await expect(
            coordinator.flushPendingData('legacy-structural-fallback-failed'),
        ).rejects.toThrow('fallback write failed')

        expect(session.persistedVersion).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)

        await coordinator.flushPendingData('legacy-structural-fallback-retry')

        expect(session.residencyFallbackActive).toBe(true)
        expect(session.persistedVersion).toBe(1)
        expect(session.pinCount('dirty')).toBe(0)
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.residentBytes).toBe(0)
        expect(commit).toHaveBeenCalledTimes(3)
        expect(commit.mock.calls[2][0].conversations).toEqual([
            expect.objectContaining({
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 0,
                deleteCount: baselineMessageCount + 1,
                messages: conversation.message,
            }),
        ])

        await coordinator.flushPendingData('legacy-structural-fallback-repeat')
        expect(commit).toHaveBeenCalledTimes(3)
    })

    it('keeps strict replacement evidence across a session-token rollover before flush', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        const makeSession = () => new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        makeSession().append({ role: 'user', data: 'first session' })
        makeSession().append({ role: 'char', data: 'replacement session' })
        await coordinator.flushPendingData('session-token-rollover')

        expect(commit.mock.calls[0][0].conversations).toEqual([
            expect.objectContaining({
                type: 'replace-range',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'first session' }],
            }),
            expect.objectContaining({
                type: 'replace-range',
                start: 3,
                deleteCount: 0,
                messages: [{ role: 'char', data: 'replacement session' }],
            }),
        ])
        expect(onPersisted).toHaveBeenCalledTimes(2)
    })

    it('does not acknowledge pending evidence for a character omitted from the commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const detachedConversation: Chat = {
            id: 'detached-chat',
            name: 'Detached',
            message: [],
            localLore: [],
            note: '',
        }
        const detachedSession = new ActiveConversationSession({
            characterId: 'char-b',
            conversationId: 'detached-chat',
            conversation: detachedConversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        detachedSession.append({ role: 'user', data: 'not in selected capture' })
        database.characters[0].name = 'Committed selected character'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('other-character')).rejects.toThrow(
            'Pending conversation mutations could not be persisted',
        )

        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Committed selected character',
        })
        expect(onPersisted).not.toHaveBeenCalled()
        expect(detachedSession.persistedVersion).toBe(0)
        expect(coordinator.hasPendingPersistenceWork).toBe(true)
    })

    it('does not acknowledge same-character evidence omitted from a fallback conversation commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const onPersisted = vi.fn()
        const onPersistenceStarted = vi.fn(() => null)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: onPersistenceStarted,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const detachedConversation = structuredClone(database.characters[0].chats[1])
        const detachedSession = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: detachedConversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        detachedSession.append({ role: 'user', data: 'not in captured conversation' })
        database.characters[0].chats[0].message[0].data = 'captured fallback edit'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('same-character-fallback')).rejects.toThrow(
            'Pending conversation mutations could not be persisted',
        )

        expect(commit.mock.calls[0][0].conversations).toEqual([
            expect.objectContaining({
                characterId: 'char-a',
                conversationId: 'one',
            }),
        ])
        expect(onPersistenceStarted).not.toHaveBeenCalled()
        expect(onPersisted).not.toHaveBeenCalled()
        expect(coordinator.hasPendingPersistenceWork).toBe(true)
    })

    it('retires unprojectable evidence after persisting that conversation through fallback', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let session!: ActiveConversationSession
        const onFallbackPersisted = vi.fn((event) => {
            session.acknowledgeFallbackPersisted(
                event.sessionToken,
                event.sessionVersion,
                event.revision,
            )
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationFallbackPersisted: onFallbackPersisted,
        })
        coordinator.initialize(2)
        const conversation = database.characters[0].chats[1]
        session = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })

        session.append({ role: 'user', data: 'session append' })
        session.edit(session.locate(2), { role: 'user', data: 'session append continued' })
        conversation.message[0].data = 'untracked live edit'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('same-conversation-fallback')

        expect(commit.mock.calls[0][0].conversations).toEqual([
            expect.objectContaining({
                characterId: 'char-a',
                conversationId: 'two',
            }),
        ])
        expect(onFallbackPersisted.mock.calls.map(([event]) => event.sessionVersion)).toEqual([1, 2])
        expect(session.residencyFallbackActive).toBe(true)
        expect(session.persistedVersion).toBe(2)
        expect(coordinator.hasPendingPersistenceWork).toBe(false)
    })

    it('acknowledges only the covered session prefix from a mixed fallback commit', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        let coveredSession!: ActiveConversationSession
        let uncoveredSession!: ActiveConversationSession
        const onPersistenceStarted = vi.fn(() => null)
        const onPersisted = vi.fn((event) => {
            if (coveredSession.ownsSessionToken(event.sessionToken)) {
                coveredSession.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            } else if (uncoveredSession.ownsSessionToken(event.sessionToken)) {
                uncoveredSession.acknowledgePersisted(
                    event.sessionToken,
                    event.sessionVersion,
                    event.revision,
                )
            }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onConversationMutationPersistenceStarted: onPersistenceStarted,
            onConversationMutationPersisted: onPersisted,
        })
        coordinator.initialize(2)
        const liveConversation = database.characters[0].chats[1]
        coveredSession = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: liveConversation,
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        coveredSession.append({ role: 'user', data: 'covered append' })
        uncoveredSession = new ActiveConversationSession({
            characterId: 'char-a',
            conversationId: 'two',
            conversation: structuredClone(liveConversation),
            storeRevision: 2,
            onMutation: (event) => coordinator.recordActiveConversationMutation(event),
        })
        uncoveredSession.append({ role: 'char', data: 'detached append' })
        database.characters[0].lastInteraction = 456
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('covered-prefix-fallback')

        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            lastInteraction: 456,
            chats: [
                expect.anything(),
                expect.objectContaining({
                    message: expect.arrayContaining([
                        expect.objectContaining({ data: 'covered append' }),
                    ]),
                }),
            ],
        })
        expect(onPersisted).toHaveBeenCalledOnce()
        expect(onPersistenceStarted).toHaveBeenCalledOnce()
        expect(onPersistenceStarted).toHaveBeenCalledWith(expect.objectContaining({
            sessionToken: coveredSession.locate(2).sessionToken,
            sessionVersion: 1,
        }))
        expect(onPersisted).toHaveBeenCalledWith(expect.objectContaining({
            sessionToken: coveredSession.locate(2).sessionToken,
            sessionVersion: 1,
        }))
        expect(coveredSession.persistedVersion).toBe(1)
        expect(uncoveredSession.persistedVersion).toBe(0)
    })

    it.each([
        {
            label: 'tail edit',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message[1] = { role: 'char', data: 'edited reply' }
            },
            expected: {
                start: 1,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'edited reply' }],
            },
        },
        {
            label: 'middle insert',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message.splice(1, 0, { role: 'user', data: 'inserted' })
            },
            expected: {
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'inserted' }],
            },
        },
        {
            label: 'middle delete',
            prepare: (chat: Chat) => {
                chat.message.push({ role: 'user', data: 'shared suffix' })
            },
            mutate: (chat: Chat) => {
                chat.message.splice(1, 1)
            },
            expected: {
                start: 1,
                deleteCount: 1,
                messages: [],
            },
        },
        {
            label: 'complete replacement',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message.splice(
                    0,
                    chat.message.length,
                    { role: 'char', data: 'replacement one' },
                    { role: 'user', data: 'replacement two' },
                )
            },
            expected: {
                start: 0,
                deleteCount: 2,
                messages: [
                    { role: 'char', data: 'replacement one' },
                    { role: 'user', data: 'replacement two' },
                ],
            },
        },
        {
            label: 'metadata-only mutation',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.name = 'Renamed conversation'
            },
            expected: {
                start: 2,
                deleteCount: 0,
                messages: [],
            },
        },
    ])('commits the minimal conversation replace range for $label', async ({ prepare, mutate, expected }) => {
        const database = makeChattyDatabase()
        prepare(database.characters[0].chats[1])
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        mutate(database.characters[0].chats[1])
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-range')

        const mutation = commit.mock.calls[0][0].conversations[0]
        expect(mutation).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            ...expected,
        })
        expect(mutation.conversation).toEqual({
            id: 'two',
            name: database.characters[0].chats[1].name,
            localLore: [],
            note: '',
        })
    })

    it('preserves a shared suffix around a middle message edit', async () => {
        const database = makeChattyDatabase()
        database.characters[0].chats[1].message.push({ role: 'user', data: 'shared suffix' })
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message[1] = { role: 'char', data: 'middle edit' }
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-range')

        expect(commit.mock.calls[0][0].conversations[0]).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            start: 1,
            deleteCount: 1,
            messages: [{ role: 'char', data: 'middle edit' }],
        })
    })

    it('commits only the edited conversation when just chat content changes', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message.push({ role: 'user', data: 'follow-up' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(committedChanges(commit.mock.calls[0][0]).rootChanges).toEqual({})
        expect(commit.mock.calls[0][0].conversations).toEqual([
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'follow-up' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
        ])
        expect(coordinator.revision).toBe(3)

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(1)
    })

    it('replaces the whole character when a character field changes alongside chats', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].name = 'Alpha renamed'
        database.characters[0].chats[0].message.push({ role: 'char', data: 'more' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('detail-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({ name: 'Alpha renamed' })
    })

    it.each([
        ['added', (chats: { id?: string }[]) => chats.push({
            id: 'three',
            name: 'Three',
            message: [],
            localLore: [],
            note: '',
        } as never)],
        ['removed', (chats: { id?: string }[]) => chats.splice(0, 1)],
        ['reordered', (chats: { id?: string }[]) => chats.reverse()],
    ] as const)('replaces the whole character when chats are %s', async (_label, mutate) => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        mutate(database.characters[0].chats)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('structure-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0].replaceCharacter.chats.map((chat: { id: string }) => chat.id))
            .toEqual(database.characters[0].chats.map((chat) => chat.id))

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(1)
    })

    it('replaces the whole character when a chat is missing an id', async () => {
        const database = makeChattyDatabase()
        delete database.characters[0].chats[1].id
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message.push({ role: 'user', data: 'anonymous' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('missing-id')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0]).toHaveProperty('replaceCharacter')
    })

    it('keeps conversation edits dirty when the mutation commit fails', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[0].message.push({ role: 'user', data: 'retry me' })
        coordinator.markPersistentDataDirty(5)
        await expect(coordinator.flushPendingData('fails')).rejects.toThrow('write failed')

        expect(coordinator.revision).toBe(2)
        expect(coordinator.pendingBytes).toBe(5)

        await coordinator.flushPendingData('retry')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].conversations).toMatchObject([
            {
                conversationId: 'one',
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'retry me' }],
            },
        ])
        expect(coordinator.revision).toBe(3)
    })

    it('reports a successful local revision once before official publication', async () => {
        const database = makeDatabase()
        const events: string[] = []
        const store = makeStore(vi.fn(async ({ expectedRevision }) => {
            events.push('commit')
            return { revision: expectedRevision + 1 }
        }))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onLocalRevision: (revision) => events.push(`local:${revision}`),
            officialPublisher: {
                pin: async () => ({
                    publish: async () => {
                        events.push('publish')
                    },
                    dispose: async () => undefined,
                }),
            },
        })
        coordinator.initialize(4)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('test')
        await coordinator.flushPendingData('clean')

        expect(events).toEqual(['commit', 'local:5', 'publish'])
    })

    it('installs and commits a live character addition with root and previous selected edits', async () => {
        const { database, added } = makeAdditionDatabase()
        const install = vi.fn(() => database.characters.push(added))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.username = 'Root changed'
        database.characters[0].name = 'Selected changed'

        await coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 123,
            install,
        }, 'new-character')

        expect(install).toHaveBeenCalledOnce()
        const changes = commit.mock.calls.map(([input]) => committedChanges(input))
        expect(changes.find((change) => change.addCharacter)?.addCharacter).toMatchObject({ chaId: 'char-added', name: 'Added' })
        const edits = changes.find((change) => change.replaceCharacter)
        expect(edits?.replaceCharacter).toMatchObject({ chaId: 'char-a', name: 'Selected changed' })
        expect(edits?.rootChanges).toEqual({ username: 'Root changed' })
        expect(coordinator.revision).toBe(6)
    })

    it('reports a standalone addition exact promise and then idle state', async () => {
        const { database, added } = makeAdditionDatabase()
        const pending = deferred<{ revision: number }>()
        const reported: Array<Promise<void> | null> = []
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(() => pending.promise)),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
        })
        coordinator.initialize(1)

        const addition = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')

        expect(reported).toEqual([addition])
        pending.resolve({ revision: 2 })
        await addition
        await Promise.resolve()
        expect(reported).toEqual([addition, null])
    })

    it.each(['store', 'pin', 'publish'] as const)(
        'persists live character edits made while awaiting %s',
        async (stage) => {
            const { database, added } = makeAdditionDatabase()
            const storage = await createPersistentStoreHarness(structuredClone(database))
            const storeGate = stage === 'store' ? storage.gate('commit') : undefined
            const pinGate = deferred<{ publish(): Promise<void>; dispose(): Promise<void> }>()
            const publishGate = deferred<void>()
            const handle = {
                publish: vi.fn(() => stage === 'publish' ? publishGate.promise : Promise.resolve()),
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn(() => stage === 'pin' ? pinGate.promise : Promise.resolve(handle))
            const coordinator = new SaveCoordinator({
                store: storage.store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(storage.revision)

            const saving = coordinator.commitCharacterAddition({
                characterId: added.chaId,
                estimatedBytes: 1,
                install: () => database.characters.push(added),
            }, 'new-character')
            await storeGate?.entered
            await vi.waitFor(() => {
                if (stage === 'pin') expect(pin).toHaveBeenCalledOnce()
                if (stage === 'publish') expect(handle.publish).toHaveBeenCalledOnce()
            })
            added.name = `Changed during ${stage}`
            coordinator.markPersistentDataDirty(1)
            storeGate?.release()
            if (stage === 'pin') pinGate.resolve(handle)
            if (stage === 'publish') publishGate.resolve()
            await saving

            expect((await storage.store.readCharacter('char-added'))?.value.name).toBe(`Changed during ${stage}`)
        },
    )

    it('serializes previous selected and added-character edits into separate trailing replacements', async () => {
        const { database, added } = makeAdditionDatabase()
        const firstPublish = deferred<void>()
        const publish = vi.fn()
            .mockImplementationOnce(() => firstPublish.promise)
            .mockResolvedValue(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: async () => ({ publish, dispose: vi.fn(async () => undefined) }),
            },
        })
        coordinator.initialize(1)
        const saving = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        database.characters[0].name = 'Selected during publish'
        added.name = 'Added during publish'
        firstPublish.resolve()
        await saving

        expect(commit).toHaveBeenCalledTimes(3)
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Selected during publish',
        })
        expect(commit.mock.calls[2][0].replaceCharacter).toMatchObject({
            chaId: 'char-added',
            name: 'Added during publish',
        })
    })

    it('publishes only the newest revision after addition edits committed while offline', async () => {
        const { database, added } = makeAdditionDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn(async (_revision: number) => handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')
        added.name = 'Edited while offline'
        coordinator.markPersistentDataDirty(1)
        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin.mock.calls.map((call) => call[0])).toEqual([2, 3])
        expect(publish).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].replaceCharacter.name).toBe('Edited while offline')
    })

    it('gives an addition requested during an older failing publication its own serialized turn', async () => {
        const { database, added } = makeAdditionDatabase()
        const oldPublish = deferred<void>()
        const reported: Array<Promise<void> | null> = []
        const publish = vi.fn()
            .mockImplementationOnce(() => oldPublish.promise)
            .mockResolvedValue(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
            officialPublisher: {
                pin: async () => ({ publish, dispose: vi.fn(async () => undefined) }),
            },
        })
        coordinator.initialize(1)
        database.username = 'Older change'
        coordinator.markPersistentDataDirty(1)
        const olderFlush = coordinator.flushPendingData('older')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        const install = vi.fn(() => database.characters.push(added))
        const addition = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install,
        }, 'new-character')

        expect(addition).not.toBe(olderFlush)
        expect(reported).toEqual([olderFlush, addition])
        expect(install).not.toHaveBeenCalled()
        oldPublish.reject(new Error('older publication failed'))
        await expect(olderFlush).rejects.toThrow('older publication failed')
        await addition
        await Promise.resolve()

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(reported).toEqual([olderFlush, addition, null])
    })

    it('keeps an installed addition dirty after local failure for one explicit retry', async () => {
        const { database, added } = makeAdditionDatabase()
        const install = vi.fn(() => database.characters.push(added))
        const commit = vi.fn().mockRejectedValueOnce(new Error('write failed')).mockResolvedValueOnce({ revision: 2 })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 7,
            install,
        }, 'new-character')).rejects.toThrow('write failed')
        await coordinator.flushPendingData('retry')

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(coordinator.revision).toBe(2)
    })

    it.each([
        Object.assign(new Error('invalid character'), { code: 'validation' }),
        new UnsaveableValueError('character', 'synthetic invalid value'),
        new PayloadTooLargeError('character', 100_000_000),
    ])('withdraws a permanently refused addition and preserves unrelated edits (%s)', async (refusal) => {
        const {database, added} = makeAdditionDatabase()
        const storage = await createPersistentStoreHarness(structuredClone(database))
        const original = storage.store.commit.bind(storage.store)
        vi.spyOn(storage.store, 'commit').mockImplementation(async (input) => {
            if (input.addCharacter) throw refusal
            return original(input)
        })
        const coordinator = new SaveCoordinator({store: storage.store,
            captureRoot: () => captureRoot(database), captureSelectedCharacter: () => database.characters[0],
            captureCharacter: id => database.characters.find(character => character.chaId === id) ?? null,
            replaceDatabase: replacement => Object.assign(database, replacement),
            publishCharacterMutation: state => {
                database.characters = database.characters.filter(character => character.chaId !== state.characterId)
                Object.assign(database, state.root)
            },
        })
        coordinator.initialize(storage.revision)
        database.username = 'Unrelated edit'
        await expect(coordinator.commitCharacterAddition({characterId: added.chaId, estimatedBytes: 1,
            install: () => database.characters.push(added)}, 'refused-addition')).rejects.toBe(refusal)
        expect(database.characters.some(character => character.chaId === added.chaId)).toBe(false)
        await coordinator.flushPendingData('save-after-refusal')
        expect((await storage.store.readRoot()).value.username).toBe('Unrelated edit')
        expect(await storage.store.readCharacter(added.chaId)).toBeNull()
    })

    it('releases the reservation when install throws so later additions still run', async () => {
        const { database, added } = makeAdditionDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: 'char-broken',
            estimatedBytes: 1,
            install: () => {
                throw new Error('install failed')
            },
        }, 'broken')).rejects.toThrow('install failed')
        expect(commit).not.toHaveBeenCalled()

        await coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].addCharacter).toMatchObject({ chaId: 'char-added' })
    })

    it.each([false, true])('retains an addition whose commit response was lost when retry reports a permanent refusal (read fails: %s)', async (readFails) => {
        const {database, added} = makeAdditionDatabase()
        const storage = await createPersistentStoreHarness(structuredClone(database))
        const original = storage.store.commit.bind(storage.store)
        const refused = Object.assign(new Error('character already exists'), {code: 'validation'})
        let first = true
        vi.spyOn(storage.store, 'commit').mockImplementation(async input => {
            if (!first) throw refused
            first = false
            await original(input)
            throw new Error('commit response lost')
        })
        const coordinator = new SaveCoordinator({store: storage.store,
            captureRoot: () => captureRoot(database), captureSelectedCharacter: () => database.characters[0],
            captureCharacter: id => database.characters.find(character => character.chaId === id) ?? null,
            replaceDatabase: replacement => Object.assign(database, replacement),
            publishCharacterMutation: state => { database.characters = database.characters.filter(character => character.chaId !== state.characterId) },
        })
        coordinator.initialize(storage.revision)
        await expect(coordinator.commitCharacterAddition({characterId: added.chaId, estimatedBytes: 1,
            install: () => database.characters.push(added)}, 'unknown-addition')).rejects.toThrow('commit response lost')
        if (readFails) {
            vi.spyOn(storage.store, 'readCharacter').mockRejectedValueOnce(new Error('read unavailable'))
            await expect(coordinator.flushPendingData('retry-without-confirmation')).rejects.toBe(refused)
            expect(database.characters.some(character => character.chaId === added.chaId)).toBe(true)
            expect(coordinator.pendingWorkingSetRefreshRevision).toBeNull()
        }
        await expect(coordinator.flushPendingData('retry-unknown-addition')).rejects.toBe(refused)
        expect(database.characters.some(character => character.chaId === added.chaId)).toBe(true)
        expect((await storage.store.readCharacter(added.chaId))?.value.name).toBe(added.name)
        expect(coordinator.pendingWorkingSetRefreshRevision).toBe(storage.revision + 1)
    })

    it.each([
        ['another writer changed the replaced conversation', { kind: 'conversation', key1: 'char-a', key2: 'one' }, 1],
        ['another writer changed only a sibling conversation', { kind: 'conversation', key1: 'char-a', key2: 'two' }, 2],
        ['only an unrelated record moved', { kind: 'asset', key1: 'assets/synthetic.png', key2: '' }, 2],
    ])('resends a replaced conversation after a conflict only when %s', async (_name, change, commits) => {
        const database = makeChattyDatabase()
        const conflict = new RevisionConflictError(2, 3)
        const commit = vi.fn(async ({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) throw conflict
            return { revision: expectedRevision + 1 }
        })
        const readWorkingSetChangePage = vi.fn(async () => [change])
        const store = makeStore(commit, {
            acquireRevision: vi.fn(async (revision: number) => createLeaseStub({
                revision, readWorkingSetChangePage, release: vi.fn(async () => undefined),
            })),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)
        database.characters[0].chats[0].name = 'Renamed'

        const flushed = coordinator.flushPendingData('replaced-conversation-conflict')
        if (commits === 1) await expect(flushed).rejects.toBe(conflict)
        else await flushed

        expect(commit.mock.calls[0][0]).toMatchObject({ conversations: [expect.objectContaining({
            type: 'replace-range', characterId: 'char-a', conversationId: 'one', conversation: expect.objectContaining({ name: 'Renamed' }),
        })] })
        expect(commit).toHaveBeenCalledTimes(commits)
        expect(readWorkingSetChangePage).toHaveBeenCalledWith(2, null, expect.any(Number))
        if (commits === 2) expect(commit.mock.calls[1][0]).toMatchObject({ expectedRevision: 3 })
    })

    it('retries a revision conflict once and preserves a failed addition when the conflict cannot advance', async () => {
        const { database, added } = makeAdditionDatabase()
        const gate = deferred<{ revision: number }>()
        const conflict = new RevisionConflictError(1, 2)
        const commit = vi.fn(() => gate.promise)
        const coordinator = new SaveCoordinator({
            // A lease without a change index cannot show what moved, so the conflicting commit is resent.
            store: makeStore(commit, {
                acquireRevision: async (revision) => createLeaseStub({ revision, release: async () => undefined }),
            }),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const first = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 9,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        gate.reject(conflict)
        await expect(first).rejects.toBe(conflict)
        expect(commit).toHaveBeenCalledTimes(2)

        const secondInstall = vi.fn()
        await expect(coordinator.commitCharacterAddition({
            characterId: 'char-other',
            estimatedBytes: 1,
            install: secondInstall,
        }, 'other-character')).rejects.toBe(conflict)

        expect(secondInstall).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(3)
        expect(coordinator.revision).toBe(2)
        expect(coordinator.pendingBytes).toBe(9)
    })

    it('lets a later import succeed after a transient addition failure', async () => {
        const { database, added } = makeAdditionDatabase()
        const other = structuredClone(added)
        other.chaId = 'char-other'
        other.name = 'Other'
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('write failed')

        await coordinator.commitCharacterAddition({
            characterId: other.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(other),
        }, 'other-character')

        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(commit.mock.calls[2][0].addCharacter).toMatchObject({ chaId: 'char-other' })
        expect(coordinator.revision).toBe(3)
    })

    it('defers a second import that starts while the first is still committing', async () => {
        const { database, added } = makeAdditionDatabase()
        const other = structuredClone(added)
        other.chaId = 'char-other'
        other.name = 'Other'
        const gate = deferred<{ revision: number }>()
        let commits = 0
        const commit = vi.fn(async () => {
            commits += 1
            return commits === 1 ? gate.promise : { revision: 3 }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const first = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 9,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        let installedSecond = false
        const second = coordinator.commitCharacterAddition({
            characterId: other.chaId,
            estimatedBytes: 1,
            install: () => {
                installedSecond = true
                database.characters.push(other)
            },
        }, 'other-character')
        expect(installedSecond).toBe(false)

        gate.resolve({ revision: 2 })
        await first
        await second

        expect(installedSecond).toBe(true)
        expect(database.characters.map((item) => item.chaId)).toContain('char-other')
    })

    it('rejects an old-authority queued addition and accepts a fresh request after replacement', async () => {
        let database = makeDatabase()
        const replacement = makeDatabase()
        replacement.username = 'Replacement'
        const replacementGate = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = createStoreStub({
            commit,
            replaceFromDatabase: vi.fn(() => replacementGate.promise),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => {
                database = structuredClone(candidate)
            },
        })
        coordinator.initialize(1)
        const replacing = coordinator.replacePersistentDatabase(replacement, 'replace')
        const install = vi.fn(() => {
            const added = structuredClone(database.characters[0])
            added.chaId = 'char-added'
            database.characters.push(added)
        })
        expect(() => coordinator.commitCharacterAddition({
            characterId: 'char-added',
            estimatedBytes: 1,
            install,
        }, 'new-character')).toThrow(/replacement is active/i)
        expect(install).not.toHaveBeenCalled()
        replacementGate.resolve({ revision: 2 })
        await replacing
        expect(install).not.toHaveBeenCalled()
        expect(commit).not.toHaveBeenCalled()

        await coordinator.commitCharacterAddition({
            characterId: 'char-added', estimatedBytes: 1, install,
        }, 'fresh-addition-after-replacement')
        expect(install).toHaveBeenCalledOnce()
        expect(database.username).toBe('Replacement')
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            addCharacter: { chaId: 'char-added' },
        })
    })

    it('drains a failed addition before replacement and preserves it when the drain fails', async () => {
        const { database, added } = makeAdditionDatabase()
        const commit = vi.fn().mockRejectedValue(new Error('addition failed'))
        const replaceFromDatabase = vi.fn()
            .mockRejectedValueOnce(new Error('replacement failed'))
            .mockResolvedValueOnce({ revision: 3 })
        const store = createStoreStub({ commit, replaceFromDatabase })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => Object.assign(database, structuredClone(candidate)),
        })
        coordinator.initialize(1)
        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('addition failed')
        await expect(coordinator.replacePersistentDatabase(makeDatabase(), 'failed-drain')).rejects.toThrow('addition failed')
        await expect(coordinator.flushPendingData('still-pending')).rejects.toThrow('addition failed')
        expect(commit).toHaveBeenCalledTimes(3)
        expect(replaceFromDatabase).not.toHaveBeenCalled()

        commit.mockResolvedValue({ revision: 2 })
        await coordinator.flushPendingDataLocally('retry-addition')
        await expect(coordinator.replacePersistentDatabase(makeDatabase(), 'failed-replace')).rejects.toThrow('replacement failed')
        expect(database.characters.map((value) => value.chaId)).toContain(added.chaId)
        await coordinator.replacePersistentDatabase(makeDatabase(), 'successful-replace')
        await coordinator.flushPendingData('clean')
        expect(coordinator.revision).toBe(3)
        expect(commit).toHaveBeenCalledTimes(4)
        expect(replaceFromDatabase).toHaveBeenCalledTimes(2)
    })

    it('authoritative replacement supersedes a locally added character after remote failure', async () => {
        let { database, added } = makeAdditionDatabase()
        const staleHandle = {
            publish: vi.fn().mockRejectedValue(new Error('offline')),
            dispose: vi.fn(async () => undefined),
        }
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = createStoreStub({
            commit,
            replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => {
                database = structuredClone(candidate)
            },
            officialPublisher: { pin: async () => staleHandle },
        })
        coordinator.initialize(1)
        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')

        await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')
        await coordinator.flushPendingData('clean')

        expect(database.characters.map((character) => character.chaId)).toEqual(['char-a'])
        expect(commit).toHaveBeenCalledOnce()
        expect(staleHandle.dispose).toHaveBeenCalledOnce()
    })

    it('does not resurrect a character the replacement removed', async () => {
        vi.useFakeTimers()
        try {
            let db = makeDatabase()
            const replacementGate = deferred<{ revision: number }>()
            const store = createStoreStub({
                commit: vi.fn(),
                replaceFromDatabase: vi.fn(() => replacementGate.promise),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(db),
                captureSelectedCharacter: () => db.characters[0] ?? null,
                replaceDatabase: (replacement) => {
                    db = replacement
                },
            })
            coordinator.initialize(1)

            const candidate = makeDatabase()
            candidate.characters = []
            const replacing = coordinator.replacePersistentDatabase(candidate, 'remove-character')
            expect(() => {
                coordinator.assertPersistentMutationAllowed()
                db.characters[0].name = 'Edited after enqueue'
                coordinator.markPersistentDataDirty(1)
            }).toThrow(/replacement is active/i)
            replacementGate.resolve({ revision: 2 })
            await replacing

            expect(db.characters).toEqual([])
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it('reports a successful replacement revision and reports nothing on failure', async () => {
        let database = makeDatabase()
        const revisions: number[] = []
        const store = makeStore()
        vi.mocked(store.replaceFromDatabase)
            .mockResolvedValueOnce({ revision: 8 })
            .mockRejectedValueOnce(new Error('replace failed'))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
            onLocalRevision: (revision) => revisions.push(revision),
        })
        coordinator.initialize(7)

        await coordinator.replacePersistentDatabase(makeDatabase(), 'first')
        await expect(
            coordinator.replacePersistentDatabase(makeDatabase(), 'second'),
        ).rejects.toThrow('replace failed')

        expect(revisions).toEqual([8])
    })

    it('ordinary_flush_keeps_later_edits_dirty across a failed trailing batch', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Initial preset' }] as Database['botPresets']
        database.pluginCustomStorage = { payload: { value: 'initial' } }
        database.characters[0].chats = [{
            id: 'chat-a', name: 'Chat', message: [{ role: 'user', data: 'initial' }],
        }] as Chat[]
        const firstStarted = deferred<void>()
        const firstFinished = deferred<{ revision: number }>()
        const failure = new Error('trailing batch failed')
        const commit = vi.fn()
            .mockImplementationOnce(() => {
                firstStarted.resolve()
                return firstFinished.promise
            })
            .mockRejectedValueOnce(failure)
            .mockResolvedValueOnce({ revision: 3 })
        const store = makeStore(commit)
        store.readCharacter = vi.fn()
        store.readConversation = vi.fn()
        const captureCharacter = vi.fn(() => null)
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const edit = (value: string) => {
            database.username = value
            database.botPresets[0].name = value
            database.pluginCustomStorage.payload = { value }
            database.characters[0].chats[0].message[0].data = value
            coordinator.markPersistentDataDirty(25)
        }
        edit('first')
        const flushing = coordinator.flushPendingDataLocally('frozen-batches')
        const rejected = expect(flushing).rejects.toBe(failure)
        await firstStarted.promise
        const firstBatch = structuredClone(commit.mock.calls[0][0])
        edit('second')
        expect(commit.mock.calls[0][0]).toEqual(firstBatch)
        firstFinished.resolve({ revision: 2 })
        await rejected

        expect(coordinator.revision).toBe(2)
        expect(coordinator.pendingBytes).toBe(25)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0]).toEqual(firstBatch)
        expect(committedChanges(firstBatch).rootChanges).toEqual({ username: 'first' })
        const secondBatch = structuredClone(commit.mock.calls[1][0])
        expect(committedChanges(secondBatch).rootChanges).toEqual({ username: 'second' })
        expect(secondBatch).toMatchObject({
            expectedRevision: 2,
            replacePresets: [{ name: 'second' }],
            pluginStorage: [{
                type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: 'payload', value: { value: 'second' },
            }],
            conversations: [expect.objectContaining({
                characterId: 'char-a', conversationId: 'chat-a',
                messages: [{ role: 'user', data: 'second' }],
            })],
        })

        await coordinator.flushPendingDataLocally('retry-trailing-batch')
        expect(commit).toHaveBeenNthCalledWith(3, secondBatch)
        expect(coordinator.revision).toBe(3)
        expect(coordinator.pendingBytes).toBe(0)
        await coordinator.flushPendingDataLocally('clean-frozen-batches')
        expect(commit).toHaveBeenCalledTimes(3)
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(store.readCharacter).not.toHaveBeenCalled()
        expect(store.readConversation).not.toHaveBeenCalled()
        expect(captureCharacter).not.toHaveBeenCalled()
    })

    it('runs trailing commits for mutations made during every deferred commit', async () => {
        const database = makeDatabase()
        const first = deferred<{ revision: number }>()
        const second = deferred<{ revision: number }>()
        const commit = vi
            .fn()
            .mockImplementationOnce(() => first.promise)
            .mockImplementationOnce(() => second.promise)
            .mockResolvedValueOnce({ revision: 4 })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'one'
        coordinator.markPersistentDataDirty(1)
        const flushing = coordinator.flushPendingData('test')
        await Promise.resolve()
        database.username = 'two'
        coordinator.markPersistentDataDirty(1)
        first.resolve({ revision: 2 })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledTimes(2))
        database.username = 'three'
        coordinator.markPersistentDataDirty(1)
        second.resolve({ revision: 3 })

        await flushing

        expect(commit.mock.calls.map((call) => call[0].expectedRevision)).toEqual([1, 2, 3])
        expect(commit.mock.calls.map((call) => committedChanges(call[0]).rootChanges)).toEqual(
            ['one', 'two', 'three'].map((username) => ({ username })),
        )
    })

    it.each([
        new Error('write failed'),
    ])('keeps failed work dirty without retrying for %s', async (error) => {
        const database = makeDatabase()
        const commit = vi.fn().mockRejectedValue(error)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'Uncommitted'
        coordinator.markPersistentDataDirty(25)

        await expect(coordinator.flushPendingData('test')).rejects.toBe(error)

        expect(commit).toHaveBeenCalledTimes(1)
        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(25)
    })

    it('commits locally even when the official publish fails and retries the pin later', async () => {
        const database = makeDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn().mockResolvedValue(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        database.username = 'Offline edit'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.flushPendingData('offline')).rejects.toThrow('offline')

        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)

        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(commit).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledTimes(1)
        expect(publish).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenCalledWith(2)
    })

    it('acknowledges a local generation commit without waiting for an unbounded official publisher', async () => {
        const database = makeDatabase()
        const scheduled: Array<() => void> = []
        const cleared = new Set<symbol>()
        const clock = {
            setTimeout: (callback: () => void) => {
                const handle = Symbol('timer')
                scheduled.push(() => {
                    if (!cleared.has(handle)) callback()
                })
                return handle
            },
            clearTimeout: (handle: unknown) => {
                if (typeof handle === 'symbol') cleared.add(handle)
            },
        }
        const pin = vi.fn(() => new Promise<never>(() => undefined))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            clock,
        })
        coordinator.initialize(1)
        database.username = 'Durable local generation'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingDataLocally('generation-completion')

        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        expect(pin).not.toHaveBeenCalled()
        expect(scheduled.length).toBeGreaterThan(1)
    })

    it('does not let an in-flight unbounded publication block a newer local generation commit', async () => {
        const database = makeDatabase()
        const publish = vi.fn(() => new Promise<never>(() => undefined))
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'First local revision'
        coordinator.markPersistentDataDirty(1)

        void coordinator.flushPendingData('ordinary-save')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        database.username = 'Completed generation revision'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingDataLocally('generation-completion')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(3)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
        expect(pin).toHaveBeenCalledWith(2)
    })

    it('does not queue local acknowledgement behind a publication operation that has not started yet', async () => {
        const database = makeDatabase()
        const publish = vi.fn(() => new Promise<never>(() => undefined))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) })),
            },
        })
        coordinator.initialize(1)
        database.username = 'Completed generation'
        coordinator.markPersistentDataDirty(1)

        void coordinator.flushPendingData('ordinary-save')
        await coordinator.flushPendingDataLocally('generation-completion')
        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
    })

    it('does not retry stalled official publication cleanup during local acknowledgement', async () => {
        const database = makeDatabase()
        const cleanupFailure = new Error('cleanup offline')
        const dispose = vi.fn()
            .mockRejectedValueOnce(cleanupFailure)
            .mockImplementationOnce(() => new Promise<never>(() => undefined))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: vi.fn(async () => ({
                    publish: vi.fn(async () => undefined),
                    dispose,
                })),
            },
            clock: {
                setTimeout: () => Symbol('timer'),
                clearTimeout: () => undefined,
            },
        })
        coordinator.initialize(1)
        database.username = 'Published revision'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('ordinary-save')

        database.username = 'Completed generation'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingDataLocally('generation-completion')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(dispose).toHaveBeenCalledOnce()
    })

    it('serializes publication completion with an in-flight local generation commit', async () => {
        const database = makeDatabase()
        const publication = deferred<void>()
        const generationCommit = deferred<{ revision: number }>()
        const publish = vi.fn(() => publication.promise)
        const commit = vi.fn()
            .mockResolvedValueOnce({ revision: 2 })
            .mockImplementationOnce(() => generationCommit.promise)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) })),
            },
        })
        coordinator.initialize(1)
        database.username = 'First revision'
        coordinator.markPersistentDataDirty(1)
        const ordinaryFlush = coordinator.flushPendingData('ordinary-save')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())

        database.username = 'Completed generation'
        coordinator.markPersistentDataDirty(1)
        const localFlush = coordinator.flushPendingDataLocally('generation-completion')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledTimes(2))
        publication.resolve()
        const ordinaryState = await Promise.race([
            ordinaryFlush.then(() => 'settled', () => 'rejected'),
            new Promise<string>((resolve) => setTimeout(() => resolve('pending'), 25)),
        ])

        expect(ordinaryState).toBe('pending')
        expect(commit).toHaveBeenCalledTimes(2)
        generationCommit.resolve({ revision: 3 })
        await localFlush
        await ordinaryFlush

        expect(coordinator.revision).toBe(3)
        expect(coordinator.hasPendingOfficialPublication).toBe(true)
    })

    it('rejects local generation acknowledgement when the PDS commit fails', async () => {
        const database = makeDatabase()
        const error = new Error('local PDS failed')
        const commit = vi.fn().mockRejectedValue(error)
        const pin = vi.fn()
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Uncommitted generation'
        coordinator.markPersistentDataDirty(25)
        await expect(
            coordinator.flushPendingDataLocally('generation-completion'),
        ).rejects.toBe(error)

        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(25)
        expect(coordinator.hasPendingOfficialPublication).toBe(false)
        expect(pin).not.toHaveBeenCalled()
    })

    it('autonomously retries a failed official publication while preserving its lease', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn()
                .mockRejectedValueOnce(new Error('offline'))
                .mockResolvedValueOnce(undefined)
            const handle = {
                publish,
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn(async () => handle)
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'Autonomous retry'
            coordinator.markPersistentDataDirty(1)

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledOnce()
            expect(pin).toHaveBeenCalledOnce()
            expect(publish).toHaveBeenCalledOnce()
            expect(handle.dispose).not.toHaveBeenCalled()
            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            expect(coordinator.pendingBytes).toBe(0)

            await vi.advanceTimersByTimeAsync(2_999)
            expect(publish).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(1)

            expect(pin).toHaveBeenCalledOnce()
            expect(publish).toHaveBeenCalledTimes(2)
            expect(handle.dispose).toHaveBeenCalledOnce()
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('rearms an official publication retry when its timer fires during a replacement fence', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn()
                .mockRejectedValueOnce(new Error('offline'))
                .mockResolvedValueOnce(undefined)
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(vi.fn(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: {
                    pin: vi.fn(async () => ({
                        publish,
                        dispose: vi.fn(async () => undefined),
                    })),
                },
                onBackgroundError,
            })
            coordinator.initialize(1)
            database.username = 'Pending publication'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('initial')).rejects.toThrow('offline')
            const fence = await coordinator.acquireCommittedWorkingSetRefreshFence()

            await vi.advanceTimersByTimeAsync(3_000)

            expect(publish).toHaveBeenCalledOnce()
            expect(onBackgroundError).not.toHaveBeenCalled()
            coordinator.releaseDestructiveReplacementFence(fence)
            await vi.advanceTimersByTimeAsync(3_000)
            expect(publish).toHaveBeenCalledTimes(2)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('supersedes a failed publication with the newer local revision', async () => {
        const database = makeDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('remote')).mockResolvedValueOnce(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn().mockResolvedValue(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        database.username = 'Local one'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote')
        database.username = 'Local two'
        coordinator.markPersistentDataDirty(1)

        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(publish).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenNthCalledWith(1, 2)
        expect(pin).toHaveBeenNthCalledWith(2, 3)
        expect(handle.dispose).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].expectedRevision).toBe(2)
    })

    it('retries cleanup of a superseded publication without blocking the newer revision', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let nowValue = 0
            const staleHandle = {
                publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
                dispose: vi.fn()
                    .mockRejectedValueOnce(new Error('cleanup failed'))
                    .mockResolvedValueOnce(undefined),
            }
            const currentHandle = {
                publish: vi.fn(async () => undefined),
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn()
                .mockResolvedValueOnce(staleHandle)
                .mockResolvedValueOnce(currentHandle)
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
                now: () => nowValue,
                onBackgroundError,
            })
            coordinator.initialize(1)
            database.username = 'First revision'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote failed')

            nowValue = 4_000
            database.username = 'Newer revision'
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingData('newer')

            expect(pin).toHaveBeenNthCalledWith(2, 3)
            expect(currentHandle.publish).toHaveBeenCalledOnce()
            expect(currentHandle.dispose).toHaveBeenCalledOnce()
            expect(staleHandle.dispose).toHaveBeenCalledOnce()
            expect(onBackgroundError).toHaveBeenCalledWith(expect.objectContaining({
                message: 'cleanup failed',
            }))

            await vi.advanceTimersByTimeAsync(3_000)

            expect(staleHandle.dispose).toHaveBeenCalledTimes(2)
            expect(currentHandle.publish).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('spaces official publishes at least three seconds apart and publishes the newest revision', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(pin).toHaveBeenCalledWith(2)

            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Third edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledTimes(3)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await vi.advanceTimersByTimeAsync(2000)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(4)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('adopts a validated materialized database without dropping a throttled official publication', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            expect(coordinator.adoptMaterializedDatabase(
                coordinator.revision,
                coordinator.mutationGeneration,
                structuredClone(database),
            )).toBe(true)

            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            await vi.advanceTimersByTimeAsync(2500)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(3)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('adopts materialized baselines without traversing inactive conversation bodies', async () => {
        let database = makeDatabase()
        database.pluginCustomStorage = {}
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () =>
                database.characters.find((candidate) => candidate.chaId === 'char-a') ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(7)

        const authoritative = makeDatabase()
        authoritative.username = 'Adopted root'
        authoritative.botPresets = [
            { name: 'Adopted preset', mainPrompt: 'Before preset edit' },
        ] as Database['botPresets']
        authoritative.pluginCustomStorage = JSON.parse(
            '{"zeta":0,"__proto__":false,"alpha":""}',
        )
        authoritative.characters[0].chats = [{
            id: 'chat-a',
            name: 'Selected chat',
            note: '',
            localLore: [],
            message: [{ chatId: 'message-a', role: 'char', data: 'Before message edit' }],
        } as Chat]
        const inactive = structuredClone(authoritative.characters[0])
        inactive.chaId = 'char-b'
        inactive.name = 'Inactive'
        inactive.chats = [{
            id: 'chat-b',
            name: 'Inactive chat',
            note: '',
            localLore: [],
            message: [{ chatId: 'message-b', role: 'char', data: 'Do not traverse' }],
        } as Chat]
        Object.defineProperty(inactive.chats[0].message[0], 'data', {
            enumerable: true,
            get: () => {
                throw new Error('inactive conversation body was traversed')
            },
        })
        authoritative.characters.push(inactive)

        expect(coordinator.adoptMaterializedDatabase(
            7,
            coordinator.mutationGeneration,
            authoritative,
        )).toBe(true)
        database = authoritative

        await coordinator.flushPendingData('clean-adopted-baseline')
        expect(commit).not.toHaveBeenCalled()
        expect(Object.keys(database.pluginCustomStorage)).toEqual([
            'zeta',
            '__proto__',
            'alpha',
        ])
        expect(Object.hasOwn(database.pluginCustomStorage, '__proto__')).toBe(true)
        expect(database.pluginCustomStorage.__proto__).toBe(false)
        expect(database.pluginCustomStorage).toMatchObject({
            zeta: 0,
            alpha: '',
        })

        database.username = 'Edited root'
        database.botPresets[0].mainPrompt = 'After preset edit'
        database.pluginCustomStorage.__proto__ = 0
        database.characters[0].chats[0].message[0].data = 'After message edit'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('after-adopted-mutations')

        expect(commit).toHaveBeenCalledOnce()
        expect(committedChanges(commit.mock.calls[0][0])).toEqual({
            expectedRevision: 7,
            rootChanges: { username: 'Edited root' },
            pluginStorage: [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '__proto__', value: 0 }],
            replacePresets: [{ name: 'Adopted preset', mainPrompt: 'After preset edit' }],
            conversations: [
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'chat-a',
                    start: 0,
                    deleteCount: 1,
                    messages: [
                        {
                            chatId: 'message-a',
                            role: 'char',
                            data: 'After message edit',
                        },
                    ],
                    conversation: {
                        id: 'chat-a',
                        name: 'Selected chat',
                        note: '',
                        localLore: [],
                    },
                },
            ],
        })
    })

    it('publishes immediately on explicit request despite the publish interval', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await coordinator.publishCurrentOfficialRevision()

            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(3)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
            await vi.advanceTimersByTimeAsync(4000)
            expect(pin).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('drops a deferred publication when an authoritative replacement supersedes it', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const store = createStoreStub({
                commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 9 })),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')

            expect(coordinator.hasPendingOfficialPublication).toBe(false)
            await vi.advanceTimersByTimeAsync(4000)
            expect(pin).toHaveBeenCalledTimes(1)
        } finally {
            vi.useRealTimers()
        }
    })

    it('retargets a throttled publication to a local replacement revision', async () => {
        vi.useFakeTimers()
        try {
            let database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const store = createStoreStub({
                commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 9 })),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => {
                    database = replacement
                },
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            const replacement = makeDatabase()
            replacement.username = 'Local compatibility replacement'
            await coordinator.replacePreparedPersistentDatabase(
                async () => replacement,
                'plugin-profile-change',
                { publishOfficial: true },
            )

            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            await vi.advanceTimersByTimeAsync(2500)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(9)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('exposes whether an official publication is still pending', async () => {
        const database = makeDatabase()
        let offline = true
        let nowValue = 0
        const publish = vi.fn(async () => {
            if (offline) throw new Error('offline')
        })
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        expect(coordinator.hasPendingOfficialPublication).toBe(false)

        database.username = 'Offline edit'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('offline')).rejects.toThrow('offline')
        expect(coordinator.hasPendingOfficialPublication).toBe(true)

        offline = false
        nowValue = 4000
        await coordinator.flushPendingData('online')
        expect(coordinator.hasPendingOfficialPublication).toBe(false)
    })

    it('reports repeated background publish failures once until a flush succeeds', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let offline = true
            const publish = vi.fn(async () => {
                if (offline) throw new Error('offline')
            })
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
                onBackgroundError,
            })
            coordinator.initialize(1)

            database.username = 'Edit one'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(onBackgroundError).toHaveBeenCalledTimes(1)

            database.username = 'Edit two'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(onBackgroundError).toHaveBeenCalledTimes(1)

            offline = false
            database.username = 'Edit three'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            await vi.advanceTimersByTimeAsync(2000)

            offline = true
            database.username = 'Edit four'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            await vi.advanceTimersByTimeAsync(2500)
            expect(onBackgroundError).toHaveBeenCalledTimes(2)
            expect(commit).toHaveBeenCalledTimes(4)
        } finally {
            vi.useRealTimers()
        }
    })

    it('takes the background save path for a failed generation checkpoint and rejects one during a fence', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
                .mockRejectedValueOnce(new Error('synthetic I/O failure'))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                onBackgroundError,
            })
            coordinator.initialize(1)
            database.username = 'Checkpointed edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.flushPendingDataLocallyForCheckpoint('generation-checkpoint')).resolves.toBe(false)
            expect(onBackgroundError).toHaveBeenCalledOnce()
            expect(commit).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(2_000)
            expect(commit).toHaveBeenCalledTimes(2)
            expect(coordinator.hasPendingPersistenceWork).toBe(false)

            const fence = await coordinator.acquireCommittedWorkingSetRefreshFence()
            await expect(coordinator.flushPendingDataLocallyForCheckpoint('generation-checkpoint'))
                .rejects.toBeInstanceOf(PersistentMutationFencedError)
            coordinator.releaseDestructiveReplacementFence(fence)
            expect(onBackgroundError).toHaveBeenCalledOnce()
            expect(commit).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('retries pinning the committed revision before creating a newer local revision', async () => {
        const database = makeDatabase()
        const handle = {
            publish: vi.fn(async () => undefined),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn().mockRejectedValueOnce(new Error('pin failed')).mockResolvedValueOnce(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Committed locally'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.flushPendingData('first')).rejects.toThrow('pin failed')
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenNthCalledWith(1, 2)
        expect(pin).toHaveBeenNthCalledWith(2, 2)
        expect(commit).toHaveBeenCalledTimes(1)
        expect(handle.publish).toHaveBeenCalledTimes(1)
        expect(handle.dispose).toHaveBeenCalledTimes(1)
    })

    it('disposes a failed pre-replacement publication so it can never publish later', async () => {
        const database = makeDatabase()
        const staleHandle = {
            publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn(async () => staleHandle)
        const store = createStoreStub({
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Local revision'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('publish')).rejects.toThrow('remote failed')

        const replacement = makeDatabase()
        replacement.username = 'Authoritative replacement'
        await coordinator.replacePersistentDatabase(replacement, 'replace')
        await coordinator.flushPendingData('clean')

        expect(staleHandle.dispose).toHaveBeenCalledTimes(1)
        expect(staleHandle.publish).toHaveBeenCalledTimes(1)
        expect(pin).toHaveBeenCalledTimes(1)
    })

    it('retries failed stale-publication cleanup after an authoritative replacement', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const staleHandle = {
                publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
                dispose: vi.fn()
                    .mockRejectedValueOnce(new Error('cleanup failed'))
                    .mockResolvedValueOnce(undefined),
            }
            const pin = vi.fn(async () => staleHandle)
            const store = createStoreStub({
                commit: vi.fn(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => Object.assign(database, replacement),
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'Local revision'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('publish')).rejects.toThrow('remote failed')

            await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')

            expect(staleHandle.dispose).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(3_000)
            expect(staleHandle.dispose).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(3_000)
            expect(staleHandle.dispose).toHaveBeenCalledTimes(2)
            expect(staleHandle.publish).toHaveBeenCalledOnce()
            expect(pin).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('rejects an earlier queued revision and saves new edits only after a fresh replacement', async () => {
        const database = makeDatabase()
        const firstCommit = deferred<{ revision: number }>()
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi
            .fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockResolvedValueOnce({ revision: 4 })
        const replaceFromDatabase = vi.fn(() => replacementWrite.promise)
        const store = createStoreStub({
            commit,
            replaceFromDatabase,
        })
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, replacement)
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(1)
        database.username = 'Commit before replacement'
        coordinator.markPersistentDataDirty(1)
        const flushing = coordinator.flushPendingData('active')
        const candidate = makeDatabase()
        candidate.username = 'Replacement'
        const rejected = expect(coordinator.replacePersistentDatabase(candidate, 'stale-replace'))
            .rejects.toBeInstanceOf(RevisionConflictError)
        expect(replaceFromDatabase).not.toHaveBeenCalled()
        firstCommit.resolve({ revision: 2 })
        await Promise.all([flushing, rejected])
        expect(replaceFromDatabase).not.toHaveBeenCalled()
        expect(database.username).toBe('Commit before replacement')

        const replacing = coordinator.replacePersistentDatabase(candidate, 'fresh-replace')
        await vi.waitFor(() => expect(replaceFromDatabase).toHaveBeenCalledTimes(1))
        expect(replaceFromDatabase).toHaveBeenCalledWith(candidate, 2)
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.characters[0].name = 'Blocked mutation'
        }).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 3 })
        await expect(replacing).resolves.toEqual({ kind: 'committed', revision: 3, projection: 'applied' })

        expect(commit).toHaveBeenCalledTimes(1)
        expect(replaceDatabase).toHaveBeenCalledExactlyOnceWith(candidate)
        expect(coordinator.revision).toBe(3)
        expect(coordinator.pendingBytes).toBe(0)
        database.characters[0].name = 'Mutation after capture'
        coordinator.markPersistentDataDirty(12)
        await coordinator.flushPendingData('post-replacement')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 3,
            replaceCharacter: { name: 'Mutation after capture' },
        })
        expect(coordinator.revision).toBe(4)
    })

    it('blocks concurrent root, preset and conversation edits during authoritative replacement', async () => {
        const database = makeDatabase()
        database.mainPrompt = 'Before prompt'
        database.botPresets = [
            { name: 'Before first', mainPrompt: 'first' },
            { name: 'Before second', mainPrompt: 'second' },
        ] as Database['botPresets']
        ;(database.characters[0] as character).desc = 'Before description'
        database.characters[0].chats = [
            { id: 'chat-a', name: 'First chat', note: '', localLore: [], message: [] },
            { id: 'chat-b', name: 'Second chat', note: '', localLore: [], message: [] },
        ]
        const candidate = structuredClone(database)
        candidate.username = 'Authoritative username'
        candidate.botPresets[1].mainPrompt = 'Authoritative second prompt'
        candidate.characters[0].name = 'Authoritative character name'
        candidate.characters[0].chats = [
            { ...candidate.characters[0].chats[1], name: 'Authoritative second chat' },
            candidate.characters[0].chats[0],
        ]
        const replacementWrite = deferred<{ revision: number }>()
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
        })
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, structuredClone(replacement))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(5)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'authoritative', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.mainPrompt = 'Later live prompt'
            database.botPresets[0].name = 'Later live first'
            ;(database.characters[0] as character).desc = 'Later live description'
            database.characters[0].chats[0].note = 'Later live first-chat note'
            coordinator.markPersistentDataDirty(1)
        }).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 6 })
        await replacing

        expect(database).toEqual(candidate)
        expect(database.mainPrompt).toBe('Before prompt')
        expect(database.botPresets[0].name).toBe('Before first')
        expect(database.characters[0].chats).toMatchObject([
            { id: 'chat-b', name: 'Authoritative second chat' },
            { id: 'chat-a', note: '' },
        ])
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
    })

    it('blocks a compatibility plugin edit during replacement and saves a fresh edit afterward', async () => {
        const database = makeDatabase()
        database.pluginCustomStorage = JSON.parse(
            '{"retained":"before","__proto__":"before"}',
        )
        const storagePrototype = Object.getPrototypeOf(database.pluginCustomStorage)
        const candidate = structuredClone(database)
        candidate.pluginCustomStorage.retained = 'replacement'
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit,
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
        })
        coordinator.initialize(5, database)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'plugin-rebase', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.pluginCustomStorage.__proto__ = 'blocked'
        }).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 6 })

        await replacing

        expect(Object.keys(database.pluginCustomStorage)).toEqual(['retained', '__proto__'])
        expect(database.pluginCustomStorage.retained).toBe('replacement')
        expect(Object.hasOwn(database.pluginCustomStorage, '__proto__')).toBe(true)
        expect(database.pluginCustomStorage.__proto__).toBe('before')
        expect(Object.getPrototypeOf(database.pluginCustomStorage)).toBe(storagePrototype)
        expect(commit).not.toHaveBeenCalled()

        database.pluginCustomStorage.__proto__ = 'later'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('plugin-after-replacement-save')

        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 6,
            pluginStorage: [{ type: 'set', owner: UNOWNED_PLUGIN_OWNER, key: '__proto__', value: 'later' }],
        })
    })

    it('keeps preset entities intact by blocking a rename during authoritative reorder', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
        })
        coordinator.initialize(6)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'preset-reorder', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.botPresets[0].name = 'Preset A2'
        }).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 7 })
        await replacing

        expect(database.botPresets).toMatchObject([
            { name: 'Preset B', mainPrompt: 'Candidate B' },
            { name: 'Preset A', mainPrompt: 'A' },
        ])
    })

    it('prevents a live edit from attaching to a renamed preset during replacement', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        candidate.botPresets[1].name = 'Preset A2'
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'ambiguous-rename', {
            authoritative: true,
        })
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.botPresets[0].mainPrompt = 'Later A'
        }).toThrow(/replacement is active/i)
        await expect(replacing).resolves.toMatchObject({ projection: 'applied', revision: 8 })
        expect(store.replaceFromDatabase).toHaveBeenCalledExactlyOnceWith(candidate, 7)
        expect(database.botPresets[0].mainPrompt).toBe('A')
    })

    it('blocks stale preset edits before an all-renamed reorder can attach them to another entity', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets = [
            { ...candidate.botPresets[1], name: 'Preset B2' },
            { ...candidate.botPresets[0], name: 'Preset A2' },
        ]
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'all-renamed-reorder', {
            authoritative: true,
        })
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.botPresets[0].temperature = 1.3
        }).toThrow(/replacement is active/i)
        await expect(replacing).resolves.toMatchObject({ projection: 'applied', revision: 8 })
        expect(store.replaceFromDatabase).toHaveBeenCalledExactlyOnceWith(candidate, 7)
        expect(database.botPresets[0]).not.toHaveProperty('temperature')
    })

    it.each(['add', 'delete'] as const)(
        'preserves an authoritative preset %s while a stale edit is blocked',
        async (operation) => {
            const database = makeDatabase()
            database.botPresets = [
                { name: 'Preset A', mainPrompt: 'A' },
                { name: 'Preset B', mainPrompt: 'B' },
            ] as Database['botPresets']
            const candidate = structuredClone(database)
            if (operation === 'add') {
                candidate.botPresets.splice(1, 0, {
                    name: 'Imported preset',
                    mainPrompt: 'Imported',
                } as Database['botPresets'][number])
            } else {
                candidate.botPresets.splice(0, 1)
            }
            const replacementWrite = deferred<{ revision: number }>()
            const store = createStoreStub({
                replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => Object.assign(database, replacement),
            })
            coordinator.initialize(7)

            const replacing = coordinator.replacePersistentDatabase(
                candidate,
                `preset-${operation}`,
                { authoritative: true },
            )
            await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
            expect(() => {
                coordinator.assertPersistentMutationAllowed()
                database.botPresets[1].mainPrompt = 'Later B'
            }).toThrow(/replacement is active/i)
            replacementWrite.resolve({ revision: 8 })
            await replacing

            expect(database.botPresets).toMatchObject(operation === 'add'
                ? [
                    { name: 'Preset A', mainPrompt: 'A' },
                    { name: 'Imported preset', mainPrompt: 'Imported' },
                    { name: 'Preset B', mainPrompt: 'B' },
                ]
                : [{ name: 'Preset B', mainPrompt: 'B' }])
        },
    )

    it('requires a read-only refresh instead of compensating an unguarded id-less preset edit', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Duplicate', mainPrompt: 'A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        })
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, structuredClone(replacement))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(8)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'ambiguous-presets', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        // Simulate a legacy writer that bypasses the admission guard before mutating.
        database.botPresets[0].mainPrompt = 'Later A'
        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 9 })

        await expect(replacing).resolves.toEqual({
            kind: 'committed', revision: 9, projection: 'refresh-required',
        })
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(database.botPresets).toMatchObject([
            { name: 'Duplicate', mainPrompt: 'Later A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ])
        expect(store.replaceFromDatabase).toHaveBeenCalledExactlyOnceWith(candidate, 8)
        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.pendingWorkingSetRefreshRevision).toBe(9)
        await expect(coordinator.flushPendingDataLocally('after-conflict')).rejects.toThrow(/replacement is active/i)
        expect(store.commit).not.toHaveBeenCalled()
    })

    it('does not save later stale edits after a committed replacement requires refresh', async () => {
        let database = makeDatabase()
        database.botPresets = [
            { name: 'Duplicate', mainPrompt: 'A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi.fn()
        const store = createStoreStub({
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit,
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
        })
        coordinator.initialize(8)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'pending-compensation', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Later A'
        expect(() => coordinator.markPersistentDataDirty(1)).toThrow(/replacement is active/i)
        replacementWrite.resolve({ revision: 9 })
        await expect(replacing).resolves.toEqual({
            kind: 'committed', revision: 9, projection: 'refresh-required',
        })
        expect(coordinator.hasDestructiveReplacementFence).toBe(false)
        expect(() => {
            coordinator.assertPersistentMutationAllowed()
            database.botPresets[0].mainPrompt = 'Newest A'
        }).toThrow(/replacement is active/i)
        await expect(coordinator.flushPendingDataLocally('no-compensation')).rejects.toThrow(/replacement is active/i)
        expect(coordinator.revision).toBe(9)
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(commit).not.toHaveBeenCalled()
    })

    it('re-arms ordinary autosave for edits made after a completed replacement', async () => {
        vi.useFakeTimers()
        try {
            let db = makeDatabase()
            const gate = deferred<{ revision: number }>()
            const commit = vi.fn()
                .mockImplementationOnce(() => gate.promise)
                .mockImplementation(async ({ expectedRevision }: { expectedRevision: number }) => ({
                    revision: expectedRevision + 1,
                }))
            const store = createStoreStub({
                commit,
                replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(db),
                captureSelectedCharacter: () => db.characters[0],
                replaceDatabase: (replacement) => {
                    db = replacement
                },
            })
            coordinator.initialize(1)
            db.username = 'Edit one'
            coordinator.markPersistentDataDirty(1)
            const flushing = coordinator.flushPendingData('first')
            await vi.advanceTimersByTimeAsync(0)
            const candidate = makeDatabase()
            candidate.username = 'Replacement'
            gate.resolve({ revision: 2 })
            await flushing
            await coordinator.replacePersistentDatabase(candidate, 'replace')
            db.username = 'Edit two'
            coordinator.markPersistentDataDirty(1)

            expect(commit).toHaveBeenCalledTimes(1)
            expect(db.username).toBe('Edit two')

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledTimes(2)
            const trailing = committedChanges(commit.mock.calls[1][0])
            expect(trailing.expectedRevision).toBe(3)
            expect(trailing.rootChanges).toEqual({ username: 'Edit two' })
        } finally {
            vi.useRealTimers()
        }
    })

    it('finishes flushing after the selected character is deselected', async () => {
        const database = makeDatabase()
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        selectedIndex = -1
        database.username = 'Deselected'
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('deselected')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(committedChanges(commit.mock.calls[0][0]).rootChanges).toEqual({ username: 'Deselected' })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(coordinator.pendingBytes).toBe(0)
    })

    it('rejects hydrated character adoption after the mutation generation changes', () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(4)
        const mutationGeneration = coordinator.mutationGeneration

        coordinator.markPersistentDataDirty(1)

        expect(coordinator.adoptHydratedCharacter(
            4,
            mutationGeneration,
            database.characters[0],
        )).toBe(false)
    })

    it('saves a live edit made before a separate hydrated copy is adopted', async () => {
        const database = makeChattyDatabase()
        const storage = await createPersistentStoreHarness(structuredClone(database))
        const hydrated = structuredClone(database.characters[0])
        const coordinator = new SaveCoordinator({
            store: storage.store,
            canonicalCapture: createPersistenceCanonicalCapture({
                root: () => captureRoot(database),
                presets: () => null,
                pluginStorage: () => null,
                character: () => database.characters[0],
                characters: () => database.characters,
            }),
            captureRoot: () => captureRoot(database),
            captureCharacters: () => database.characters,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(storage.revision)

        database.characters[0].chats[1].message[0].data = 'live edit before adoption'
        expect(coordinator.adoptHydratedCharacter(storage.revision, coordinator.mutationGeneration, hydrated)).toBe(true)
        await coordinator.flushPendingDataLocally('adoption-live-edit')

        expect((await storage.store.readConversation('char-a', 'two'))?.value.message[0].data)
            .toBe('live edit before adoption')
    })

    it('commits the newly selected character after the selection changes', async () => {
        const database = makeDatabase()
        const second = structuredClone(database.characters[0])
        second.chaId = 'char-b'
        second.name = 'Beta'
        database.characters.push(second)
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        selectedIndex = 1
        database.characters[1].name = 'Beta edited'
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('reselected')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({
            replaceCharacter: { chaId: 'char-b', name: 'Beta edited' },
        })
    })

    it('rescues the previous character edits when the selection switches directly', async () => {
        const database = makeDatabase()
        const second = structuredClone(database.characters[0])
        second.chaId = 'char-b'
        second.name = 'Beta'
        database.characters.push(second)
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        database.characters[0].name = 'Alpha edited'
        selectedIndex = 1
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('switched')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Alpha edited',
        })
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-b',
            name: 'Beta',
        })

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('does not publish or change baselines when replacement fails', async () => {
        const database = makeDatabase()
        const replaceDatabase = vi.fn()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = createStoreStub({
            commit,
            replaceFromDatabase: vi.fn().mockRejectedValue(new Error('replacement failed')),
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(7)
        await expect(
            coordinator.replacePersistentDatabase(makeDatabase(), 'replace'),
        ).rejects.toThrow('replacement failed')

        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(7)
        expect(coordinator.hasDestructiveReplacementFence).toBe(false)
        database.username = 'Still dirty'
        coordinator.markPersistentDataDirty(9)
        expect(coordinator.pendingBytes).toBe(9)
        await coordinator.flushPendingData('after-failure')
        const afterFailure = committedChanges(commit.mock.lastCall![0])
        expect(afterFailure.expectedRevision).toBe(7)
        expect(afterFailure.rootChanges).toEqual({ username: 'Still dirty' })
    })

})
