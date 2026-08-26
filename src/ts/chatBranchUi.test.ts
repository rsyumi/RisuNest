import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import {
    captureChatMessageTarget,
    type ChatMessageUiContext,
} from './chatMessageUi'
import {
    createPersistentDataRuntime,
    capturePersistentPluginStorage,
    capturePersistentPresets,
    capturePersistentRoot,
} from './storage/persistentDataRuntime'
import type { Chat, Database, Message } from './storage/database.svelte'
import { IndexedDbPersistentDataStore } from './storage/indexedDbPersistentDataStore'
import { RevisionConflictError } from './storage/persistentDataStore'
import { createCapturedConversationBranch } from './chatBranchUi'

function message(data: string, chatId?: string): Message {
    return {
        role: 'user',
        data,
        ...(chatId === undefined ? {} : { chatId }),
    }
}

function makeDatabase(messages: Message[] = [
    message('zero', 'duplicate'),
    message('one'),
    message('two', 'duplicate'),
]): Database {
    return {
        username: 'User',
        botPresets: [],
        pluginCustomStorage: {},
        characters: [{
            type: 'character',
            chaId: 'char-a',
            name: 'Character',
            firstMessage: 'Greeting',
            alternateGreetings: [],
            chatPage: 0,
            chatFolders: [],
            unknownCharacterField: { keep: false },
            chats: [{
                id: 'source-chat',
                name: 'Source',
                note: 'source note',
                localLore: [{ key: 'lore', content: 'value' }],
                bookmarks: ['duplicate'],
                bookmarkNames: { duplicate: 'Duplicate' },
                unknownConversationField: { keep: 0 },
                message: messages,
            }, {
                id: 'other-chat',
                name: 'Other',
                note: '',
                localLore: [],
                message: [message('other', 'other')],
            }],
        }],
    } as unknown as Database
}

async function createHarness(name: string, initial = makeDatabase()) {
    const store = new IndexedDbPersistentDataStore(name, new IDBFactory(), IDBKeyRange)
    await store.open()
    const imported = await store.replaceFromDatabase(structuredClone(initial))
    let database = structuredClone(initial)
    let selectedConversationId = 'source-chat'
    const forcedScalableProjection: Array<boolean | undefined> = []
    const runtime = createPersistentDataRuntime({
        store,
        state: {
            captureRoot: () => capturePersistentRoot(database),
            capturePluginStorage: () => capturePersistentPluginStorage(database),
            capturePresets: () => capturePersistentPresets(database),
            captureSelectedCharacter: () => database.characters[0] ?? null,
            captureCharacter: (id) => database.characters.find(
                (character) => character.chaId === id,
            ) ?? null,
            getSelectedCharacterId: () => 'char-a',
            getSelectedConversationId: () => selectedConversationId,
            replaceDatabase: (replacement, _activeCharacterIds, forceScalableProjection) => {
                forcedScalableProjection.push(forceScalableProjection)
                database = structuredClone(replacement)
            },
            publishCharacter: (character) => {
                const index = database.characters.findIndex(
                    (candidate) => candidate.chaId === character.chaId,
                )
                if (index >= 0) database.characters[index] = structuredClone(character)
            },
            publishConversation: (characterId, conversation, nextCharacter) => {
                const characterIndex = database.characters.findIndex(
                    (candidate) => candidate.chaId === characterId,
                )
                if (characterIndex < 0) return
                if (nextCharacter) {
                    database.characters[characterIndex] = structuredClone(nextCharacter)
                } else {
                    const character = database.characters[characterIndex]
                    const conversationIndex = character.chats.findIndex(
                        (candidate) => candidate.id === conversation.id,
                    )
                    if (conversationIndex >= 0) {
                        character.chats[conversationIndex] = structuredClone(conversation)
                        character.chatPage = conversationIndex
                    }
                }
                selectedConversationId = conversation.id!
            },
        },
        prepareDatabase: async (value) => value,
    })
    await runtime.initializeActiveWorkingSet(database)

    const context: ChatMessageUiContext = {
        captureCurrent: () => {
            const character = database.characters.find((candidate) => candidate.chaId === 'char-a')
            const conversation = character?.chats.find(
                (candidate) => candidate.id === selectedConversationId,
            )
            return character && conversation ? { character, conversation } : null
        },
        getCurrentSession: () => runtime.getActiveConversationSession(),
    }

    return {
        store,
        runtime,
        context,
        imported,
        forcedScalableProjection,
        get database() { return database },
        selectConversation(id: string) {
            selectedConversationId = id
            runtime.invalidateNavigation()
        },
        async navigate(id: string) {
            const activated = await runtime.activateConversation(id)
            if (activated) selectedConversationId = id
            return activated
        },
    }
}

