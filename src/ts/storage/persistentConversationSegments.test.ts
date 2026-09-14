import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'

import type { Message } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import {
    PinnedPersistentConversationClosedError,
    commitStrictConversationReplaceRange,
    openPinnedPersistentConversation,
} from './persistentConversationSegments'
import { fixtureDatabase } from './tests/persistentDataFixtures'

let databaseSequence = 0

function message(index: number, data = `replacement-${index}`): Message {
    return {
        role: index % 2 === 0 ? 'user' : 'char',
        data,
        chatId: `replacement-${index}`,
    }
}

async function createStore() {
    const indexedDB = new IDBFactory()
    const store = new IndexedDbPersistentDataStore(
        `persistent-conversation-segments-${databaseSequence++}`,
        indexedDB,
        IDBKeyRange,
    )
    await store.open()
    const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
    return { imported, store }
}

describe('PinnedPersistentConversation', () => {
    it('reads and iterates absolute ranges from one pinned revision without full materialization', async () => {
        const { imported, store } = await createStore()
        const fullRead = vi.spyOn(store, 'readConversation')
        const materialize = vi.spyOn(store, 'materializeDatabase')
        const reader = await openPinnedPersistentConversation({
            store,
            characterId: 'char-a',
            conversationId: 'conv-long',
            revision: imported.revision,
        })

        const root = (await store.readRoot()).value
        await store.commit({
            expectedRevision: imported.revision,
            root: { ...root, username: 'new live revision' },
        })

        expect(reader.totalMessages).toBe(130)
        expect((await reader.readRange(126, 4)).messages.map((item) => item.data)).toEqual([
            'message-126',
            'message-127',
            'message-128',
            'message-129',
        ])

        const visited: Array<{ absoluteIndex: number; data: string }> = []
        for await (const entry of reader.iterateBackward({ pageSize: 47 })) {
            visited.push({ absoluteIndex: entry.absoluteIndex, data: entry.message.data })
        }
        expect(visited).toHaveLength(130)
        expect(visited.slice(0, 3)).toEqual([
            { absoluteIndex: 129, data: 'message-129' },
            { absoluteIndex: 128, data: 'message-128' },
            { absoluteIndex: 127, data: 'message-127' },
        ])
        expect(visited.at(-1)).toEqual({ absoluteIndex: 0, data: 'message-000' })
        expect(fullRead).not.toHaveBeenCalled()
        expect(materialize).not.toHaveBeenCalled()

        await reader.close()
    })

    it('clears a temporary full snapshot on disposal and closes its lease idempotently', async () => {
        const { imported, store } = await createStore()
        const reader = await openPinnedPersistentConversation({
            store,
            characterId: 'char-a',
            conversationId: 'conv-long',
            revision: imported.revision,
        })
        const snapshot = await reader.materializeCompatibilitySnapshot(31)
        const retainedMessages = snapshot.messages

        expect(retainedMessages).toHaveLength(130)
        expect(snapshot.residentMessageCount).toBe(130)
        snapshot.dispose()
        snapshot.dispose()

        expect(retainedMessages).toHaveLength(0)
        expect(snapshot.residentMessageCount).toBe(0)

        await reader.close()
        await reader.close()
        await expect(reader.readRange(0, 1)).rejects.toBeInstanceOf(
            PinnedPersistentConversationClosedError,
        )
    })

    it('rejects a stale or out-of-bounds replace range before the clamping store primitive', async () => {
        const { imported, store } = await createStore()
        const commit = vi.spyOn(store, 'commit')

        await expect(commitStrictConversationReplaceRange({
            store,
            characterId: 'char-a',
            conversationId: 'conv-long',
            expectedRevision: imported.revision,
            start: 131,
            deleteCount: 0,
            messages: [message(131)],
        })).rejects.toThrow('start exceeds')
        expect(commit).not.toHaveBeenCalled()

        await expect(commitStrictConversationReplaceRange({
            store,
            characterId: 'char-a',
            conversationId: 'conv-long',
            expectedRevision: imported.revision,
            start: 129,
            deleteCount: 2,
            messages: [],
        })).rejects.toThrow('deleteCount exceeds')
        expect(commit).not.toHaveBeenCalled()

        const committed = await commitStrictConversationReplaceRange({
            store,
            characterId: 'char-a',
            conversationId: 'conv-long',
            expectedRevision: imported.revision,
            start: 129,
            deleteCount: 1,
            messages: [message(129)],
        })

        expect(committed.revision).toBe(imported.revision + 1)
        expect(commit).toHaveBeenCalledTimes(1)
        expect((await store.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            startIndex: 129,
            limit: 1,
        }))?.value.messages).toEqual([message(129)])
    })

    it('preserves an open failure when releasing its revision fails', async () => {
        const { imported, store } = await createStore()
        const acquireRevision = store.acquireRevision.bind(store)
        const releaseError = new Error('release failed')
        let cleanup: (() => Promise<void>) | undefined
        vi.spyOn(store, 'acquireRevision').mockImplementationOnce(async (revision) => {
            const lease = await acquireRevision(revision)
            cleanup = lease.release.bind(lease)
            lease.release = vi.fn(async () => { throw releaseError })
            return lease
        })
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

        await expect(openPinnedPersistentConversation({
            store,
            characterId: 'char-a',
            conversationId: 'missing',
            revision: imported.revision,
        })).rejects.toThrow('Conversation missing was not found for char-a')
        expect(consoleError).toHaveBeenCalledWith(
            'Persistent conversation revision release failed after open failed',
            releaseError,
        )

        await cleanup?.()
        consoleError.mockRestore()
    })

    it('preserves range validation when closing its revision fails', async () => {
        const { imported, store } = await createStore()
        const acquireRevision = store.acquireRevision.bind(store)
        const releaseError = new Error('release failed')
        let cleanup: (() => Promise<void>) | undefined
        vi.spyOn(store, 'acquireRevision').mockImplementationOnce(async (revision) => {
            const lease = await acquireRevision(revision)
            cleanup = lease.release.bind(lease)
            lease.release = vi.fn(async () => { throw releaseError })
            return lease
        })
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

        await expect(commitStrictConversationReplaceRange({
            store,
            characterId: 'char-a',
            conversationId: 'conv-long',
            expectedRevision: imported.revision,
            start: 131,
            deleteCount: 0,
            messages: [],
        })).rejects.toThrow('start exceeds')
        expect(consoleError).toHaveBeenCalledWith(
            'Persistent conversation revision release failed after range validation failed',
            releaseError,
        )

        await cleanup?.()
        consoleError.mockRestore()
    })
})
