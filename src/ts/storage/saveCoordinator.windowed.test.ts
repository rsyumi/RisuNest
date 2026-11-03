import { describe, expect, it, vi } from 'vitest'
import type { Chat, character } from './database.svelte'
import type {
    CharacterDetail,
    WorkingSetCommit,
} from './persistentDataStore'
import { MAX_NATIVE_REQUEST_BYTES, PayloadTooLargeError } from './nativePersistenceValue'
import type { WindowedConversationPersistenceAuthority } from './saveCoordinator'
import {
    captureRoot,
    deferred,
    makeDatabase,
    makeStore,
    SaveCoordinator,
} from './saveCoordinator.testSupport'
import { safeStructuredClone } from '../polyfill'

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
            commit?: Parameters<typeof makeStore>[0]
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
                storeRevision: 2,
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
            const store = makeStore(commit, { readConversationWindow })
            const onPersisted = vi.fn((event) => {
                if (authority?.sessionToken !== event.sessionToken) return
                authority = {
                    ...authority,
                    storeRevision: event.revision,
                    persistedSessionVersion: event.sessionVersion,
                }
            })
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => selected,
                captureSelectedConversationAuthority: () => authority,
                captureCharacter: (id) => selected?.chaId === id
                    ? selected
                    : database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
                onConversationMutationPersisted: onPersisted,
                onWindowedSelectedConversationRevision: (revision) => {
                    if (authority) authority = { ...authority, storeRevision: revision }
                },
            })
            coordinator.initialize(2, database)
            expect(coordinator.adoptWindowedSelectedConversation(
                2,
                coordinator.mutationGeneration,
                projection,
                authority,
            )).toBe(true)
            return {
                authority: () => authority,
                commit,
                coordinator,
                database,
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