function idSequence(...ids: string[]) {
    const createId = vi.fn(() => {
        const id = ids.shift()
        if (!id) throw new Error('Unexpected ID request')
        return id
    })
    return createId
}

describe('captured conversation branch UI', () => {
    it('creates the exact prefix branch through the paged store and preserves folder metadata', async () => {
        const harness = await createHarness('captured-branch-success')
        const target = captureChatMessageTarget({
            ...harness.context,
            absoluteIndex: 1,
        })
        expect(target?.session).not.toBeNull()

        const created = await createCapturedConversationBranch({
            target: target!,
            context: harness.context,
            runtime: harness.runtime,
            createFolderOnBranch: true,
            createId: idSequence('folder-id', 'branch-id', 'marker-id'),
            createBranchName: () => 'Source (Branch)',
            navigateToBranch: (id) => harness.navigate(id),
            pageSize: 1,
        })

        expect(created).toBe(true)
        const materialized = await harness.store.materializeDatabase()
        const character = materialized.characters[0]
        expect(character.chatFolders).toEqual([{
            id: 'folder-id',
            name: 'Branches of Source',
            folded: false,
        }])
        expect(character).toMatchObject({ unknownCharacterField: { keep: false } })
        expect(character.chats.map((chat) => chat.id)).toEqual([
            'branch-id',
            'source-chat',
            'other-chat',
        ])
        expect(character.chats[1].folderId).toBe('folder-id')
        expect(character.chats[1].message.map((entry) => entry.data)).toEqual([
            'zero',
            'one',
            'two',
        ])
        expect(character.chats[0]).toEqual({
            id: 'branch-id',
            name: 'Source (Branch)',
            note: 'source note',
            localLore: [{ key: 'lore', content: 'value' }],
            bookmarks: ['duplicate'],
            bookmarkNames: { duplicate: 'Duplicate' },
            unknownConversationField: { keep: 0 },
            folderId: 'folder-id',
            message: [
                message('zero', 'duplicate'),
                message('one'),
                {
                    role: 'char',
                    data: '{{specialcomment::branchedfrom::source-chat::Source::undefined::}}',
                    isComment: true,
                    disabled: true,
                    chatId: 'marker-id',
                },
            ],
        })
        expect(harness.context.captureCurrent()?.conversation.id).toBe('branch-id')
    })

    it('persists the inserted branch as the selected conversation from a nonzero source index', async () => {
        const initial = makeDatabase()
        initial.characters[0].chats.reverse()
        initial.characters[0].chatPage = 1
        const harness = await createHarness('captured-branch-nonzero-source', initial)
        const target = captureChatMessageTarget({ ...harness.context, absoluteIndex: 1 })!

        await expect(createCapturedConversationBranch({
            target,
            context: harness.context,
            runtime: harness.runtime,
            createFolderOnBranch: false,
            createId: idSequence('branch-id', 'marker-id'),
            createBranchName: () => 'Source (Branch)',
            navigateToBranch: (id) => harness.navigate(id),
        })).resolves.toBe(true)

        const materialized = await harness.store.materializeDatabase()
        expect(materialized.characters[0].chats.map((chat) => chat.id)).toEqual([
            'branch-id',
            'other-chat',
            'source-chat',
        ])
        expect(materialized.characters[0].chatPage).toBe(0)
    })

    it('refreshes a maximum-compatibility branch without forcing a scalable projection', async () => {
        const initial = makeDatabase()
        initial.plugins = [{
            name: 'Compatibility plugin',
            version: '2.1',
            enabled: true,
        }] as Database['plugins']
        const harness = await createHarness('captured-branch-maximum-compatibility', initial)
        const target = captureChatMessageTarget({ ...harness.context, absoluteIndex: 1 })!

        await expect(createCapturedConversationBranch({
            target,
            context: harness.context,
            runtime: harness.runtime,
            createFolderOnBranch: false,
            createId: idSequence('branch-id', 'marker-id'),
            createBranchName: () => 'Source (Branch)',
            navigateToBranch: (id) => harness.navigate(id),
        })).resolves.toBe(true)

        expect(harness.forcedScalableProjection.at(-1)).toBe(false)
        expect(harness.database.characters.map((character) => character.chaId)).toEqual([
            'char-a',
        ])
        expect(harness.database.characters[0].chats.map((chat) => chat.id)).toEqual([
            'branch-id',
            'source-chat',
            'other-chat',
        ])
    })

    it('creates no branch when the retained locator is already stale', async () => {
        const harness = await createHarness('captured-branch-stale')
        const target = captureChatMessageTarget({ ...harness.context, absoluteIndex: 1 })!
        target.session!.edit(target.locator!, message('changed', 'changed'))

        await expect(createCapturedConversationBranch({
            target,
            context: harness.context,
            runtime: harness.runtime,
            createFolderOnBranch: false,
            createId: idSequence('branch-id', 'marker-id'),
            createBranchName: () => 'Source (Branch)',
            navigateToBranch: (id) => harness.navigate(id),
        })).resolves.toBe(false)

        expect(await harness.store.readConversation('char-a', 'branch-id')).toBeNull()
    })

    it('creates no branch when navigation changes while the prefix is being paged', async () => {
        const messages = Array.from({ length: 260 }, (_, index) => message(
            `turn-${index}`,
            index % 2 === 0 ? 'duplicate' : undefined,
        ))
        const harness = await createHarness(
            'captured-branch-navigation',
            makeDatabase(messages),
        )
        const target = captureChatMessageTarget({ ...harness.context, absoluteIndex: 259 })!
        const originalAcquire = harness.store.acquireRevision.bind(harness.store)
        vi.spyOn(harness.store, 'acquireRevision').mockImplementationOnce(async (revision) => {
            const lease = await originalAcquire(revision)
            const originalRead = lease.readConversationWindow.bind(lease)
            vi.spyOn(lease, 'readConversationWindow').mockImplementationOnce(async (query) => {
                const page = await originalRead(query)
                harness.selectConversation('other-chat')
                return page
            })
            return lease
        })

        await expect(createCapturedConversationBranch({
            target,
            context: harness.context,
            runtime: harness.runtime,
            createFolderOnBranch: false,
            createId: idSequence('branch-id', 'marker-id'),
            createBranchName: () => 'Source (Branch)',
            navigateToBranch: (id) => harness.navigate(id),
            pageSize: 128,
        })).resolves.toBe(false)

        expect(await harness.store.readConversation('char-a', 'branch-id')).toBeNull()
    })

    it('propagates a revision conflict and leaves no partial branch', async () => {
        const harness = await createHarness('captured-branch-revision')
        const target = captureChatMessageTarget({ ...harness.context, absoluteIndex: 1 })!
        const originalAcquire = harness.store.acquireRevision.bind(harness.store)
        vi.spyOn(harness.store, 'acquireRevision').mockImplementationOnce(async (revision) => {
            const lease = await originalAcquire(revision)
            const originalRead = lease.readConversationWindow.bind(lease)
            vi.spyOn(lease, 'readConversationWindow').mockImplementationOnce(async (query) => {
                const page = await originalRead(query)
                await harness.store.commit({
                    expectedRevision: harness.imported.revision,
                    conversations: [{
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'source-chat',
                        start: 3,
                        deleteCount: 0,
                        messages: [message('concurrent', 'concurrent')],
                    }],
                })
                return page
            })
            return lease
        })

        await expect(createCapturedConversationBranch({
            target,
            context: harness.context,
            runtime: harness.runtime,
            createFolderOnBranch: false,
            createId: idSequence('branch-id', 'marker-id'),
            createBranchName: () => 'Source (Branch)',
            navigateToBranch: (id) => harness.navigate(id),
        })).rejects.toBeInstanceOf(RevisionConflictError)

        expect(await harness.store.readConversation('char-a', 'branch-id')).toBeNull()
    })
})
