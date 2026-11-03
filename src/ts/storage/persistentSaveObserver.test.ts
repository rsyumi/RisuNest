import { flushSync } from 'svelte'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Chat, Database } from './database.svelte'
import { ActiveConversationSession } from './activeConversationSession'
import { observePersistentSaveChanges } from './persistentSaveObserver.svelte'
import { PersistentMutationFencedError, SaveCoordinator } from './saveCoordinator'
import type { PersistentDataStore } from './persistentDataStore'
import { createStoreStub } from './persistentDataStore.testSupport'
import { PENDING_SAVE_BYTE_LIMIT } from './pendingDataSize'
import { createMetadataOnlySelectedConversation } from './selectedConversationLifecycle'
import { createPersistentSaveObserverHarness } from './tests/persistentSaveObserverHarness.svelte'
import { createPersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'

const disposers: Array<() => void> = []
afterEach(() => {
    for (const dispose of disposers.splice(0)) dispose()
    vi.useRealTimers()
})

function fixture(): Database {
    return {
        username: 'Synthetic user',
        pluginCustomStorage: { largeUnchangedValue: 'x'.repeat(2 * 1_048_576) },
        characters: [
            {
                type: 'character',
                chaId: 'synthetic-character',
                chatPage: 0,
                chats: [{ id: 'synthetic-chat', message: [{ role: 'user', data: 'Hello' }] }],
            },
        ],
    } as unknown as Database
}

describe('persistent save mutation observation', () => {
    it('tracks added and removed root fields without reading hidden properties', () => {
        const database = fixture()
        Object.defineProperty(database, 'hiddenObserverProbe', {
            enumerable: false,
            get() { throw new Error('Hidden properties are not persistent fields') },
        })
        const state = createPersistentSaveObserverHarness(database)
        const markDirty = vi.fn()
        disposers.push(observePersistentSaveChanges({
            readDatabase: () => state.database,
            readSelectedCharacter: () => null,
            markDirty,
        }))
        flushSync()
        markDirty.mockClear()
        const root = state.database as unknown as Record<string, unknown>
        root.syntheticObserverField = { value: 'one' }
        flushSync()
        expect(markDirty).toHaveBeenCalled()

        const added = root.syntheticObserverField as { value: string }
        markDirty.mockClear()
        added.value = 'two'
        flushSync()
        expect(markDirty).toHaveBeenCalledOnce()

        markDirty.mockClear()
        delete root.syntheticObserverField
        flushSync()
        expect(markDirty).toHaveBeenCalled()
        markDirty.mockClear()
        added.value = 'detached'
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()
    })

    it('keeps observing when compatibility code temporarily clears the message value', () => {
        const state = createPersistentSaveObserverHarness(fixture())
        const markDirty = vi.fn()
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[0],
                markDirty,
            }),
        )
        flushSync()
        state.database.characters[0].chats[0].message = undefined
        expect(() => flushSync()).not.toThrow()
        state.database.characters[0].chats[0].message = [{ role: 'char', data: 'Restored' }]
        flushSync()
        markDirty.mockClear()
        state.database.characters[0].chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()
    })

    it('does not revisit unrelated root fields or sibling messages', () => {
        const database = fixture()
        const rootProbe = vi.fn()
        const siblingProbe = vi.fn()
        database.pluginCustomStorage.deep = {
            get probe() {
                rootProbe()
                return 1
            },
        }
        database.characters[0].chats[0].message.push({
            role: 'char',
            data: 'Second',
            get probe() {
                siblingProbe()
                return 1
            },
        } as unknown as Database['characters'][number]['chats'][number]['message'][number])
        const state = createPersistentSaveObserverHarness(database)
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[0],
                markDirty: vi.fn(),
            }),
        )
        flushSync()
        rootProbe.mockClear()
        siblingProbe.mockClear()
        state.database.username += '!'
        flushSync()
        expect(rootProbe).not.toHaveBeenCalled()
        expect(siblingProbe).not.toHaveBeenCalled()
        state.database.characters[0].chats[0].message[0].data += '!'
        flushSync()
        expect(rootProbe).not.toHaveBeenCalled()
        expect(siblingProbe).not.toHaveBeenCalled()
    })

    it('walks only the changed history and skips all message bodies for metadata edits', () => {
        const database = fixture()
        const probes = [vi.fn(), vi.fn()]
        const first = database.characters[0].chats[0]
        first.scriptstate = { count: 0 }
        const second = structuredClone(first)
        second.id = 'synthetic-second'
        database.characters[0].chats.push(second)
        for (let index = 0; index < 2; index++) {
            Object.defineProperty(database.characters[0].chats[index].message[0], 'probe', {
                enumerable: true,
                get: () => {
                    probes[index]()
                    return true
                },
            })
        }
        const state = createPersistentSaveObserverHarness(database)
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[0],
                markDirty: vi.fn(),
            }),
        )
        flushSync()
        for (const probe of probes) probe.mockClear()

        state.database.characters[0].chats[0].scriptstate.count = 1
        flushSync()
        expect(probes.map((probe) => probe.mock.calls.length)).toEqual([0, 0])

        state.database.characters[0].chats[0].message[0].data += '!'
        flushSync()
        expect(probes[0]).toHaveBeenCalled()
        expect(probes[1]).not.toHaveBeenCalled()
    })

    it('follows chat insertion, reorder, removal, replacement and deleted metadata keys', () => {
        const state = createPersistentSaveObserverHarness(fixture())
        const markDirty = vi.fn()
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[0],
                markDirty,
            }),
        )
        flushSync()
        const chats = state.database.characters[0].chats
        chats.push({
            id: 'synthetic-added',
            message: [{ role: 'char', data: 'Added' }],
            note: 'Temporary',
        } as any)
        flushSync()
        markDirty.mockClear()
        chats[1].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()

        chats.reverse()
        flushSync()
        markDirty.mockClear()
        chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()
        markDirty.mockClear()
        delete chats[0].note
        flushSync()
        expect(markDirty).toHaveBeenCalled()

        const removed = chats[0]
        chats.splice(0, 1)
        flushSync()
        markDirty.mockClear()
        removed.message[0].data += '!'
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()

        state.database.characters[0].chats = [
            {
                id: 'synthetic-replacement',
                message: [{ role: 'user', data: 'Replacement' }],
            } as any,
        ]
        flushSync()
        markDirty.mockClear()
        chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()
        state.database.characters[0].chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()

        const replaced = state.database.characters[0].chats[0]
        state.database.characters[0].chats[0] = {
            id: 'synthetic-slot',
            message: [{ role: 'user', data: 'Slot replacement' }],
        } as any
        flushSync()
        markDirty.mockClear()
        replaced.message[0].data += '!'
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()

        const deleted = state.database.characters[0].chats[0]
        delete state.database.characters[0].chats[0]
        flushSync()
        expect(markDirty).toHaveBeenCalled()
        markDirty.mockClear()
        deleted.message[0].data += '!'
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()
    })

    it('observes every materialized character and stops observing when disposed', () => {
        const database = fixture()
        const next = structuredClone(database.characters[0])
        next.chaId = 'synthetic-next'
        database.characters.push(next)
        const state = createPersistentSaveObserverHarness(database)
        const markDirty = vi.fn()
        const dispose = observePersistentSaveChanges({
            readDatabase: () => state.database,
            readSelectedCharacter: () => state.database.characters[state.selectedIndex] ?? null,
            markDirty,
        })
        disposers.push(dispose)
        flushSync()
        const previous = state.database.characters[0]
        state.selectedIndex = 1
        flushSync()
        markDirty.mockClear()
        previous.chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()
        state.database.characters[1].chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()
        state.selectedIndex = -1
        flushSync()
        markDirty.mockClear()
        state.database.characters[1].chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty).toHaveBeenCalled()

        dispose()
        markDirty.mockClear()
        state.database.username += '!'
        state.selectedIndex = 0
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()
    })

    it('observes metadata-only shells without reading their throwing message getter', () => {
        const database = fixture()
        database.characters[0].chats[0] = createMetadataOnlySelectedConversation(
            database.characters[0].chats[0],
        )
        const state = createPersistentSaveObserverHarness(database)
        const markDirty = vi.fn()
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[0],
                markDirty,
            }),
        )
        expect(() => flushSync()).not.toThrow()
        markDirty.mockClear()
        state.database.characters[0].chats[0].note = 'Synthetic metadata'
        expect(() => flushSync()).not.toThrow()
        expect(markDirty).toHaveBeenCalled()
    })

    it('debounces repeated small edits in a large store and preserves explicit byte-limit saves', async () => {
        vi.useFakeTimers()
        const state = createPersistentSaveObserverHarness(fixture())
        const captureRoot = vi.fn(() => {
            const { characters: _characters, ...root } = state.database
            return root
        })
        let revision = 1
        const commit = vi.fn(async () => ({ revision: ++revision }))
        const coordinator = new SaveCoordinator({
            store: createStoreStub({ commit }),
            captureRoot,
            captureSelectedCharacter: () => state.database.characters[0],
            captureCharacter: () => state.database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(revision)
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[state.selectedIndex] ?? null,
                markDirty: (bytes) => coordinator.markPersistentDataDirty(bytes),
            }),
        )
        flushSync()
        await vi.advanceTimersByTimeAsync(500)
        captureRoot.mockClear()
        commit.mockClear()

        for (let index = 0; index < 10; index++) {
            state.database.username += '!'
            flushSync()
            await vi.advanceTimersByTimeAsync(16)
        }
        expect(captureRoot).not.toHaveBeenCalled()
        expect(commit).not.toHaveBeenCalled()

        await vi.advanceTimersByTimeAsync(500)
        expect(commit).toHaveBeenCalledOnce()
        expect(captureRoot).toHaveBeenCalledTimes(2)

        state.database.username = 'Explicit large mutation'
        coordinator.markPersistentDataDirty(PENDING_SAVE_BYTE_LIMIT)
        await coordinator.flushPendingData('explicit-large-mutation')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('does not mistake the existing database size for changed bytes', () => {
        const state = createPersistentSaveObserverHarness(fixture())
        const markDirty = vi.fn()
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[state.selectedIndex] ?? null,
                markDirty,
            }),
        )
        flushSync()
        markDirty.mockClear()

        state.database.username += '!'
        flushSync()
        expect(markDirty.mock.calls).toEqual([[0]])

        markDirty.mockClear()
        state.database.characters[0].chats[0].message[0].data += '!'
        flushSync()
        expect(markDirty.mock.calls).toEqual([[0]])
    })

    it('lets a replacement install its database while mutations are fenced', () => {
        const state = createPersistentSaveObserverHarness(fixture())
        let fenced = false
        const markDirty = vi.fn(() => {
            if (fenced) throw new PersistentMutationFencedError()
        })
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[state.selectedIndex] ?? null,
                markDirty,
            }),
        )
        flushSync()

        fenced = true
        state.database = fixture()
        expect(() => flushSync()).not.toThrow()

        fenced = false
        markDirty.mockClear()
        state.database.username = 'Edited after the replacement'
        flushSync()
        expect(markDirty).toHaveBeenCalled()
        markDirty.mockClear()
        ;(state.database as unknown as Record<string, unknown>).syntheticObserverField = 'added'
        flushSync()
        expect(markDirty).toHaveBeenCalled()

        markDirty.mockImplementation(() => {
            throw new Error('Synthetic save failure')
        })
        state.database.username = 'Edited again'
        expect(() => flushSync()).toThrow('Synthetic save failure')
    })

    it('keeps compatibility mutations observable and coordinator reads untracked', () => {
        const state = createPersistentSaveObserverHarness(fixture())
        const markDirty = vi.fn(() => {
            void state.unrelated
        })
        disposers.push(
            observePersistentSaveChanges({
                readDatabase: () => state.database,
                readSelectedCharacter: () => state.database.characters[state.selectedIndex] ?? null,
                markDirty,
            }),
        )
        flushSync()
        markDirty.mockClear()

        state.unrelated += 1
        flushSync()
        expect(markDirty).not.toHaveBeenCalled()

        state.database.pluginCustomStorage.nested = { values: ['synthetic'] }
        flushSync()
        expect(markDirty).toHaveBeenCalledOnce()
        markDirty.mockClear()
        ;(state.database.pluginCustomStorage.nested as { values: string[] }).values.push('new')
        flushSync()
        expect(markDirty).toHaveBeenCalledOnce()
        markDirty.mockClear()

        delete state.database.pluginCustomStorage.nested
        flushSync()
        expect(markDirty).toHaveBeenCalledOnce()
    })

    it.each(['metadata', 'message'] as const)('saves %s changes after complete promotion without reading unchanged message bodies and writes what a copied baseline writes', async (kind) => {
        const body = 'synthetic message body'
        const flush = async (capture: 'reactive' | 'plain') => {
            const initial = {
                username: 'Synthetic user',
                characters: [{
                    type: 'character',
                    chaId: 'synthetic-character',
                    chatPage: 0,
                    name: 'Synthetic',
                    chats: [{
                        id: 'synthetic-chat',
                        name: 'Synthetic chat',
                        note: '',
                        localLore: [],
                        message: [
                            { role: 'user', data: `first: ${body}` },
                            { role: 'char', data: `second: ${body}` },
                            { role: 'char', data: 'synthetic response draft', chatId: 'response-id' },
                        ],
                    }],
                }],
            } as unknown as Database
            const state = capture === 'reactive' ? createPersistentSaveObserverHarness(initial) : null
            const database = state?.database ?? initial
            let revision = 1
            const commit = vi.fn(async (_request: Parameters<PersistentDataStore['commit']>[0]) => ({ revision: ++revision }))
            const onPersisted = vi.fn()
            const canonicalCapture = state ? createPersistenceCanonicalCapture({
                root: () => state.database,
                presets: () => [],
                pluginStorage: () => null,
                character: () => state.database.characters[0],
                characters: () => state.database.characters,
            }) : undefined
            const coordinator = new SaveCoordinator({
                store: createStoreStub({ commit, replaceFromDatabase: vi.fn() }),
                canonicalCapture,
                captureRoot: () => {
                    const { characters: _characters, ...root } = database
                    return root
                },
                captureCharacters: () => database.characters,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                onConversationMutationPersisted: onPersisted,
            })
            coordinator.initialize(revision)
            const session = new ActiveConversationSession({
                characterId: 'synthetic-character',
                conversationId: 'synthetic-chat',
                conversation: database.characters[0].chats[0],
                storeRevision: revision,
                onMutation: (event) => coordinator.recordActiveConversationMutation(event),
            })
            // Promotion adopts the resident character as the baseline.
            expect(coordinator.adoptHydratedCharacter(revision, coordinator.mutationGeneration, database.characters[0])).toBe(true)
            const baseline = canonicalCapture?.materializedCharacters?.().get('synthetic-character') ?? null
            const baselineJson = JSON.stringify(baseline)
            if (kind === 'metadata') {
                session.updateChatVariables(new Map([['hp', '9']]))
                session.updateChatVariables(new Map([['mood', 'calm']]))
            } else {
                session.edit(session.locate(2), { role: 'char', data: 'updated response', chatId: 'response-id' })
                session.edit(session.locate(2), { role: 'char', data: 'final response', chatId: 'response-id', custom: { nested: ['한글', '🙂'] } } as Chat['message'][number])
            }

            // Whether the flush serializes or copies an unchanged message body.
            const readsMessages: string[] = []
            const stringify = JSON.stringify
            const clone = globalThis.structuredClone
            const serialized = vi.spyOn(JSON, 'stringify').mockImplementation(((...args: Parameters<typeof JSON.stringify>) => {
                const json = stringify(...args)
                if (typeof json === 'string' && json.includes(body)) readsMessages.push('serialize')
                return json
            }) as typeof JSON.stringify)
            const copied = vi.spyOn(globalThis, 'structuredClone').mockImplementation(((value: unknown, options?: StructuredSerializeOptions) => {
                try {
                    if (stringify(value)?.includes(body)) readsMessages.push('copy')
                } catch {
                    // Not serializable as JSON; the clone below decides.
                }
                return clone(value, options)
            }) as typeof structuredClone)
            try {
                await coordinator.flushPendingDataLocally('variable-only')
            } finally {
                serialized.mockRestore()
                copied.mockRestore()
            }
            expect(onPersisted).toHaveBeenCalledTimes(2)
            expect(coordinator.hasPendingPersistenceWork).toBe(false)
            expect(JSON.stringify(baseline)).toBe(baselineJson)
            return { request: commit.mock.calls[0][0], readsMessages }
        }

        const reactive = await flush('reactive')
        const plain = await flush('plain')

        expect(reactive.readsMessages).toEqual([])
        expect(plain.readsMessages).not.toEqual([])
        expect(reactive.request).toEqual(plain.request)
        if (kind === 'metadata') {
            expect(reactive.request.conversations?.map((mutation) =>
                'conversation' in mutation ? (mutation.conversation as Chat).scriptstate : null,
            )).toEqual([{ $hp: '9', $mood: 'calm' }])
        } else {
            expect(reactive.request.conversations?.map((mutation) =>
                mutation.type === 'replace-range' ? [mutation.start, mutation.deleteCount, mutation.messages] : null,
            )).toEqual([
                [2, 1, [{ role: 'char', data: 'final response', chatId: 'response-id', custom: { nested: ['한글', '🙂'] } }]],
            ])
        }
    })
})
