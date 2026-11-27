import { describe, expect, it, vi } from 'vitest'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { WindowedConversationOverlay } from './windowedConversationOverlay'
import type { Chat, Message, character } from './database.svelte'
import type {
    CharacterDetail,
    WorkingSetCommit,
} from './persistentDataStore'
import { MAX_NATIVE_REQUEST_BYTES, PayloadTooLargeError } from './nativePersistenceValue'
import { RevisionConflictError } from './persistentDataStore'
import type { WindowedConversationPersistenceAuthority } from './saveCoordinator'
import {
    captureRoot,
    deferred,
    makeDatabase,
    makeStore,
    SaveCoordinator,
} from './saveCoordinator.testSupport'
import { safeStructuredClone } from '../polyfill'
import type { ConversationRowsRewrite } from '../conversationViewportSource'

vi.mock('../polyfill', async (importOriginal) => {
    const actual = await importOriginal<typeof import('../polyfill')>()
    return { ...actual, safeStructuredClone: vi.fn(actual.safeStructuredClone) }
})

describe('SaveCoordinator', () => {
    describe('windowed selected-conversation persistence authority', () => {
        type TestWindowedAuthority = Omit<
            WindowedConversationPersistenceAuthority,
            'sessionToken'
        > & {
            sessionToken: any
        }

        function makeWindowedProjection() {
            const projectedConversation: Record<string, unknown> = {
                id: 'two',
                name: 'Two',
                localLore: [],
                note: '',
            }
            Object.defineProperty(projectedConversation, 'message', {
                enumerable: true,
                get: () => {
                    throw new Error('projected messages must not be traversed')
                },
            })
            return {
                character: {
                    type: 'character',
                    chaId: 'char-a',
                    name: 'Alpha',
                    chats: [projectedConversation],
                } as unknown as character,
                projectedConversation,
            }
        }

        function makeWindowedHarness(options: {
            totalMessages?: number
            readRevision?: number
            overlay?: WindowedConversationOverlay
            pageBytes?: number
            commit?: Parameters<typeof makeStore>[0]
            store?: Parameters<typeof makeStore>[1]
        } = {}) {
            const database = makeDatabase()
            const { character: projection, projectedConversation } = makeWindowedProjection()
            let selected: character = projection
            const sessionToken = 'windowed-session' as any
            let authority: TestWindowedAuthority | null = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                storeRevision: options.overlay?.revision ?? 2,
                overlay: options.overlay,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: options.totalMessages ?? 10_000,
            }
            const commit = options.commit ?? vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const readConversationWindow = vi.fn(async () => ({
                revision: options.readRevision ?? 2,
                value: {
                    characterId: 'char-a',
                    conversationId: 'two',
                    messages: [{ role: 'user' as const, data: 'first persisted message' }],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: options.totalMessages ?? 10_000,
                    hasMoreBefore: false,
                    hasMoreAfter: true,
                },
            }))
            const store = makeStore(commit, { readConversationWindow, ...options.store })
            const onPersisted = vi.fn((event) => {
                if (authority?.sessionToken !== event.sessionToken) return
                authority = {
                    ...authority,
                    storeRevision: event.revision,
                    persistedSessionVersion: event.sessionVersion,
                }
            })
            // What each local commit told the working set: its revision and, when given, the stored count
            // and the rows it rewrote.
            const localRevisions: [number, number | undefined, readonly ConversationRowsRewrite[] | undefined][] = []
            // The stored count each flush handed to the working set with its revision.
            const windowedRevisions: [number, number | undefined][] = []
            const coordinator = new SaveCoordinator({
                store,
                conversationInsertPageBytes: options.pageBytes,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                captureCharacter: (id) => selected?.chaId === id
                    ? selected
                    : database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
                onConversationMutationPersisted: onPersisted,
                onWindowedSelectedConversationRevision: (revision, _totalMessages, _preserveRows, storedTotalMessages) => {
                    windowedRevisions.push([revision, storedTotalMessages])
                    if (authority) authority = { ...authority, storeRevision: revision,
                        ...(options.overlay ? { totalMessages: options.overlay.count, persistedSessionVersion: options.overlay.persistedVersion } : {}) }
                },
                onLocalRevision: (revision, storedTotalMessages, rewritten) =>
                    localRevisions.push([revision, storedTotalMessages, rewritten]),
            })
            coordinator.initialize(authority.storeRevision, database)
            expect(coordinator.adoptWindowedSelectedConversation(
                authority.storeRevision,
                coordinator.mutationGeneration,
                projection,
                authority,
            )).toBe(true)
            return {
                authority: () => authority,
                commit,
                coordinator,
                database,
                localRevisions,
                windowedRevisions,
                onPersisted,
                projectedConversation,
                readConversationWindow,
                selected: () => selected,
                replaceAuthority: (next: TestWindowedAuthority | null) => {
                    authority = next
                },
                replaceSelected: (next: character) => {
                    selected = next
                },
                sessionToken,
            }
        }

        it('persists owned send recency without any older message reads or character replacement', async () => {
            const harness = makeWindowedHarness()
            const authority = harness.authority()! as WindowedConversationPersistenceAuthority
            expect(harness.coordinator.recordSelectedCharacterLastInteraction(authority, undefined, 42)).toBe(true)
            expect(harness.selected().lastInteraction).toBe(42)
            expect(harness.coordinator.recordSelectedCharacterLastInteraction(authority, undefined, 43)).toBe(false)
            await harness.coordinator.flushPendingData('send-recency')
            expect(harness.commit).toHaveBeenCalledExactlyOnceWith({ expectedRevision: 2,
                unitMutations: [{ key: '["character","char-a","lastInteraction"]', type: 'set', value: 42 }] })
            expect(harness.readConversationWindow).not.toHaveBeenCalled()
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
            expect(harness.coordinator.recordSelectedCharacterLastInteraction(authority, 42, 43)).toBe(false)
            expect(harness.coordinator.recordSelectedCharacterLastInteraction(harness.authority()!, 42, 43)).toBe(true)
            await harness.coordinator.flushPendingData('next-send-recency')
            expect(harness.commit.mock.calls[1][0].unitMutations[0].value).toBe(43)
        })

        it('rebases a second send recency intent recorded while the first local commit is pending', async () => {
            const first = deferred<{ revision: number }>()
            const started = deferred<void>()
            const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => {
                if (commit.mock.calls.length === 1) { started.resolve(); return first.promise }
                return { revision: expectedRevision + 1 }
            })
            const harness = makeWindowedHarness({ commit })
            expect(harness.coordinator.recordSelectedCharacterLastInteraction(harness.authority()!, undefined, 42)).toBe(true)
            const flushing = harness.coordinator.flushPendingDataLocally('overlapping-send-recency')
            await started.promise
            expect(harness.coordinator.recordSelectedCharacterLastInteraction(harness.authority()!, 42, 43)).toBe(true)
            first.resolve({ revision: 3 })
            await flushing
            await harness.coordinator.flushPendingData('finish-overlapping-send-recency')
            expect(commit.mock.calls.map(([request]) => {
                const mutation = request.unitMutations![0]
                return 'value' in mutation ? mutation.value : undefined
            })).toEqual([42, 43])
            expect(harness.readConversationWindow).not.toHaveBeenCalled()
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
        })

        it('retires flushed windowed ownership before deselection so root saves remain available', async () => {
            const harness = makeWindowedHarness()
            await harness.coordinator.flushPendingDataLocally('leave-windowed')
            harness.coordinator.retireWindowedSelectedConversation()
            harness.replaceAuthority(null)
            harness.replaceSelected(null as any)
            harness.database.username = 'Changed after Home'
            harness.coordinator.markPersistentDataDirty(0)
            await harness.coordinator.flushPendingDataLocally('save-after-Home')
            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0]).toMatchObject({ rootMutations: [
                { type: 'set', key: 'username', value: 'Changed after Home' },
            ] })
        })

        it('does not retire windowed ownership while a zero-byte edit remains pending', async () => {
            const harness = makeWindowedHarness()
            harness.database.username = 'Pending root edit'
            harness.coordinator.markPersistentDataDirty(0)
            expect(() => harness.coordinator.retireWindowedSelectedConversation()).toThrow('pending persistence')
            await harness.coordinator.flushPendingDataLocally('complete-before-leaving')
            expect(() => harness.coordinator.retireWindowedSelectedConversation()).not.toThrow()
        })

        it('retires windowed ownership while only a storage-only mutation is queued behind the flush', async () => {
            const harness = makeWindowedHarness()
            harness.database.username = 'Saved before leaving'
            harness.coordinator.markPersistentDataDirty(0)
            const cleanup = deferred<void>()
            void harness.coordinator.flushPendingData('deleted-owner-assets')
            const storageOnly = harness.coordinator.runStorageOnlyMutation(async (expectedRevision) => {
                await cleanup.promise
                return expectedRevision + 1
            })
            expect(() => harness.coordinator.retireWindowedSelectedConversation()).toThrow('pending persistence')
            await harness.coordinator.flushPendingData('deactivate-working-set')
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(true)
            expect(() => harness.coordinator.retireWindowedSelectedConversation()).not.toThrow()
            cleanup.resolve()
            await storageOnly
            expect(harness.coordinator.revision).toBe(4)
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
        })

        it('commits a character addition beside the windowed selection and keeps later saves to their own changes', async () => {
            const harness = makeWindowedHarness()
            const added = {
                type: 'character',
                chaId: 'char-added',
                name: 'Added',
                chats: [{ id: 'added-chat', name: 'Chat 1', note: '', localLore: [], message: [] }],
            } as unknown as character
            harness.database.username = 'Changed with the addition'
            await harness.coordinator.commitCharacterAddition({
                characterId: added.chaId,
                estimatedBytes: 1,
                install: () => harness.database.characters.push(added),
            }, 'create-character')

            const saved = harness.commit.mock.calls.map(([input]) => input)
            expect(saved.find(input => input.addCharacter)?.addCharacter).toMatchObject({ chaId: 'char-added', name: 'Added' })
            expect(saved.flatMap(input => input.rootMutations ?? [])).toEqual([{ type: 'set', key: 'username', value: 'Changed with the addition' }])
            expect(saved.every(input => !input.character && !input.replaceCharacter)).toBe(true)
            expect(harness.readConversationWindow).not.toHaveBeenCalled()
            expect(harness.authority()?.storeRevision).toBe(4)
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)

            harness.database.username = 'Changed after the addition'
            harness.coordinator.markPersistentDataDirty(1)
            await harness.coordinator.flushPendingDataLocally('after-addition')
            expect(harness.commit.mock.calls.at(-1)![0]).toEqual({
                expectedRevision: 4,
                rootMutations: [{ type: 'set', key: 'username', value: 'Changed after the addition' }],
            })
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
            expect(() => harness.coordinator.retireWindowedSelectedConversation()).not.toThrow()
        })

        it('saves an edit made to the added character while its windowed addition commit is pending', async () => {
            const first = deferred<{ revision: number }>()
            const started = deferred<void>()
            const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => {
                if (commit.mock.calls.length === 1) { started.resolve(); return first.promise }
                return { revision: expectedRevision + 1 }
            })
            const harness = makeWindowedHarness({ commit })
            const added = {
                type: 'character',
                chaId: 'char-added',
                name: 'Added',
                chats: [{ id: 'added-chat', name: 'Chat 1', note: '', localLore: [], message: [] }],
            } as unknown as character
            const saving = harness.coordinator.commitCharacterAddition({
                characterId: added.chaId,
                estimatedBytes: 1,
                install: () => harness.database.characters.push(added),
            }, 'create-character')
            await Promise.race([started.promise, saving])
            added.name = 'Renamed during the addition'
            harness.coordinator.markPersistentDataDirty(1)
            first.resolve({ revision: 3 })
            await saving

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[0][0].addCharacter).toMatchObject({ chaId: 'char-added', name: 'Added' })
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 3,
                replaceCharacter: { chaId: 'char-added', name: 'Renamed during the addition' },
            })
            expect(commit.mock.calls[1][0]).not.toHaveProperty('character')
            expect(harness.readConversationWindow).not.toHaveBeenCalled()
            expect(harness.authority()?.storeRevision).toBe(4)
            // The edit armed a debounce; draining it finds nothing left to commit.
            await harness.coordinator.flushPendingDataLocally('drain-debounce')
            expect(commit).toHaveBeenCalledTimes(2)
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
        })

        function recordWindowedMutation(
            coordinator: SaveCoordinator,
            options: {
                characterId?: string
                conversationId?: string
                sessionToken: any
                previousVersion: number
                sessionVersion: number
                start: number
                deleteCount: number
                messages: Array<{ role: string; data: string }>
                conversationName?: string
                note?: string
            },
        ) {
            coordinator.recordActiveConversationMutation({
                characterId: options.characterId ?? 'char-a',
                conversationId: options.conversationId ?? 'two',
                sessionToken: options.sessionToken,
                previousVersion: options.previousVersion,
                sessionVersion: options.sessionVersion,
                commands: ['replace-range'],
                mutations: [{
                    start: options.start,
                    deleteCount: options.deleteCount,
                    messages: options.messages as any,
                    sessionVersion: options.sessionVersion,
                }],
                conversation: {
                    id: options.conversationId ?? 'two',
                    name: options.conversationName ?? 'Two',
                    localLore: [],
                    note: options.note ?? '',
                },
            })
        }

        it('rejects windowed adoption while ordinary dirty work is pending', () => {
            const database = makeDatabase()
            const { character: projection } = makeWindowedProjection()
            const sessionToken = 'windowed-session' as any
            let selected = database.characters[0]
            let authority: TestWindowedAuthority | null = null
            const coordinator = new SaveCoordinator({
                store: makeStore(),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(2, database)
            database.username = 'unsaved root change'
            coordinator.markPersistentDataDirty(1)
            selected = projection
            authority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                storeRevision: 2,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }

            expect(coordinator.adoptWindowedSelectedConversation(
                2,
                coordinator.mutationGeneration,
                projection,
                authority,
            )).toBe(false)
        })

        it.each([false, true])('persists activation differences and retries without losing asset-owner updates (%s)', async (assetsChanged) => {
            const database = makeDatabase()
            const beforeCharacter = {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 0,
                lastInteraction: 10,
                desc: 'synthetic unchanged description'.repeat(40_000),
                creatorNotes: 'remove during normalization',
            } as unknown as CharacterDetail
            const afterCharacter = {
                ...beforeCharacter,
                chatPage: 1,
                lastInteraction: 20,
                firstMessage: 'normalized greeting',
            } as CharacterDetail
            delete afterCharacter.creatorNotes
            if (assetsChanged) afterCharacter.additionalAssets = []
            const beforeConversation = {
                id: 'two',
                name: 'Two',
                localLore: [],
                note: 'before',
            }
            const afterConversation = {
                ...beforeConversation,
                note: 'after',
                fmIndex: -1,
            }
            const selected = {
                ...afterCharacter,
                chats: [
                    { id: 'one', name: 'One', localLore: [], note: '' },
                    afterConversation,
                ],
            } as unknown as character
            const sessionToken = 'activation-session' as any
            let authority: TestWindowedAuthority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                storeRevision: 2,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }
            const commit = vi
                .fn()
                .mockRejectedValueOnce(new Error('activation commit failed'))
                .mockImplementation(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                replaceDatabase: () => undefined,
                onWindowedSelectedConversationRevision: (revision) => {
                    authority = { ...authority, storeRevision: revision }
                },
            })
            coordinator.initialize(2, database)

            expect(
                coordinator.runSelectedConversationTransition(() =>
                    coordinator.adoptWindowedSelectedConversation(
                        2,
                        coordinator.mutationGeneration,
                        selected,
                        authority,
                        {
                            character: {
                                before: beforeCharacter,
                                after: afterCharacter,
                            },
                            conversation: {
                                before: beforeConversation,
                                after: afterConversation,
                            },
                        },
                    ),
                ),
            ).toBe(true)
            expect(coordinator.hasPendingPersistenceWork).toBe(true)
            expect(coordinator.recordSelectedCharacterLastInteraction(authority, 20, 25)).toBe(true)

            await expect(
                coordinator.flushPendingData('windowed-activation-failure'),
            ).rejects.toThrow('activation commit failed')
            expect(coordinator.hasPendingPersistenceWork).toBe(true)
            await coordinator.flushPendingData('windowed-activation-retry')

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toEqual({
                expectedRevision: 2,
                ...(assetsChanged ? { character: { ...afterCharacter, lastInteraction: 25 } } : { unitMutations: [
                    { key: '["character","char-a","chatPage"]', type: 'set', value: 1 },
                    { key: '["character","char-a","lastInteraction"]', type: 'set', value: 25 },
                    { key: '["character","char-a","creatorNotes"]', type: 'delete' },
                    { key: '["character","char-a","firstMessage"]', type: 'set', value: 'normalized greeting' },
                ] }),
                conversations: [
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'two',
                        start: 0,
                        deleteCount: 0,
                        messages: [],
                        conversation: afterConversation,
                    },
                ],
            })
            if (!assetsChanged) expect(JSON.stringify(commit.mock.calls[1][0]).length).toBeLessThan(1_024)
            expect(commit.mock.calls[1][0]).toEqual(commit.mock.calls[0][0])
            expect(commit.mock.calls[1][0]).not.toHaveProperty(
                'replaceCharacter',
            )

            selected.chatPage = 0
            coordinator.markPersistentDataDirty(1)
            await expect(
                coordinator.flushPendingData('unrecorded-page-change'),
            ).rejects.toThrow(/compatibility/i)
            expect(commit).toHaveBeenCalledTimes(2)
        })

        it('composes activation metadata before a later exact message event', async () => {
            const database = makeDatabase()
            const beforeCharacter = {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                lastInteraction: 10,
            } as unknown as CharacterDetail
            const afterCharacter = {
                ...beforeCharacter,
                lastInteraction: 20,
            } as CharacterDetail
            const beforeConversation = {
                id: 'two',
                name: 'Two',
                localLore: [],
                note: 'legacy note',
            }
            const activatedConversation = {
                ...beforeConversation,
                note: 'normalized note',
                fmIndex: -1,
            }
            const eventConversation = {
                ...activatedConversation,
                name: 'Edited after activation',
                note: 'later exact note',
            }
            const selected = {
                ...afterCharacter,
                chats: [activatedConversation],
            } as unknown as character
            const sessionToken = 'activation-with-message-session' as any
            let authority: TestWindowedAuthority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                storeRevision: 2,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit, {
                readConversationWindow: vi.fn(async () => ({
                    revision: 2,
                    value: {
                        characterId: 'char-a',
                        conversationId: 'two',
                        messages: [{ role: 'user' as const, data: 'persisted first' }],
                        startIndex: 0,
                        endIndex: 1,
                        totalMessages: 10_000,
                        hasMoreBefore: false,
                        hasMoreAfter: true,
                    },
                })),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                replaceDatabase: () => undefined,
                onConversationMutationPersisted: (event) => {
                    authority = {
                        ...authority,
                        storeRevision: event.revision,
                        persistedSessionVersion: event.sessionVersion,
                    }
                },
                onWindowedSelectedConversationRevision: (revision) => {
                    authority = { ...authority, storeRevision: revision }
                },
            })
            coordinator.initialize(2, database)

            expect(
                coordinator.runSelectedConversationTransition(() =>
                    coordinator.adoptWindowedSelectedConversation(
                        2,
                        coordinator.mutationGeneration,
                        selected,
                        authority,
                        {
                            character: {
                                before: beforeCharacter,
                                after: afterCharacter,
                            },
                            conversation: {
                                before: beforeConversation,
                                after: activatedConversation,
                            },
                        },
                    ),
                ),
            ).toBe(true)

            selected.chats[0] = eventConversation as Chat
            authority = {
                ...authority,
                sessionVersion: 1,
                totalMessages: 10_001,
            }
            coordinator.recordActiveConversationMutation({
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                commands: ['replace-range'],
                mutations: [
                    {
                        start: 10_000,
                        deleteCount: 0,
                        messages: [
                            { role: 'char', data: 'appended after activation' },
                        ],
                        sessionVersion: 1,
                    },
                ],
                conversation: eventConversation,
            })

            await coordinator.flushPendingData('activation-with-message-event')

            expect(commit).toHaveBeenCalledOnce()
            expect(commit.mock.calls[0][0]).toEqual({
                expectedRevision: 2,
                unitMutations: [{ key: '["character","char-a","lastInteraction"]', type: 'set', value: 20 }],
                conversations: [
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'two',
                        start: 0,
                        deleteCount: 0,
                        messages: [],
                        conversation: activatedConversation,
                    },
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'two',
                        start: 10_000,
                        deleteCount: 0,
                        messages: [
                            { role: 'char', data: 'appended after activation' },
                        ],
                        conversation: eventConversation,
                    },
                ],
            })
            expect(authority).toMatchObject({
                storeRevision: 3,
                persistedSessionVersion: 1,
                sessionVersion: 1,
                totalMessages: 10_001,
            })
            expect(coordinator.hasPendingPersistenceWork).toBe(false)
        })

        it('restores windowed baselines and activation evidence when adoption publication rolls back', () => {
            const database = makeDatabase()
            let selected = database.characters[0]
            let authority: TestWindowedAuthority | null = null
            const coordinator = new SaveCoordinator({
                store: makeStore(),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(2, database)
            const previous = selected
            const next = {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha normalized',
                chats: [{ id: 'two', name: 'Two', localLore: [], note: '' }],
            } as unknown as character
            const nextAuthority: TestWindowedAuthority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken: 'rolled-back-activation' as any,
                storeRevision: 2,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }

            expect(() =>
                coordinator.runSelectedConversationTransition(() => {
                    selected = next
                    authority = nextAuthority
                    expect(
                        coordinator.adoptWindowedSelectedConversation(
                            2,
                            coordinator.mutationGeneration,
                            next,
                            nextAuthority,
                            {
                                character: {
                                    before: {
                                        type: 'character',
                                        chaId: 'char-a',
                                        name: 'Alpha',
                                    } as unknown as CharacterDetail,
                                    after: {
                                        type: 'character',
                                        chaId: 'char-a',
                                        name: 'Alpha normalized',
                                    } as unknown as CharacterDetail,
                                },
                            },
                        ),
                    ).toBe(true)
                    throw new Error('publication failed')
                }),
            ).toThrow('publication failed')

            selected = previous
            authority = null
            expect(coordinator.hasPendingPersistenceWork).toBe(false)
            expect(
                coordinator.adoptHydratedCharacter(
                    2,
                    coordinator.mutationGeneration,
                    previous,
                ),
            ).toBe(true)
        })

        it('keeps the previous windowed baseline usable when a transfer rolls back', async () => {
            const harness = makeWindowedHarness()
            const previousSelected = harness.selected()
            const previousAuthority = harness.authority()!
            const { character: tentative } = makeWindowedProjection()
            const tentativeAuthority = {
                ...previousAuthority,
                sessionToken: 'tentative-transfer' as any,
            }

            expect(() =>
                harness.coordinator.runSelectedConversationTransition(() => {
                    harness.replaceSelected(tentative)
                    harness.replaceAuthority(tentativeAuthority)
                    expect(
                        harness.coordinator.adoptWindowedSelectedConversation(
                            2,
                            harness.coordinator.mutationGeneration,
                            tentative,
                            tentativeAuthority,
                        ),
                    ).toBe(true)
                    throw new Error('tentative publication failed')
                }),
            ).toThrow('tentative publication failed')

            harness.replaceSelected(previousSelected)
            harness.replaceAuthority(previousAuthority)
            await expect(
                harness.coordinator.flushPendingData(
                    'rolled-back-windowed-transfer',
                ),
            ).resolves.toBeUndefined()
            expect(harness.commit).not.toHaveBeenCalled()
        })

        it('commits absolute ranges from a 10k windowed projection without traversing projected messages', async () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'bounded edit' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })

            await harness.coordinator.flushPendingData('windowed-absolute-range')

            expect(harness.readConversationWindow).toHaveBeenCalledWith({
                characterId: 'char-a',
                conversationId: 'two',
                startIndex: 0,
                limit: 1,
            })
            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0]).toMatchObject({
                expectedRevision: 2,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'two',
                    start: 9_500,
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'bounded edit' }],
                }],
            })
            expect(harness.commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
            expect(harness.onPersisted).toHaveBeenCalledWith(expect.objectContaining({
                sessionVersion: 1,
                revision: 3,
            }))
        })

        it.each([
            {
                label: 'out-of-range replacement',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionVersion: 1,
                }),
                event: { start: 10_001, deleteCount: 0, sessionVersion: 1 },
            },
            {
                label: 'final count mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionVersion: 1,
                    totalMessages: 10_001,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
            {
                label: 'session version mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionVersion: 2,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
            {
                label: 'session token mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    sessionToken: 'other-session' as any,
                    sessionVersion: 1,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
            {
                label: 'authority revision mismatch',
                mutateAuthority: (authority: TestWindowedAuthority) => ({
                    ...authority,
                    storeRevision: 1,
                    sessionVersion: 1,
                }),
                event: { start: 9_500, deleteCount: 1, sessionVersion: 1 },
            },
        ])('fails closed without replacement for $label', async ({ mutateAuthority, event }) => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: event.sessionVersion,
                start: event.start,
                deleteCount: event.deleteCount,
                messages: [{ role: 'char', data: 'invalid projection' }],
            })
            harness.replaceAuthority(mutateAuthority(harness.authority()!))

            await expect(
                harness.coordinator.flushPendingData(`windowed-${event.start}`),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit).not.toHaveBeenCalled()
            expect(harness.onPersisted).not.toHaveBeenCalled()
        })

        it('fails closed when the persistent range read returns another revision', async () => {
            const harness = makeWindowedHarness({ readRevision: 1 })
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'stale read' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-stale-read'),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit).not.toHaveBeenCalled()
            expect(harness.onPersisted).not.toHaveBeenCalled()
        })

        it('rejects complete-owner evidence instead of replacing all persisted messages', async () => {
            const harness = makeWindowedHarness()
            harness.coordinator.recordActiveConversationMutation({
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                commands: ['replace-conversation'],
                mutations: [{
                    start: 0,
                    deleteCount: 64,
                    messages: [{ role: 'char', data: 'partial projected owner' }],
                    sessionVersion: 1,
                    completeOwner: true,
                }],
                conversation: {
                    id: 'two',
                    name: 'Two',
                    localLore: [],
                    note: '',
                },
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
                totalMessages: 1,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-complete-owner'),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit).not.toHaveBeenCalled()
            expect(harness.onPersisted).not.toHaveBeenCalled()
        })

        it('rechecks windowed authority after an awaited complete-character commit', async () => {
            const database = makeDatabase()
            const { character: projection } = makeWindowedProjection()
            let selected: character = database.characters[0]
            let authority: TestWindowedAuthority | null = null
            const committed = deferred<{ revision: number }>()
            const commit = vi.fn(() => committed.promise)
            const store = makeStore(commit, {
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' } as CharacterDetail,
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(10, database)
            const replacing = coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'authority-switch-during-commit',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )
            await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

            selected = projection
            authority = {
                kind: 'windowed',
                characterId: 'char-a',
                conversationId: 'two',
                sessionToken: 'windowed-during-commit' as any,
                storeRevision: 10,
                persistedSessionVersion: 0,
                sessionVersion: 0,
                totalMessages: 10_000,
            }
            committed.resolve({ revision: 11 })

            await expect(replacing).rejects.toThrow(/compatibility/i)
            expect(commit).toHaveBeenCalledOnce()
        })

        it('acknowledges a contiguous multi-event prefix through one absolute-range commit', async () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 100,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'inserted' }],
            })
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 1,
                sessionVersion: 2,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'edited' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 2,
                totalMessages: 10_001,
            })

            await harness.coordinator.flushPendingData('windowed-multi-event')

            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0].conversations).toHaveLength(2)
            expect(harness.onPersisted.mock.calls.map(([event]) => event.sessionVersion)).toEqual([
                1,
                2,
            ])
        })

        it('folds a reply streamed while the store is blocked into one catch-up range under the commit limit', async () => {
            const blocked = deferred<void>()
            const entered = deferred<void>()
            let persistedTotal = 10_000
            const commit = vi.fn(async (input: WorkingSetCommit) => {
                const bytes = new TextEncoder().encode(JSON.stringify(input)).byteLength
                if (commit.mock.calls.length === 1) {
                    entered.resolve()
                    await blocked.promise
                }
                if (bytes > 32 * 1024) throw new PayloadTooLargeError('commit', bytes)
                for (const mutation of input.conversations ?? []) {
                    if (mutation.type === 'replace-range') persistedTotal += mutation.messages.length - mutation.deleteCount
                }
                return { revision: input.expectedRevision + 1 }
            })
            const harness = makeWindowedHarness({ commit })
            harness.readConversationWindow.mockImplementation(async () => ({
                revision: harness.coordinator.revision,
                value: {
                    characterId: 'char-a',
                    conversationId: 'two',
                    messages: [{ role: 'user' as const, data: 'first persisted message' }],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: persistedTotal,
                    hasMoreBefore: false,
                    hasMoreAfter: true,
                },
            }))
            const note = 'n'.repeat(2048)
            harness.projectedConversation.note = note
            let version = 0
            const record = (deleteCount: number, data: string) => {
                recordWindowedMutation(harness.coordinator, {
                    sessionToken: harness.sessionToken,
                    previousVersion: version,
                    sessionVersion: version + 1,
                    start: 10_000,
                    deleteCount,
                    messages: [{ role: 'char', data }],
                    note,
                })
                version += 1
                harness.replaceAuthority({ ...harness.authority()!, sessionVersion: version, totalMessages: 10_001 })
            }

            record(0, '')
            const flushing = harness.coordinator.flushPendingData('blocked-stream')
            await entered.promise
            let reply = ''
            for (let update = 1; update <= 40; update++) {
                reply += 'x'.repeat(100)
                record(1, reply)
            }
            blocked.resolve()
            await flushing

            expect(commit.mock.calls.map(([input]) => input.conversations)).toEqual([
                [expect.objectContaining({ start: 10_000, deleteCount: 0, messages: [{ role: 'char', data: '' }] })],
                [expect.objectContaining({
                    start: 10_000,
                    deleteCount: 1,
                    messages: [{ role: 'char', data: reply }],
                    conversation: expect.objectContaining({ note }),
                })],
            ])
            expect(harness.onPersisted.mock.calls.map(([event]) => [event.sessionVersion, event.revision])).toEqual([
                [1, 3],
                ...Array.from({ length: 40 }, (_, index) => [index + 2, 4]),
            ])
        })

        it('retains exact windowed evidence after failure and retries it unchanged', async () => {
            const commit = vi.fn()
                .mockRejectedValueOnce(new Error('windowed write failed'))
                .mockImplementation(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                }))
            const harness = makeWindowedHarness({ commit })
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'retry me' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-failure'),
            ).rejects.toThrow('windowed write failed')
            await harness.coordinator.flushPendingData('windowed-retry')

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0].conversations).toEqual(
                commit.mock.calls[0][0].conversations,
            )
            expect(harness.onPersisted).toHaveBeenCalledOnce()
        })

        it('retains windowed evidence after validation fails before the store commit', async () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 9_500,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'retry validated evidence' }],
            })
            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 2,
            })

            await expect(
                harness.coordinator.flushPendingData('windowed-invalid-version'),
            ).rejects.toThrow(/compatibility/i)
            expect(harness.commit).not.toHaveBeenCalled()

            harness.replaceAuthority({
                ...harness.authority()!,
                sessionVersion: 1,
            })
            await harness.coordinator.flushPendingData('windowed-valid-retry')

            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.onPersisted).toHaveBeenCalledOnce()
        })

        it.each([
            { label: 'shell mismatch', kind: 'shell' },
            { label: 'foreign conversation evidence', kind: 'foreign' },
        ])('never creates a character replacement for $label', async ({ kind }) => {
            const harness = makeWindowedHarness()
            if (kind === 'shell') {
                harness.projectedConversation.name = 'Unexplained metadata change'
                harness.coordinator.markPersistentDataDirty(1)
            } else {
                recordWindowedMutation(harness.coordinator, {
                    characterId: 'char-a',
                    conversationId: 'foreign',
                    sessionToken: harness.sessionToken,
                    previousVersion: 0,
                    sessionVersion: 1,
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'char', data: 'foreign' }],
                })
                harness.replaceAuthority({
                    ...harness.authority()!,
                    sessionVersion: 1,
                    totalMessages: 10_001,
                })
            }

            await expect(
                harness.coordinator.flushPendingData(`windowed-never-replace-${kind}`),
            ).rejects.toThrow(/compatibility/i)

            expect(harness.commit.mock.calls.some(
                ([workingSet]) => workingSet.replaceCharacter !== undefined,
            )).toBe(false)
        })

        it('requires explicit complete adoption before restoring the complete fallback', async () => {
            const harness = makeWindowedHarness()
            const complete = {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chats: [{
                    id: 'two',
                    name: 'Two',
                    localLore: [],
                    note: '',
                    message: [{ role: 'user', data: 'complete body' }],
                }],
            } as unknown as character
            harness.replaceAuthority(null)
            harness.replaceSelected(complete)
            harness.coordinator.markPersistentDataDirty(1)

            await expect(
                harness.coordinator.flushPendingData('complete-without-adoption'),
            ).rejects.toThrow(/compatibility/i)
            expect(harness.commit).not.toHaveBeenCalled()

            expect(harness.coordinator.adoptHydratedCharacter(
                2,
                harness.coordinator.mutationGeneration,
                complete,
            )).toBe(true)
            complete.name = 'Complete fallback restored'
            harness.coordinator.markPersistentDataDirty(1)
            await harness.coordinator.flushPendingData('complete-after-adoption')

            expect(harness.commit).toHaveBeenCalledOnce()
            expect(harness.commit.mock.calls[0][0].replaceCharacter).toMatchObject({
                chaId: 'char-a',
                name: 'Complete fallback restored',
            })
        })

        it('blocks persistence reentry during a selected-conversation authority transition', async () => {
            const harness = makeWindowedHarness()

            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
            expect(harness.coordinator.isSelectedConversationTransitionActive).toBe(false)
            expect(() => harness.coordinator.runSelectedConversationTransition(() => {
                expect(harness.coordinator.isSelectedConversationTransitionActive).toBe(true)
                harness.coordinator.markPersistentDataDirty(1)
            })).toThrow(/transition/i)
            expect(harness.coordinator.isSelectedConversationTransitionActive).toBe(false)
            expect(() => harness.coordinator.runSelectedConversationTransition(() =>
                harness.coordinator.flushPendingData('reentrant-transition'),
            )).toThrow(/transition/i)
            expect(harness.commit).not.toHaveBeenCalled()
        })

        it('advances an adopted windowed authority after a storage-only revision', async () => {
            const harness = makeWindowedHarness()

            await harness.coordinator.runStorageOnlyMutation(async () => 3)
            const advanced = {
                ...harness.authority()!,
                storeRevision: 3,
            }
            harness.replaceAuthority(advanced)

            expect(harness.coordinator.advanceWindowedSelectedConversationRevision(
                3,
                advanced,
            )).toBe(true)
            await harness.coordinator.flushPendingData('advanced-windowed-baseline')
            expect(harness.commit).not.toHaveBeenCalled()
        })

        // A commit that stores nothing in the open conversation's messages, or only replaces rows one for
        // one, lets the working set keep the rows that hold edits made meanwhile; one that writes them
        // otherwise leaves the rows to be read again.
        const rewrittenMessage = { role: 'user' as const, data: 'rewritten' }
        it.each<{
            commit: string
            units: string[][]
            // Each range replaces from row 3 with one message: character, conversation, deleted rows, settings.
            ranges: [string, string, number, Partial<Chat>?][]
            reported: [number | undefined, ConversationRowsRewrite[] | undefined]
        }>([
            { commit: 'a root setting', units: [['root', 'loreBookDepth']], ranges: [], reported: [40, []] },
            { commit: 'a range in another conversation', units: [], ranges: [['char-a', 'one', 0]], reported: [40, []] },
            { commit: 'a row rewrite in the open conversation', units: [], ranges: [['char-a', 'two', 1]],
                reported: [40, [{ start: 3, messages: [rewrittenMessage] }]] },
            { commit: 'an insertion in the open conversation', units: [], ranges: [['char-a', 'two', 0]], reported: [undefined, undefined] },
            { commit: 'a row rewrite with the open conversation\'s settings', units: [], ranges: [['char-a', 'two', 1, { name: 'renamed' }]],
                reported: [undefined, undefined] },
            { commit: 'messages of the open conversation', units: [['messages', 'char-a', 'two', 'm1']], ranges: [], reported: [undefined, undefined] },
            { commit: 'the removal of the open conversation', units: [['exists', 'conversation', 'char-a', 'two']], ranges: [], reported: [undefined, undefined] },
            { commit: 'the removal of its character', units: [['exists', 'character', 'char-a']], ranges: [], reported: [undefined, undefined] },
        ])('tells the working set what $commit stored in the open windowed conversation\'s rows', async ({ units, ranges, reported }) => {
            const harness = makeWindowedHarness({ totalMessages: 40 })
            await harness.coordinator.commitPersistentUnitIntent('unit', units.map((key) => ({ key: JSON.stringify(key), type: 'set' as const, value: 1 })),
                ranges.map(([characterId, conversationId, deleteCount, conversation]) => ({
                    type: 'replace-range' as const, characterId, conversationId, start: 3, deleteCount, messages: [rewrittenMessage],
                    ...conversation ? { conversation: conversation as unknown as Omit<Chat, 'message'> } : {},
                })))
            expect(harness.localRevisions).toEqual([[3, ...reported]])
        })

        // A retry that moves past another writer's commit cannot tell whether it moved the open
        // conversation's rows, so the working set is not told the rows are current.
        it.each([
            { commit: 'a save', retried: false, reported: [[3, 40]] },
            { commit: 'a save', retried: true, reported: [[4, undefined]] },
            { commit: 'a chat setting change', retried: false, reported: [[3, 40]] },
            { commit: 'a chat setting change', retried: true, reported: [[4, undefined]] },
        ])('tells the working set whether the open windowed conversation\'s rows are current after $commit (retried past another writer: $retried)', async ({ commit, retried, reported }) => {
            let attempts = 0
            const harness = makeWindowedHarness({
                totalMessages: 40,
                commit: vi.fn(async ({ expectedRevision }) => {
                    if (retried && !attempts++) throw new RevisionConflictError(expectedRevision, expectedRevision + 1)
                    return { revision: expectedRevision + 1 }
                }),
                store: {
                    readConversationMetadata: vi.fn(async () => ({ revision: harness.coordinator.revision, value: {
                        characterId: 'char-a', conversationId: 'two', totalMessages: 40,
                        conversation: { id: 'two', name: 'Two', note: '' } as Omit<Chat, 'message'>,
                    } })),
                },
            })
            if (commit === 'a save') {
                harness.database.username = 'Saved while another writer committed'
                harness.coordinator.markPersistentDataDirty(0)
                await harness.coordinator.flushPendingData('save')
                expect(harness.windowedRevisions).toEqual(reported)
            } else {
                await harness.coordinator.mutateConversationBinding('char-a', 'two', { note: 'changed' }, () => {}, { note: '' })
                expect(harness.localRevisions.map(([revision, storedTotalMessages]) => [revision, storedTotalMessages])).toEqual(reported)
            }
        })

        function recordAddedChat(
            harness: ReturnType<typeof makeWindowedHarness>,
            messages: unknown[] = [{ role: 'user', data: 'added message' }],
        ) {
            const before = harness.selected()
            const added = {
                id: 'added',
                name: 'Added',
                note: 'added note',
                localLore: [],
                message: messages,
            } as unknown as Chat
            const draft = {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 1,
                chats: [added, { id: 'two', name: 'Two', localLore: [], note: '' }],
            } as unknown as character
            const recorded = harness.coordinator.recordWindowedChatListChange(
                harness.authority()! as WindowedConversationPersistenceAuthority,
                before,
                draft,
            )
            // Mirrors the working set keeping the selected shell object in place.
            harness.replaceSelected({
                ...draft,
                chats: [added, harness.projectedConversation],
            } as unknown as character)
            return { recorded, added }
        }

        it('commits a recorded chat-list edit without reading the selected conversation', async () => {
            const harness = makeWindowedHarness()
            const { recorded } = recordAddedChat(harness)
            expect(recorded).toBe(true)
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(true)

            await harness.coordinator.flushPendingData('chat-list')

            expect(harness.readConversationWindow).not.toHaveBeenCalled()
            expect(harness.commit).toHaveBeenCalledTimes(1)
            const request = harness.commit.mock.calls[0][0]
            expect(request).not.toHaveProperty('replaceCharacter')
            expect(request.character).toEqual({
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 1,
            })
            expect(request.conversations).toEqual([
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'added',
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'user', data: 'added message' }],
                    conversation: { id: 'added', name: 'Added', note: 'added note', localLore: [] },
                    configuredIndex: 1,
                },
                { type: 'reorder', characterId: 'char-a', conversationIds: ['added', 'two'] },
            ])
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
        })

        it('captures inserted messages without cloning the complete body array', async () => {
            const harness = makeWindowedHarness()
            const messages = [{ role: 'user', data: 'counted body' }]
            const clone = vi.mocked(safeStructuredClone)
            clone.mockClear()
            expect(recordAddedChat(harness, messages).recorded).toBe(true)
            await harness.coordinator.flushPendingData('chat-list-copies')
            expect(harness.commit).toHaveBeenCalledTimes(1)
            expect(harness.commit.mock.calls[0][0].conversations[0].messages).toEqual(messages)
            const bodyCopies = clone.mock.calls.filter(([value]) => JSON.stringify(value) === JSON.stringify(messages))
            expect(bodyCopies).toHaveLength(0)
        })

        function pagedSave(fail?: number) {
            const commit = vi.fn(async ({ expectedRevision }: WorkingSetCommit) => {
                const call = commit.mock.calls.length
                if (call === fail) throw new Error('synthetic storage failure')
                return { revision: expectedRevision + 1 }
            })
            const harness = makeWindowedHarness({ commit })
            // Three 7 MiB messages exceed one 16 MiB page.
            const messages = [0, 1, 2].map((index) => ({ role: 'user', data: `${index}`.padEnd(7 * 1024 * 1024, 'x') }))
            expect(recordAddedChat(harness, messages).recorded).toBe(true)
            const sent = () => commit.mock.calls.map(([input]) => input)
            const firsts = (input: WorkingSetCommit) => input.conversations?.map((mutation) =>
                mutation.type === 'replace-range' ? { ...mutation, messages: mutation.messages.map((message) => message.data[0]) } : mutation)
            return { harness, sent, firsts }
        }

        it('plans created chat pages before any whole-save rejection', async () => {
            const { harness, sent, firsts } = pagedSave()

            await harness.coordinator.flushPendingData('chat-list')

            const [created, appended] = sent()
            expect(sent()).toHaveLength(2)
            expect(created.expectedRevision).toBe(2)
            expect(created.character).toBeUndefined()
            expect(firsts(created)).toEqual([{
                type: 'replace-range', characterId: 'char-a', conversationId: 'added', start: 0, deleteCount: 0,
                messages: ['0', '1'],
                conversation: { id: 'added', name: 'Added', note: 'added note', localLore: [] },
                configuredIndex: 1,
            }])
            expect(appended.expectedRevision).toBe(3)
            expect(appended.character).toEqual({ type: 'character', chaId: 'char-a', name: 'Alpha', chatPage: 1 })
            expect(firsts(appended)).toEqual([
                { type: 'replace-range', characterId: 'char-a', conversationId: 'added', start: 2, deleteCount: 0, messages: ['2'] },
                { type: 'reorder', characterId: 'char-a', conversationIds: ['added', 'two'] },
            ])
            expect(harness.coordinator.revision).toBe(4)
            expect(harness.coordinator.hasPendingPersistenceWork).toBe(false)
        })

        it('removes a paged chat and reloads storage when a later page fails', async () => {
            const { harness, sent } = pagedSave(2)

            await expect(harness.coordinator.flushPendingData('chat-list')).rejects.toThrow('synthetic storage failure')

            expect(sent()).toHaveLength(3)
            expect(sent()[2]).toEqual({
                expectedRevision: 3,
                conversations: [{ type: 'delete', characterId: 'char-a', conversationId: 'added' }],
            })
            expect(harness.coordinator.pendingWorkingSetRefreshRevision).toBe(4)
        })

        async function makeDurableWindowedHarness(messages: Message[], fail?: (request: WorkingSetCommit) => boolean) {
            const database = makeDatabase()
            database.characters[0].chats = [{ id: 'two', name: 'Two', localLore: [], note: '', message: messages } as Chat]
            const indexedDB = new IDBFactory()
            const name = `windowed-save-${crypto.randomUUID()}`
            const store = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
            await store.open()
            const { revision } = await store.replaceFromDatabase(database)
            const overlay = new WindowedConversationOverlay('char-a', 'two', revision, messages.length)
            const commit = vi.fn(async (request: WorkingSetCommit) => {
                if (fail?.(request)) throw new Error('Page write failed')
                return store.commit(request)
            })
            const harness = makeWindowedHarness({ commit, totalMessages: messages.length, overlay, pageBytes: 64,
                store: {
                    acquireRevision: (revision) => store.acquireRevision(revision),
                    readConversationMetadata: (owner, id) => store.readConversationMetadata(owner, id),
                    readRoot: () => store.readRoot(),
                    queryPresets: () => store.queryPresets(),
                    readPreset: (id) => store.readPreset(id),
                } })
            const other = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
            await other.open()
            return { harness, overlay, store, other, revision }
        }

        it('relocates the dirty reply when a storage-only read observes another writer one revision ahead', async () => {
            const first: Message = { role: 'user', data: 'neighbour', chatId: 'first' }
            const reply: Message = { role: 'char', data: 'partial', chatId: 'reply' }
            const { harness, overlay, store, other, revision } = await makeDurableWindowedHarness([first, reply])
            const handle = overlay.bind(1, reply)
            const lease = await store.acquireRevision(revision)
            try {
                await overlay.prove(handle, lease)
            } finally {
                await lease.release()
            }
            const completed = { ...reply, data: 'completed reply' }
            overlay.applyRange(1, [reply], [completed], 1)
            harness.replaceAuthority({ ...harness.authority()!, sessionVersion: 1 })
            recordWindowedMutation(harness.coordinator, { sessionToken: harness.sessionToken, previousVersion: 0,
                sessionVersion: 1, start: 1, deleteCount: 1, messages: [completed] })
            await other.commit({ expectedRevision: revision, conversations: [{ type: 'replace-range', characterId: 'char-a',
                conversationId: 'two', start: 0, deleteCount: 2, messages: [reply, first] }] })
            await harness.coordinator.runStorageOnlyMutation(async () => (await store.readRoot()).revision)
            await harness.coordinator.flushPendingDataLocally('observed-remote-head')
            expect((await other.readConversation('char-a', 'two'))?.value.message).toEqual([completed, first])
            overlay.release(handle)
        })

        it.each(['protected binding', 'empty prepared intent', 'unchanged presets'] as const)(
            'adopts a newer head discovered by %s without writing another revision', async (route) => {
                const neighbour: Message = { role: 'user', data: 'neighbour', chatId: 'user' }
                const reply: Message = { role: 'char', data: 'reply', chatId: 'reply' }
                const { harness, overlay, store, other, revision } = await makeDurableWindowedHarness([neighbour, reply])
                const handle = overlay.bind(1, reply)
                const lease = await store.acquireRevision(revision)
                try { await overlay.prove(handle, lease) } finally { await lease.release() }
                const moved = await other.commit({ expectedRevision: revision, conversations: [{ type: 'replace-range',
                    characterId: 'char-a', conversationId: 'two', start: 0, deleteCount: 2, messages: [reply, neighbour] }] })
                if (route === 'protected binding') {
                    await harness.coordinator.mutateConversationBinding('char-a', 'two', { note: 'unaccepted' },
                        () => { throw new Error('Binding lost its precondition') }, { note: 'outdated' })
                } else if (route === 'empty prepared intent') {
                    expect(await harness.coordinator.commitPreparedUnitIntent('empty', async () => null)).toBeNull()
                } else await harness.coordinator.mutatePersistentPresets('unchanged', () => {})
                expect((await store.readRoot()).revision).toBe(moved.revision)
                expect(harness.coordinator.revision).toBe(moved.revision)
                expect(overlay.locate(handle)).toBe(0)
                overlay.release(handle)
            },
        )

        it.each(['retry', 'reset'] as const)('transfers a durable patch publication to the queue across a read error and %s', async (recovery) => {
            const { harness, store, other } = await makeDurableWindowedHarness([])
            const observations: string[] = []
            const commit = store.commit.bind(store)
            vi.spyOn(store, 'commit').mockImplementationOnce(async (input) => {
                const result = await commit(input)
                vi.spyOn(store, 'acquireRevision').mockRejectedValueOnce(new Error('Metadata temporarily unavailable'))
                return result
            })
            await expect(harness.coordinator.commitPreparedUnitIntent('patch', async () => ({
                unitMutations: [{ type: 'set', key: JSON.stringify(['conversation', 'char-a', 'two', 'note']), value: 'durable note' }],
                conversations: [],
            }), async () => { observations.push('published') }, true, {
                committed: () => { observations.push('durable') },
                retire: () => { observations.push('retired') },
            })).rejects.toThrow('Metadata temporarily unavailable')
            const persisted = await other.readConversationMetadata('char-a', 'two')
            expect(persisted?.value.conversation.note).toBe('durable note')
            expect(observations).toEqual(['durable'])
            if (recovery === 'retry') {
                await harness.coordinator.flushPendingDataLocally('retry-patch-publication')
                await harness.coordinator.flushPendingDataLocally('unchanged')
                expect(observations).toEqual(['durable', 'published'])
            } else {
                harness.coordinator.initialize(persisted!.revision, harness.database)
                harness.coordinator.initialize(persisted!.revision, harness.database)
                expect(observations).toEqual(['durable', 'retired'])
            }
            expect((await other.readRoot()).revision).toBe(persisted!.revision)
        })

        it('keeps the acknowledged reply when a later page of a newly created conversation fails', async () => {
            const { harness, overlay, other } = await makeDurableWindowedHarness([], (request) =>
                request.conversations?.some((mutation) => mutation.type === 'replace-range' &&
                    mutation.conversationId === 'added' && mutation.start > 0) ?? false)
            expect(recordAddedChat(harness, ['first page', 'second page'].map((data) => ({
                role: 'char', data: data.padEnd(64), chatId: data,
            }))).recorded).toBe(true)
            const reply = { role: 'char' as const, data: 'completed reply', chatId: 'reply' }
            overlay.applyRange(0, [], [reply], 1)
            harness.replaceAuthority({ ...harness.authority()!, sessionVersion: 1, totalMessages: 1 })
            recordWindowedMutation(harness.coordinator, { sessionToken: harness.sessionToken, previousVersion: 0,
                sessionVersion: 1, start: 0, deleteCount: 0, messages: [reply] })
            await expect(harness.coordinator.flushPendingDataLocally('split-reply')).rejects.toThrow('Page write failed')

            expect((await other.readConversation('char-a', 'two'))?.value.message).toEqual([reply])
            expect(await other.readConversation('char-a', 'added')).toBeNull()
            expect(harness.onPersisted.mock.calls.map(([event]) => event.sessionVersion)).toEqual([1])
            expect(overlay.dirty).toBe(false)
        })

        it('reports a save that is too large without creating anything', async () => {
            const commit = vi.fn(async (): Promise<{ revision: number }> => {
                throw new PayloadTooLargeError('commit', MAX_NATIVE_REQUEST_BYTES + 1)
            })
            const harness = makeWindowedHarness({ commit })
            harness.database.username = 'Changed with an oversized save'
            harness.coordinator.markPersistentDataDirty(0)

            await expect(harness.coordinator.flushPendingData('too-large')).rejects.toBeInstanceOf(PayloadTooLargeError)
            expect(commit).toHaveBeenCalledOnce()
            expect(harness.coordinator.pendingWorkingSetRefreshRevision).toBeNull()
        })

        it('still refuses a selected change the recorded chat-list edit does not explain', async () => {
            const harness = makeWindowedHarness()
            expect(recordAddedChat(harness).recorded).toBe(true)
            harness.replaceSelected({
                ...harness.selected(),
                name: 'Unrecorded rename',
            } as character)

            await expect(harness.coordinator.flushPendingData('chat-list')).rejects.toThrow(
                /compatibility/i,
            )
            expect(harness.commit).not.toHaveBeenCalled()
        })

        it('does not record a chat-list edit over unsaved conversation evidence', () => {
            const harness = makeWindowedHarness()
            recordWindowedMutation(harness.coordinator, {
                sessionToken: harness.sessionToken,
                previousVersion: 0,
                sessionVersion: 1,
                start: 10_000,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'unsaved' }],
            })

            expect(recordAddedChat(harness).recorded).toBe(false)
        })
    })
})
