import { IDBFactory, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import type { Chat, Database, Message } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import type {
    ConversationWindow,
    PersistentDataStore,
    PersistentRevisionLease,
} from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import { copyPinnedConversationBranch } from './conversationBranchJobs'

function message(data: string, chatId?: string): Message {
    return {
        role: 'user',
        data,
        ...(chatId === undefined ? {} : { chatId }),
    }
}

describe('pinned conversation branch jobs', () => {
    it('copies an inclusive prefix in bounded absolute pages and appends the marker once', async () => {
        const source = [
            message('zero', 'duplicate'),
            message('one'),
            message('two', 'duplicate'),
            message('three', 'tail'),
        ]
        const release = vi.fn(async () => undefined)
        const readConversationWindow = vi.fn(async ({
            characterId,
            conversationId,
            startIndex = 0,
            limit = 128,
        }): Promise<{ revision: number; value: ConversationWindow }> => ({
            revision: 7,
            value: {
                characterId,
                conversationId,
                messages: structuredClone(source.slice(startIndex, startIndex + limit)),
                startIndex,
                endIndex: Math.min(source.length, startIndex + limit),
                totalMessages: source.length,
                hasMoreBefore: startIndex > 0,
                hasMoreAfter: startIndex + limit < source.length,
            },
        }))
        const lease = {
            revision: 7,
            readConversationWindow,
            release,
        } as unknown as PersistentRevisionLease
        const commit = vi.fn(async () => ({ revision: 8 }))
        const store = {
            acquireRevision: vi.fn(async () => lease),
            commit,
        } as unknown as PersistentDataStore
        const branch = {
            id: 'branch-chat',
            name: 'Source (Branch)',
            note: 'detail survives',
            localLore: [{ key: 'lore', content: 'value' }],
            fmIndex: -1,
            unknownDetail: { keep: false },
        } as unknown as Omit<Chat, 'message'>
        const marker = message('{{specialcomment::branchedfrom::source-chat::Source::::}}', 'marker')

        await expect(copyPinnedConversationBranch(store, {
            characterId: 'char-a',
            sourceConversationId: 'source-chat',
            sourceRevision: 7,
            inclusiveEndIndex: 2,
            branch,
            branchMarker: marker,
            pageSize: 2,
        })).resolves.toEqual({
            sourceCharacterId: 'char-a',
            sourceConversationId: 'source-chat',
            sourceRevision: 7,
            sourceStartIndex: 0,
            sourceEndIndex: 3,
            sourceTotalMessages: 4,
            branchConversationId: 'branch-chat',
            branchRevision: 8,
        })

        expect(readConversationWindow.mock.calls.map(([query]) => query)).toEqual([
            {
                characterId: 'char-a',
                conversationId: 'source-chat',
                startIndex: 0,
                limit: 2,
            },
            {
                characterId: 'char-a',
                conversationId: 'source-chat',
                startIndex: 2,
                limit: 1,
            },
        ])
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            conversations: [{
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'branch-chat',
                start: 0,
                deleteCount: 0,
                messages: [...source.slice(0, 3), marker],
                conversation: branch,
                configuredIndex: 0,
            }],
        })
        expect(release).toHaveBeenCalledOnce()
    })

    it('commits source folder metadata and the new branch atomically after final validation', async () => {
        const source = [message('zero', 'zero')]
        const release = vi.fn(async () => undefined)
        const beforeCommit = vi.fn(async () => undefined)
        const lease = {
            revision: 3,
            readConversationWindow: vi.fn(async () => ({
                revision: 3,
                value: {
                    characterId: 'char-a',
                    conversationId: 'source-chat',
                    messages: source,
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: 1,
                    hasMoreBefore: false,
                    hasMoreAfter: false,
                },
            })),
            release,
        } as unknown as PersistentRevisionLease
        const commit = vi.fn(async () => ({ revision: 4 }))
        const store = {
            acquireRevision: vi.fn(async () => lease),
            commit,
        } as unknown as PersistentDataStore
        const character = {
            type: 'character',
            chaId: 'char-a',
            name: 'Character',
            chatFolders: [{ id: 'folder-id', name: 'Branches of Source', folded: false }],
        } as any
        const sourceConversation = {
            id: 'source-chat',
            name: 'Source',
            note: '',
            localLore: [],
            folderId: 'folder-id',
        }
        const branch = {
            ...sourceConversation,
            id: 'branch-chat',
            name: 'Source (Branch)',
        }

        await copyPinnedConversationBranch(store, {
            characterId: 'char-a',
            sourceConversationId: 'source-chat',
            sourceRevision: 3,
            inclusiveEndIndex: 0,
            branch,
            branchMarker: message('marker', 'marker'),
            character,
            sourceConversation,
            beforeCommit,
        })

        expect(beforeCommit).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledWith({
            expectedRevision: 3,
            character,
            conversations: [{
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'source-chat',
                start: 0,
                deleteCount: 0,
                messages: [],
                conversation: sourceConversation,
            }, {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'branch-chat',
                start: 0,
                deleteCount: 0,
                messages: [...source, message('marker', 'marker')],
                conversation: branch,
                configuredIndex: 0,
            }],
        })
        expect(release).toHaveBeenCalledOnce()
    })

    it('matches the canonical legacy branch for a 10,000-turn source', async () => {
        const messages = Array.from({ length: 10_000 }, (_, index) => message(
            `turn-${index.toString().padStart(5, '0')}`,
            index % 3 === 0 ? 'duplicate' : index % 3 === 1 ? undefined : `id-${index}`,
        ))
        const source: Chat = {
            id: 'source-chat',
            name: 'Source',
            note: 'source note',
            localLore: [],
            fmIndex: -1,
            message: messages,
        }
        const database = {
            botPresets: [],
            pluginCustomStorage: {},
            characters: [{
                type: 'character',
                chaId: 'char-a',
                name: 'Character',
                firstMessage: 'Greeting',
                alternateGreetings: [],
                chats: [source],
                chatPage: 0,
            }],
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore(
            'branch-copy-large',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(database))
        const originalAcquire = store.acquireRevision.bind(store)
        let readPages: ReturnType<typeof vi.fn> | undefined
        vi.spyOn(store, 'acquireRevision').mockImplementation(async (revision) => {
            const lease = await originalAcquire(revision)
            readPages = vi.spyOn(lease, 'readConversationWindow')
            return lease
        })
        const { message: _messages, ...sourceDetail } = source
        const branch = {
            ...structuredClone(sourceDetail),
            id: 'branch-chat',
            name: 'Source (Branch)',
            note: 'branch detail',
            unknownDetail: { keep: 0 },
        } as Omit<Chat, 'message'>
        const marker = {
            role: 'char',
            data: '{{specialcomment::branchedfrom::source-chat::Source::duplicate::}}',
            isComment: true,
            disabled: true,
            chatId: 'marker',
        } as Message

        let occurrencePuts = 0
        const originalPut = IDBObjectStore.prototype.put
        const putSpy = vi.spyOn(IDBObjectStore.prototype, 'put').mockImplementation(function (
            this: IDBObjectStore,
            ...args: Parameters<IDBObjectStore['put']>
        ) {
            if (this.name === 'messageOccurrences') occurrencePuts++
            return originalPut.apply(this, args)
        })
        let result
        try {
            result = await copyPinnedConversationBranch(store, {
                characterId: 'char-a',
                sourceConversationId: 'source-chat',
                sourceRevision: imported.revision,
                inclusiveEndIndex: messages.length - 1,
                branch,
                branchMarker: marker,
                pageSize: 128,
            })
        } finally {
            putSpy.mockRestore()
        }

        expect(result).toMatchObject({
            sourceStartIndex: 0,
            sourceEndIndex: 10_000,
            sourceTotalMessages: 10_000,
            branchRevision: imported.revision + 1,
        })
        expect(readPages).toHaveBeenCalledTimes(Math.ceil(10_000 / 128))
        expect(readPages!.mock.calls.every(([query]) => query.limit <= 128)).toBe(true)
        expect(readPages!.mock.calls[0][0]).toMatchObject({ startIndex: 0, limit: 128 })
        expect(readPages!.mock.calls[1][0]).toMatchObject({ startIndex: 128, limit: 128 })
        expect(occurrencePuts).toBe(
            Math.ceil(messages.length / 128) + Math.ceil((messages.length + 1) / 128),
        )

        const expected = structuredClone(database)
        expected.characters[0].chats.unshift({
            ...structuredClone(branch),
            message: [...structuredClone(messages), structuredClone(marker)],
        })
        expect(await store.materializeDatabase()).toEqual(expected)
    })

    it('releases the lease and creates no branch after cancellation between pages', async () => {
        const controller = new AbortController()
        const release = vi.fn(async () => undefined)
        const readConversationWindow = vi.fn(async () => {
            controller.abort(new Error('cancel branch copy'))
            return {
                revision: 4,
                value: {
                    characterId: 'char-a',
                    conversationId: 'source-chat',
                    messages: [message('zero')],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: 2,
                    hasMoreBefore: false,
                    hasMoreAfter: true,
                },
            }
        })
        const lease = {
            revision: 4,
            readConversationWindow,
            release,
        } as unknown as PersistentRevisionLease
        const commit = vi.fn()
        const store = {
            acquireRevision: vi.fn(async () => lease),
            commit,
        } as unknown as PersistentDataStore

        await expect(copyPinnedConversationBranch(store, {
            characterId: 'char-a',
            sourceConversationId: 'source-chat',
            sourceRevision: 4,
            inclusiveEndIndex: 1,
            branch: { id: 'branch-chat', name: 'Branch', note: '', localLore: [] },
            branchMarker: message('marker'),
            pageSize: 1,
            signal: controller.signal,
        })).rejects.toThrow('cancel branch copy')

        expect(commit).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledOnce()
    })

    it('releases the lease and leaves commit untouched on source failure or revision conflict', async () => {
        const releaseAfterReadFailure = vi.fn(async () => undefined)
        const readFailureLease = {
            revision: 5,
            readConversationWindow: vi.fn(async () => {
                throw new Error('source read failed')
            }),
            release: releaseAfterReadFailure,
        } as unknown as PersistentRevisionLease
        const noCommit = vi.fn()
        const readFailureStore = {
            acquireRevision: vi.fn(async () => readFailureLease),
            commit: noCommit,
        } as unknown as PersistentDataStore
        const request = {
            characterId: 'char-a',
            sourceConversationId: 'source-chat',
            sourceRevision: 5,
            inclusiveEndIndex: 0,
            branch: { id: 'branch-chat', name: 'Branch', note: '', localLore: [] },
            branchMarker: message('marker'),
        }

        await expect(copyPinnedConversationBranch(readFailureStore, request)).rejects.toThrow(
            'source read failed',
        )
        expect(noCommit).not.toHaveBeenCalled()
        expect(releaseAfterReadFailure).toHaveBeenCalledOnce()

        const releaseAfterConflict = vi.fn(async () => undefined)
        const conflictLease = {
            revision: 5,
            readConversationWindow: vi.fn(async () => ({
                revision: 5,
                value: {
                    characterId: 'char-a',
                    conversationId: 'source-chat',
                    messages: [message('zero')],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: 1,
                    hasMoreBefore: false,
                    hasMoreAfter: false,
                },
            })),
            release: releaseAfterConflict,
        } as unknown as PersistentRevisionLease
        const conflictCommit = vi.fn(async () => {
            throw new RevisionConflictError(5, 6)
        })
        const conflictStore = {
            acquireRevision: vi.fn(async () => conflictLease),
            commit: conflictCommit,
        } as unknown as PersistentDataStore

        await expect(copyPinnedConversationBranch(conflictStore, request)).rejects.toBeInstanceOf(
            RevisionConflictError,
        )
        expect(conflictCommit).toHaveBeenCalledOnce()
        expect(releaseAfterConflict).toHaveBeenCalledOnce()
    })

    it('creates no branch when the source mutates after the revision is pinned', async () => {
        const database = {
            botPresets: [],
            pluginCustomStorage: {},
            characters: [{
                type: 'character',
                chaId: 'char-concurrent',
                name: 'Concurrent',
                firstMessage: 'Greeting',
                alternateGreetings: [],
                chats: [{
                    id: 'source-chat',
                    name: 'Source',
                    note: '',
                    localLore: [],
                    message: [message('zero'), message('one')],
                }],
                chatPage: 0,
            }],
        } as unknown as Database
        const store = new IndexedDbPersistentDataStore(
            'branch-copy-concurrent',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const originalAcquire = store.acquireRevision.bind(store)
        vi.spyOn(store, 'acquireRevision').mockImplementation(async (revision) => {
            const lease = await originalAcquire(revision)
            const originalRead = lease.readConversationWindow.bind(lease)
            vi.spyOn(lease, 'readConversationWindow').mockImplementationOnce(async (query) => {
                const pinnedPage = await originalRead(query)
                await store.commit({
                    expectedRevision: imported.revision,
                    conversations: [{
                        type: 'replace-range',
                        characterId: 'char-concurrent',
                        conversationId: 'source-chat',
                        start: 2,
                        deleteCount: 0,
                        messages: [message('live append')],
                    }],
                })
                return pinnedPage
            })
            return lease
        })

        await expect(copyPinnedConversationBranch(store, {
            characterId: 'char-concurrent',
            sourceConversationId: 'source-chat',
            sourceRevision: imported.revision,
            inclusiveEndIndex: 1,
            branch: { id: 'branch-chat', name: 'Branch', note: '', localLore: [] },
            branchMarker: message('marker'),
        })).rejects.toBeInstanceOf(RevisionConflictError)

        expect(await store.readConversation('char-concurrent', 'branch-chat')).toBeNull()
        expect((await store.readConversation(
            'char-concurrent',
            'source-chat',
        ))?.value.message.map((entry) => entry.data)).toEqual(['zero', 'one', 'live append'])
    })
})
