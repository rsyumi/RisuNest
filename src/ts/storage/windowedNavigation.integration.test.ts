import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { flushSync } from 'svelte'

vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))
vi.mock('../process/modules', () => ({ moduleUpdate: vi.fn() }))
vi.mock('../process/scripts', () => ({ resetScriptCache: vi.fn() }))

import { characterFormatUpdate } from '../characters'
import { selectedCharID, selIdState } from '../stores.svelte'
import type { Chat, Database, Message, character } from './database.svelte'
import { getDatabase, setDatabaseLite } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { createPersistentDataRuntime } from './persistentDataRuntime'
import { createProductionStateAdapter } from './persistentDataRuntime.svelte'
import { workingSetResidency } from './workingSetResidency'
import { observePersistentSaveChanges } from './persistentSaveObserver.svelte'

afterEach(() => {
    workingSetResidency.clear()
    workingSetResidency.setEvictionAllowed(true)
    selectedCharID.set(-1)
})

function makeLargeLegacyDatabase(): {
    database: Database
    messages: Message[]
} {
    const messages = Array.from({ length: 10_000 }, (_, index) => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: `synthetic-message-${index}`,
        chatId: `synthetic-id-${index}`,
        time: 1_800_000_000_000 + index,
    })) as Message[]
    const conversation = {
        id: 'legacy-chat',
        name: 'Legacy metadata',
        note: 'Original note',
        message: messages,
        scriptstate: { arbitrary: ['metadata', 7] },
        lastDate: 1_800_000_010_000,
    } as unknown as Chat
    const legacyCharacter = {
        type: 'character',
        chaId: 'legacy-character',
        name: 'Legacy character',
        image: '',
        firstMessage: 'Hello',
        desc: 'Synthetic legacy fixture',
        notes: '',
        chats: [conversation],
        chatFolders: [],
        chatPage: 0,
        viewScreen: 'none',
        bias: [],
        emotionImages: [],
        globalLore: [
            {
                key: 'legacy lore',
                content: '<char> @@@end',
                activationPercent: 25,
            },
        ],
        postHistoryInstructions: 'Migrated instruction',
        replaceGlobalNote: '',
        firstMsgIndex: 7,
        lastInteraction: 1,
        newGenData: {},
        syntheticCharacterField: { preserved: true },
    } as unknown as character
    return {
        database: {
            username: 'Synthetic user',
            botPresets: [],
            plugins: [],
            characters: [legacyCharacter],
        } as unknown as Database,
        messages,
    }
}

