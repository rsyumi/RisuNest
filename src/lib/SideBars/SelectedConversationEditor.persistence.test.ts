import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { createRawSnippet, mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { languageEnglish } from '../../lang/en'
import { DBState } from 'src/ts/stores.svelte'
import type { Database } from 'src/ts/storage/database.svelte'
import { IndexedDbPersistentDataStore } from 'src/ts/storage/indexedDbPersistentDataStore'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
} from 'src/ts/storage/persistentDataRuntime'
import { fixtureDatabase } from 'src/ts/storage/tests/persistentDataFixtures'
import SelectedConversationEditor from './SelectedConversationEditor.svelte'

const state = vi.hoisted(() => ({
    runtime: null as ReturnType<typeof createPersistentDataRuntime> | null,
}))
vi.mock('src/lang', () => ({ language: languageEnglish }))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { db: {} },
    selectedCharID: writable(0),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => state.runtime,
}))

describe('upstream bound editors with windowed persistence', () => {
    it.each(['character', 'group'] as const)(
        'saves %s detail and conversation edits without losing unloaded history',
        async (type) => {
            const indexedDB = new IDBFactory()
            const store = new IndexedDbPersistentDataStore(
                'bound-editor',
                indexedDB,
                IDBKeyRange,
            )
            await store.open()
            let database = structuredClone(fixtureDatabase)
            database.characters = [database.characters[1]]
            database.characters[0].type = type
            const original = structuredClone(database.characters[0])
            await store.replaceFromDatabase(database)
            const runtime = createPersistentDataRuntime({
                store,
                state: {
                    captureRoot: () => capturePersistentRoot(database),
                    capturePresets: () => database.botPresets,
                    captureSelectedCharacter: () => database.characters[0],
                    captureCharacter: (id) =>
                        database.characters.find(
                            (entry) => entry.chaId === id,
                        ) ?? null,
                    getSelectedCharacterId: () => database.characters[0].chaId,
                    getSelectedConversationId: () =>
                        database.characters[0].chats[
                            database.characters[0].chatPage
                        ].id,
                    replaceDatabase: (value: Database) => {
                        database = value
                    },
                    publishCharacter: (value) => {
                        database.characters[0] = value
                    },
                    publishConversation: (_id, conversation, nextCharacter) => {
                        if (nextCharacter)
                            database.characters[0] = nextCharacter
                        else {
                            const owner = database.characters[0]
                            const index = owner.chats.findIndex(
                                (chat) => chat.id === conversation.id,
                            )
                            owner.chats[index] = conversation
                            owner.chatPage = index
                        }
                    },
                    shouldHydrateFullCharacter: () => false,
                    canUseWindowedSelectedConversation: () => true,
                    canReleaseConversation: () => true,
                    conversationViewportRowBudget: 32,
                },
                prepareDatabase: async (value) => value,
            })
            await runtime.initializeActiveWorkingSet(database)
            await runtime.activateCharacter(original.chaId)
            expect(runtime.getActiveConversationSession()).toBeNull()

            state.runtime = runtime
            DBState.db = database
            const mounted = vi.fn()
            const editor = mount(SelectedConversationEditor, {
                target: document.body,
                props: {
                    children: createRawSnippet(() => ({
                        render: () => '<span>Editor</span>',
                        setup: mounted,
                    })),
                },
            })
            await vi.waitFor(() => expect(mounted).toHaveBeenCalled())
            await tick()
            expect(runtime.getActiveConversationSession()).not.toBeNull()

            const owner = database.characters[0]
            owner.name = '편집된 이름'
            owner.firstMessage = 'Edited greeting'
            owner.modules = ['synthetic-character-module']
            owner.chats[0].modules = ['synthetic-conversation-module']
            owner.customscript = [
                {
                    in: 'a',
                    out: 'b',
                    type: 'editoutput',
                    flag: 'g',
                    comment: 'Synthetic',
                },
            ]
            owner.chats[0].note = 'Edited note'
            owner.chats[0].localLore = [
                { key: 'synthetic', content: 'Edited lore' },
            ] as any
            owner.chats[1].name = 'Renamed unloaded conversation'
            runtime.markPersistentDataDirty(0)
            await runtime.flushPendingData('bound-editor-test')
            expect(
                (await store.readCharacter(original.chaId))?.value.name,
            ).toBe('편집된 이름')
            owner.name = ''
            owner.chats[0].name = ''
            runtime.markPersistentDataDirty(0)
            await runtime.flushPendingData('empty-name-test')

            owner.chatFolders = [
                {
                    id: 'folder',
                    name: 'Edited folder',
                    folded: true,
                    color: 'blue',
                },
            ]
            owner.chats[1].folderId = 'folder'
            owner.chats.reverse()
            owner.chatPage = 1
            runtime.markPersistentDataDirty(0)
            await runtime.flushPendingData('chat-folder-order-test')

            const reopened = new IndexedDbPersistentDataStore(
                'bound-editor',
                indexedDB,
                IDBKeyRange,
            )
            await reopened.open()
            expect(
                (await reopened.readCharacter(original.chaId))?.value,
            ).toMatchObject({
                name: '',
                firstMessage: 'Edited greeting',
                modules: ['synthetic-character-module'],
                customscript: owner.customscript,
                chatFolders: owner.chatFolders,
            })
            const selected = (await reopened.readConversation(
                original.chaId,
                original.chats[0].id!,
            ))!.value
            expect(selected).toMatchObject({
                name: '',
                note: 'Edited note',
                modules: ['synthetic-conversation-module'],
                localLore: owner.chats[1].localLore,
            })
            expect(selected.message).toEqual(original.chats[0].message)
            const unloaded = (await reopened.readConversation(
                original.chaId,
                original.chats[1].id!,
            ))!.value
            expect(unloaded).toEqual({
                ...original.chats[1],
                name: 'Renamed unloaded conversation',
                folderId: 'folder',
            })
            expect(
                (
                    await reopened.queryConversations({
                        characterId: original.chaId,
                        order: 'configured',
                        limit: 10,
                    })
                ).items.map((entry) => entry.id),
            ).toEqual([original.chats[1].id, original.chats[0].id])
            await unmount(editor)
            document.body.replaceChildren()
        },
    )
})
