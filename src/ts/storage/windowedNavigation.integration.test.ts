import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterEach, describe, expect, it, vi } from 'vitest'

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
import { selectedCharID } from '../stores.svelte'
import type { Chat, Database, Message, character } from './database.svelte'
import { getDatabase, setDatabaseLite } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { createPersistentDataRuntime } from './persistentDataRuntime'
import { createProductionStateAdapter } from './persistentDataRuntime.svelte'
import { workingSetResidency } from './workingSetResidency'

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