describe('windowed navigation integration', () => {
    it.each(['immediate', 'autosave'])('preserves cold-selection editor changes and a user-only turn through %s navigation', async (timing) => {
        const indexedDB = new IDBFactory()
        const databaseName = `cold-selection-${timing}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const { database } = makeLargeLegacyDatabase()
        const first = database.characters[0] as character
        first.chats[0].message = [{ role: 'char', data: 'Synthetic existing history' }]
        const second = structuredClone(first)
        second.chaId = 'second-character'
        second.chats[0].id = 'second-chat'
        database.characters.push(second)
        await store.replaceFromDatabase(database)
        setDatabaseLite(structuredClone(database))
        selectedCharID.set(-1)
        const state = createProductionStateAdapter()
        expect(state.canonicalCapture!.character()).toBeNull()
        const runtime = createPersistentDataRuntime({ store, state, prepareDatabase: async (value) => value })
        await runtime.initializeActiveWorkingSet(getDatabase())
        const navigate = async (id: string) => {
            const expectedGeneration = runtime.getNavigationGeneration() + 1
            if (await runtime.activateCharacter(id)) return true
            if (runtime.getNavigationGeneration() !== expectedGeneration) return false
            // Match changeChar's single retry after observer invalidation.
            return runtime.activateCharacter(id)
        }
        const dispose = observePersistentSaveChanges({
            readDatabase: getDatabase,
            readSelectedCharacter: () => getDatabase().characters[selIdState.selId] ?? null,
            markDirty: (bytes) => runtime.markPersistentDataDirty(bytes),
        })
        try {
            flushSync()
            expect(await navigate(first.chaId)).toBe(true)
            const lease = await runtime.acquireCompleteConversation('synthetic-bound-editor')
            try {
                const live = getDatabase().characters[selIdState.selId] as character
                live.name = '수정 🙂'
                live.desc = 'Synthetic edited description'
                live.chats[0].note = 'Synthetic edited note'
                lease.session.append({ role: 'user', data: 'Synthetic user-only turn' })
                flushSync()
                if (timing === 'autosave') {
                    await vi.waitFor(async () => {
                        expect((await store.readCharacter(first.chaId))?.value.name).toBe('수정 🙂')
                        expect((await store.readConversation(first.chaId, 'legacy-chat'))?.value.message).toHaveLength(2)
                    }, { timeout: 3000 })
                }
            } finally {
                lease.release()
            }
            expect(await navigate(second.chaId)).toBe(true)
            expect(await navigate(first.chaId)).toBe(true)
            expect(getDatabase().characters[selIdState.selId].name).toBe('수정 🙂')
            const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
            await reopened.open()
            expect((await reopened.readCharacter(first.chaId))?.value).toMatchObject({
                name: '수정 🙂', desc: 'Synthetic edited description',
            })
            expect((await reopened.readConversation(first.chaId, 'legacy-chat'))?.value).toMatchObject({
                note: 'Synthetic edited note',
                message: [
                    { role: 'char', data: 'Synthetic existing history' },
                    { role: 'user', data: 'Synthetic user-only turn' },
                ],
            })
        } finally {
            dispose()
        }
    })

    it('normalizes through the production Svelte adapter and survives immediate promotion and restart', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'windowed-navigation-normalize-promote-restart'
        const store = new IndexedDbPersistentDataStore(
            databaseName,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const { database, messages } = makeLargeLegacyDatabase()
        const imported = await store.replaceFromDatabase(database)

        setDatabaseLite(structuredClone(database))
        selectedCharID.set(0)
        workingSetResidency.setEvictionAllowed(false)
        const runtime = createPersistentDataRuntime({
            store,
            state: createProductionStateAdapter(),
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(getDatabase())
        workingSetResidency.setEvictionAllowed(true)

        const readConversation = vi.spyOn(store, 'readConversation')
        expect(
            await runtime.activateCharacter('legacy-character', {
                normalize: (candidate) =>
                    characterFormatUpdate(candidate, {
                        updateInteraction: true,
                    }),
            }),
        ).toBe(true)

        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        expect(readConversation).not.toHaveBeenCalled()
        const windowedCharacter = getDatabase().characters[0] as character
        const windowedConversation = windowedCharacter.chats[0]
        expect(windowedCharacter).toMatchObject({
            chaId: 'legacy-character',
            postHistoryInstructions: null,
            firstMsgIndex: 7,
            syntheticCharacterField: { preserved: true },
        })
        expect(windowedCharacter.lastInteraction).toBeGreaterThan(1)
        expect(windowedCharacter.globalLore[0]).toMatchObject({
            bookVersion: 2,
            activationPercent: null,
            content: '@@probability 25\n{{char}} @@depth 0',
        })
        expect(windowedConversation).toMatchObject({
            id: 'legacy-chat',
            note: 'Original note\nMigrated instruction',
            fmIndex: 7,
            localLore: [],
            scriptstate: { arbitrary: ['metadata', 7] },
        })

        const target = runtime.captureSelectedConversationTarget()
        expect(target).not.toBeNull()
        const completeLease = await runtime.acquireCompleteConversation(
            'integration-immediate-promotion',
            target,
        )
        try {
            expect(runtime.getSelectedConversationMode()).toBe('complete')
            expect(readConversation).toHaveBeenCalledTimes(1)
            expect(completeLease.session.totalMessages).toBe(messages.length)
            expect(completeLease.session.readRange(0, 128).messages).toEqual(
                messages.slice(0, 128),
            )
            expect(completeLease.session.readLatest(128).messages).toEqual(
                messages.slice(-128),
            )

            const reopened = new IndexedDbPersistentDataStore(
                databaseName,
                indexedDB,
                IDBKeyRange,
            )
            await reopened.open()
            const detail = await reopened.readCharacter('legacy-character')
            const persisted = await reopened.readConversation(
                'legacy-character',
                'legacy-chat',
            )
            expect(detail).toMatchObject({
                revision: imported.revision + 1,
                value: {
                    postHistoryInstructions: null,
                    firstMsgIndex: 7,
                    syntheticCharacterField: { preserved: true },
                },
            })
            expect(persisted).toMatchObject({
                revision: imported.revision + 1,
                value: {
                    note: 'Original note\nMigrated instruction',
                    fmIndex: 7,
                    localLore: [],
                    scriptstate: { arbitrary: ['metadata', 7] },
                },
            })
            expect(persisted?.value.message).toEqual(messages)
            expect(
                persisted?.value.note.match(/Migrated instruction/g),
            ).toHaveLength(1)
        } finally {
            completeLease.release()
        }
    })
})
