import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { IndexedDbPersistentDataStore } from './storage/indexedDbPersistentDataStore'
import { fixtureDatabase } from './storage/tests/persistentDataFixtures'
import { EditableViewportFixture } from './conversationViewportSource.testSupport'
import { describe, expect, it, vi } from 'vitest'
import {
    PersistentConversationViewportSource,
    SynchronousSessionConversationViewportSource,
    type ConversationViewportKey,
    type ConversationViewportSource,
} from './conversationViewportSource'
import { ActiveConversationSession } from './storage/activeConversationSession'
import { createConversationOperationContext } from './process/conversationOperationContext'
import { CbsHistoryDependencies, hasCbsHistoryChanges } from './cbsHistoryAccess'
import type { Chat, Message, character } from './storage/database.svelte'
import type {
    ConversationWindowQuery,
    Versioned,
    ConversationWindow,
} from './storage/persistentDataStore'

function message(chatId: string | undefined, data: string): Message {
    return { role: 'char', data, ...(chatId === undefined ? {} : { chatId }) }
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function persistentWindow(
    startIndex: number,
    messages: Message[],
    totalMessages: number,
    overrides: Partial<ConversationWindow> = {},
): ConversationWindow {
    const endIndex = startIndex + messages.length
    return {
        characterId: 'character-a',
        conversationId: 'conversation-a',
        messages,
        startIndex,
        endIndex,
        totalMessages,
        hasMoreBefore: startIndex > 0,
        hasMoreAfter: endIndex < totalMessages,
        ...overrides,
    }
}

function harness(messages: Message[]) {
    const conversation: Chat = {
        id: 'conversation-a',
        message: messages,
        note: '',
        name: '',
        localLore: [],
    }
    const owner = {
        type: 'character',
        chaId: 'character-a',
        name: 'Character',
        chats: [conversation],
        chatPage: 0,
    } as unknown as character
    const session = new ActiveConversationSession({
        characterId: owner.chaId,
        conversationId: conversation.id,
        conversation,
        storeRevision: 1,
        measureMessage: () => 1,
    })
    const source = new SynchronousSessionConversationViewportSource({
        session,
        captureCurrent: () => ({ character: owner, conversation }),
    })
    return { conversation, owner, session, source }
}

describe('SynchronousSessionConversationViewportSource', () => {
    it('finds the latest ordinary row of each role and invalidates on role edits', async () => {
        const { session, source } = harness([
            { role: 'user', data: 'u' },
            { role: 'char', data: 'c' },
            { role: 'user', data: 'comment', isComment: true },
            { role: 'user', data: 'disabled', disabled: true },
            { role: 'char', data: 'c2' },
        ])
        const signal = new AbortController().signal
        await expect(source.findLatestMessagePair(signal)).resolves.toEqual([0, 4])
        session.edit(session.locate(4), { role: 'user', data: 'pending reply' })
        await expect(source.findLatestMessagePair(signal)).resolves.toEqual([1, 4])
        source.dispose()
    })

    it('keeps rows readable across display variable commits and invalidates the next external edit', async () => {
        const { conversation, session, source } = harness([
            message('first', 'first'),
            message('second', 'second'),
        ])
        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        const before = source.snapshot()
        const listener = vi.fn()
        const mutations = vi.fn()
        source.subscribe(listener)
        session.subscribe(mutations)
        const pendingRange = source.ensureRange({
            startIndex: 1,
            limit: 1,
            reason: 'viewport',
        })
        for (let index = 0; index < 3; index++) {
            const operation = createConversationOperationContext(
                session,
                conversation,
            )
            operation.chat.scriptstate = { $scratch: String(index) }
            operation.commit(session, { origin: 'display' })
        }
        expect(session.version).toBe(3)
        expect(mutations).toHaveBeenCalledTimes(3)
        expect(conversation.scriptstate).toEqual({ $scratch: '2' })
        expect(listener).not.toHaveBeenCalled()
        expect(source.snapshot().version).toBe(before.version)
        expect(source.snapshot().rowAt(0)).toBe(before.rowAt(0))
        await pendingRange
        expect(source.snapshot().rowAt(1)?.message.data).toBe('second')
        // New loads and click targets must use the advanced session authority.
        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        const target = source.captureMessageTarget(before.keyAt(0)!)!
        expect(target.kind).toBe('session')
        session.edit(session.locate(0), message('first', 'edited'))
        expect(source.snapshot().version).toBe(4)
        expect(source.snapshot().keyAt(0)).toBe(before.keyAt(0))
        expect(source.snapshot().keyAt(1)).toBe(before.keyAt(1))
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        expect(source.snapshot().rowAt(0)?.message.data).toBe('edited')
    })

    it('invalidates rows for variables an operation changed and keeps them for variable commits', async () => {
        const { conversation, session, source } = harness([
            message('first', 'first'),
        ])
        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        const listener = vi.fn()
        source.subscribe(listener)
        const operation = createConversationOperationContext(session, conversation)
        operation.chat.scriptstate = { $button: 'clicked' }
        operation.commit(session)
        expect(listener).toHaveBeenCalledOnce()
        expect(source.snapshot().version).toBe(1)
        expect(source.snapshot().rowAt(0)).toBeUndefined()

        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        const loaded = source.snapshot()
        listener.mockClear()
        session.updateChatVariables(new Map([['button', 'again']]))
        expect(conversation.scriptstate).toEqual({ $button: 'again' })
        expect(listener).not.toHaveBeenCalled()
        expect(source.snapshot().version).toBe(loaded.version)
        expect(source.snapshot().rowAt(0)).toBe(loaded.rowAt(0))
    })

    it('exposes absolute totals and only publishes rows from ensured ranges', async () => {
        const messages = Array.from({ length: 10_000 }, (_, index) =>
            message(`message-${index}`, `data-${index}`),
        )
        const { session, source } = harness(messages)
        const readRange = vi.spyOn(session, 'readRange')

        const before = source.snapshot()
        expect(before.totalMessages).toBe(10_000)
        expect(before.rowAt(9_500)).toBeUndefined()

        await source.ensureRange({ startIndex: 9_500, limit: 32, reason: 'viewport' })

        const snapshot = source.snapshot()
        expect(readRange).toHaveBeenCalledOnce()
        expect(readRange).toHaveBeenCalledWith(9_500, 32)
        expect(snapshot.rowAt(9_499)).toBeUndefined()
        expect(snapshot.rowAt(9_500)).toMatchObject({
            absoluteIndex: 9_500,
            sourceVersion: 0,
            message: { chatId: 'message-9500', data: 'data-9500' },
        })
        expect(snapshot.rowAt(9_531)?.absoluteIndex).toBe(9_531)
        expect(snapshot.rowAt(9_532)).toBeUndefined()
    })

    it('gives duplicate IDs, missing IDs, and repeated objects distinct stable keys', () => {
        const repeated = message(undefined, 'repeated')
        const { source } = harness([
            message('duplicate', 'first'),
            message('duplicate', 'second'),
            message(undefined, 'missing'),
            repeated,
            repeated,
        ])
        const snapshot = source.snapshot()
        const keys = Array.from(
            { length: snapshot.totalMessages },
            (_, index) => snapshot.keyAt(index),
        )

        expect(keys.every(Boolean)).toBe(true)
        expect(new Set(keys).size).toBe(keys.length)
        for (const [index, key] of keys.entries()) {
            expect(snapshot.indexOfKey(key!)).toBe(index)
        }
    })

    it('keeps an edited row key and shifts untouched row keys across insert and delete', async () => {
        const { session, source } = harness([
            message(undefined, 'zero'),
            message(undefined, 'one'),
            message(undefined, 'two'),
        ])
        const initial = source.snapshot()
        const editedKey = initial.keyAt(1)!
        const shiftedKey = initial.keyAt(2)!

        session.edit(session.locate(1), message(undefined, 'edited one'))
        expect(source.snapshot().keyAt(1)).toBe(editedKey)

        session.replaceRange(session.positionAt(0), 0, [message(undefined, 'inserted')])
        expect(source.snapshot().keyAt(2)).toBe(editedKey)
        expect(source.snapshot().keyAt(3)).toBe(shiftedKey)

        const removedKey = source.snapshot().keyAt(0)!
        session.delete(session.locate(0))
        const afterDelete = source.snapshot()
        expect(afterDelete.indexOfKey(removedKey)).toBe(-1)
        expect(afterDelete.keyAt(1)).toBe(editedKey)

        await source.ensureRange({ startIndex: 1, limit: 1, reason: 'jump' })
        expect(source.snapshot().rowAt(1)?.key).toBe(editedKey)
    })

    it('retires positional proof for a same-length legacy splice while keeping it for other rows', () => {
        const a = message(undefined, 'first'), b = message(undefined, 'second'), c = message(undefined, 'third')
        const { session, source } = harness([a, b, c])
        const initial = source.snapshot()
        session.edit(session.locate(0), { ...a, data: 'edited' })
        session.append(message(undefined, 'fourth'))
        expect(source.snapshot().positionKeyAt!(0)).toBe(initial.positionKeyAt!(0))
        const second = initial.positionKeyAt!(1)
        session.replaceRange(session.positionAt(1), 2, [c, b])
        expect(source.snapshot().positionKeyAt!(0)).toBe(initial.positionKeyAt!(0))
        expect(source.snapshot().positionKeyAt!(1)).not.toBe(second)
        expect(initial.positionKeyAt!(1)).toBe(second)
    })

    it('follows unique render keys and expires old positional evidence after complete store adoption', async () => {
        const { session, source, conversation } = harness([message('a', 'first'), message('b', 'second'), message(undefined, 'legacy')])
        const original = structuredClone(conversation)
        const reordered = structuredClone(conversation)
        reordered.message = [reordered.message[1], reordered.message[0], reordered.message[2]]
        const before = source.snapshot()
        session.adoptGenerationHead(original, reordered, 2, -1, false)
        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        const after = source.snapshot()
        expect(after.indexOfKey(before.keyAt(0)!)).toBe(1)
        expect(after.rowAt(0)?.key).toBe(before.keyAt(1))
        expect(after.rowAt(0)?.message.data).toBe('second')
        expect(after.positionKeyAt!(2)).not.toBe(before.positionKeyAt!(2))
        source.dispose()
    })

    it('preserves untouched shifted keys across ordered commands in one transaction', () => {
        const { session, source } = harness([
            message(undefined, 'zero'),
            message(undefined, 'one'),
            message(undefined, 'two'),
        ])
        const initial = source.snapshot()
        const oneKey = initial.keyAt(1)
        const twoKey = initial.keyAt(2)

        session.transaction((transaction) => {
            transaction.delete(transaction.locate(0))
            transaction.append(message(undefined, 'appended'))
        })

        const current = source.snapshot()
        expect(current.keyAt(0)).toBe(oneKey)
        expect(current.keyAt(1)).toBe(twoKey)
        expect(current.indexOfKey(initial.keyAt(0)!)).toBe(-1)
    })

    it('notifies subscribers after committed mutations and range publication', async () => {
        const { session, source } = harness([message('first', 'first')])
        const listener = vi.fn()
        const unsubscribe = source.subscribe(listener)

        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        expect(listener).toHaveBeenCalledTimes(1)

        session.append(message('second', 'second'))
        expect(listener).toHaveBeenCalledTimes(2)
        expect(source.snapshot()).toMatchObject({ version: 1, totalMessages: 2 })

        unsubscribe()
        session.append(message('third', 'third'))
        expect(listener).toHaveBeenCalledTimes(2)
    })

    it('publishes a terminal empty snapshot when its session is invalidated', () => {
        const { session, source } = harness([
            message('first', 'first'),
            message('second', 'second'),
        ])
        const totals: number[] = []
        source.subscribe(() => totals.push(source.snapshot().totalMessages))
        const contract: ConversationViewportSource = source

        session.invalidate()

        expect(totals).toEqual([0])
        expect(source.snapshot().keyAt(0)).toBeUndefined()
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(() => contract.dispose()).not.toThrow()
    })

    it('updates ordinary mutation keys without rescanning untouched message identities', () => {
        let untouchedIdentityReads = 0
        const messages = Array.from({ length: 1_000 }, (_, index) => {
            const current = message(`message-${index}`, `data-${index}`)
            if (index === 0) return current
            const chatId = current.chatId
            Object.defineProperty(current, 'chatId', {
                configurable: true,
                enumerable: true,
                get() {
                    untouchedIdentityReads += 1
                    return chatId
                },
            })
            return current
        })
        const { session, source } = harness(messages)
        const retainedKey = source.snapshot().keyAt(999)
        untouchedIdentityReads = 0

        session.edit(session.locate(0), message('message-0', 'edited'))

        expect(untouchedIdentityReads).toBe(0)
        expect(source.snapshot().keyAt(999)).toBe(retainedKey)
    })

    it('discards stale and aborted range results instead of publishing them', async () => {
        const { session, source } = harness([
            message('first', 'first'),
            message('second', 'second'),
        ])
        const listener = vi.fn()
        source.subscribe(listener)

        const stale = source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        session.edit(session.locate(0), message('first', 'changed'))
        await stale
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(listener).toHaveBeenCalledTimes(1)

        const controller = new AbortController()
        const aborted = source.ensureRange({
            startIndex: 0,
            limit: 1,
            reason: 'jump',
            signal: controller.signal,
        })
        controller.abort()
        await aborted
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(listener).toHaveBeenCalledTimes(1)
    })

    it('owns exact idempotent session pins and releases all remaining pins on dispose', () => {
        const { session, source } = harness([
            message('zero', 'zero'),
            message('one', 'one'),
            message('two', 'two'),
        ])
        const viewport = source.acquireRangePin(0, 2, 'viewport')
        const editor = source.acquireRangePin(1, 2, 'editor')
        const media = source.acquireRangePin(2, 3, 'playing-media')
        const streaming = source.acquireRangePin(2, 3, 'streaming')

        expect(session.pinCount('viewport')).toBe(1)
        expect(session.pinCount('editor')).toBe(1)
        expect(session.pinCount('playing-media')).toBe(1)
        expect(session.pinCount('streaming')).toBe(1)

        editor.release()
        editor.release()
        expect(session.pinCount('editor')).toBe(0)

        source.dispose()
        expect(session.pinCount('viewport')).toBe(0)
        expect(session.pinCount('playing-media')).toBe(0)
        expect(session.pinCount('streaming')).toBe(0)
        expect(() => viewport.release()).not.toThrow()
        expect(() => media.release()).not.toThrow()
        expect(() => streaming.release()).not.toThrow()
    })

    it('captures a current absolute session target from a source-owned row key', async () => {
        const { session, source } = harness([
            message('first', 'first'),
            message('second', 'second'),
        ])
        await source.ensureRange({ startIndex: 1, limit: 1, reason: 'viewport' })
        const key = source.snapshot().keyAt(1) as ConversationViewportKey
        const target = source.captureMessageTarget(key)

        expect(target).toMatchObject({
            kind: 'session',
            absoluteIndex: 1,
            message: { chatId: 'second', data: 'second' },
            session,
        })

        session.replaceRange(session.positionAt(0), 0, [message('inserted', 'inserted')])
        expect(source.captureMessageTarget(key)).toMatchObject({
            absoluteIndex: 2,
            message: { chatId: 'second', data: 'second' },
        })

        session.delete(session.locate(2))
        expect(source.captureMessageTarget(key)).toBeNull()
    })
})

describe('PersistentConversationViewportSource', () => {
    it.each([false, true])('finds a distant latest user without hydrating the history (metadata=%s)', async (metadata) => {
        const messages: Message[] = Array.from({ length: 300 }, (_, i) => ({ role: 'char', data: `synthetic-${i}` }))
        messages[5].role = 'user'
        messages[290] = { role: 'user', data: 'comment', isComment: true }
        messages[291] = { role: 'user', data: 'disabled', disabled: true }
        const readConversationWindow = vi.fn(async ({ startIndex, limit }: ConversationWindowQuery) => ({
            revision: 1,
            value: persistentWindow(startIndex, messages.slice(startIndex, startIndex + limit), messages.length),
        }))
        const readMetadata = vi.fn(async ({ startIndex, limit }: ConversationWindowQuery) => ({
            revision: 1,
            value: {
                ...persistentWindow(startIndex, [], messages.length),
                endIndex: Math.min(messages.length, startIndex + limit),
                messages: messages.slice(startIndex, startIndex + limit).map(message => ({ role: message.role, disabled: message.disabled, parserInert: true })),
            },
        }))
        const source = new PersistentConversationViewportSource({
            reader: { readConversationWindow, ...(metadata ? { readConversationMessageMetadataWindow: readMetadata } : {}) },
            characterId: 'character-a', conversationId: 'conversation-a', revision: 1, totalMessages: 300, rowBudget: 64,
        })
        const signal = new AbortController().signal
        const lookup = source.findLatestMessagePair(signal)
        await expect(lookup).resolves.toEqual([5, 299])
        expect(source.findLatestMessagePair(signal)).toBe(lookup)
        expect(source.snapshot().rowAt(5)).toBeUndefined()
        expect(readConversationWindow).toHaveBeenCalledTimes(metadata ? 3 : 5)
        expect(readConversationWindow.mock.calls.every(([query]) => query.limit <= 64)).toBe(true)
        if (metadata) expect(readMetadata).toHaveBeenCalledTimes(5)
        source.dispose()
    })

    it('does not publish an old pair after an optimistic role change', async () => {
        const pending = deferred<Versioned<ConversationWindow>>()
        const source = new EditableViewportFixture({
            reader: { readConversationWindow: () => pending.promise },
            characterId: 'character-a', conversationId: 'conversation-a', revision: 1, totalMessages: 2, rowBudget: 64,
        })
        const lookup = source.findLatestMessagePair(new AbortController().signal)
        const before = source.snapshot().historyVersion
        const rollback = source.applyOptimisticRange(1, 1, [{ role: 'user', data: 'changed' }])
        expect(source.snapshot().historyVersion).not.toBe(before)
        pending.resolve({ revision: 1, value: persistentWindow(0, [{ role: 'user', data: 'old-u' }, { role: 'char', data: 'old-c' }], 2) })
        await expect(lookup).rejects.toMatchObject({ name: 'AbortError' })
        source.applyOptimisticRange(0, 1, [{ role: 'user', data: 'later edit' }])
        source.applyOptimisticRange(2, 0, [{ role: 'char', data: 'later append' }])
        const changed = source.snapshot().historyVersion
        const earlierRow = new CbsHistoryDependencies()
        earlierRow.range(0, 1)
        const count = new CbsHistoryDependencies()
        count.count = true
        rollback()
        expect(source.snapshot().historyVersion).not.toBe(changed)
        expect(source.snapshot().totalMessages).toBe(2)
        expect(hasCbsHistoryChanges(source.snapshot(), changed, earlierRow)).toBe(true)
        expect(hasCbsHistoryChanges(source.snapshot(), changed, count)).toBe(true)
        source.dispose()
    })

    it('publishes optimistic generated tail rows while persistent reads remain on the pinned base', async () => {
        const readConversationWindow = vi.fn(async ({ startIndex = 0, limit = 3 }: ConversationWindowQuery) => ({
            revision: 7,
            value: persistentWindow(startIndex, [
                message('zero', 'stored-zero'),
                message('one', 'stored-one'),
            ].slice(startIndex, startIndex + limit), 2),
        }))
        const source = new EditableViewportFixture({
            reader: { readConversationWindow },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 2,
            rowBudget: 8,
        })
        const listener = vi.fn()
        source.subscribe(listener)

        const rollback = source.applyOptimisticRange(2, 0, [
            message('generated', 'streamed'),
        ])
        expect(source.snapshot()).toMatchObject({ totalMessages: 3 })
        expect(source.snapshot().rowAt(2)?.message).toMatchObject({ data: 'streamed' })

        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        expect(source.snapshot().rowAt(0)?.message).toMatchObject({ data: 'stored-zero' })
        expect(source.snapshot().rowAt(2)?.message).toMatchObject({ data: 'streamed' })
        expect(readConversationWindow).toHaveBeenCalledWith(expect.objectContaining({
            startIndex: 0,
            limit: 2,
        }))

        rollback()
        expect(source.snapshot()).toMatchObject({ totalMessages: 2 })
        expect(source.snapshot().rowAt(2)).toBeUndefined()
        expect(listener).toHaveBeenCalledTimes(3)
    })

    it('evicts a large optimistic range in one pass over the cached rows', () => {
        const source = new EditableViewportFixture({
            reader: { readConversationWindow: vi.fn() },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 0,
            rowBudget: 8,
        })
        const isPinned = vi.spyOn(source as unknown as { isPinned(index: number): boolean }, 'isPinned')
        const count = 2_000
        source.applyOptimisticRange(0, 0, Array.from({ length: count }, (_, index) => message(`m${index}`, `row ${index}`)))

        expect(source.snapshot()).toMatchObject({ totalMessages: count })
        expect(source.snapshot().rowAt(count - 1)?.message).toMatchObject({ data: `row ${count - 1}` })
        expect(source.snapshot().rowAt(count - 8)?.message).toMatchObject({ data: `row ${count - 8}` })
        expect(source.snapshot().rowAt(count - 9)).toBeUndefined()
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(isPinned.mock.calls.length).toBeLessThanOrEqual(2 * count)
    })

    it('projects middle and tail removals and restores the rows on rollback', async () => {
        const readConversationWindow = vi.fn(async ({ startIndex = 0, limit = 3 }: ConversationWindowQuery) => ({
            revision: 7,
            value: persistentWindow(startIndex, [
                message('zero', 'stored-zero'),
                message('one', 'stored-one'),
                message('two', 'stored-two'),
            ].slice(startIndex, startIndex + limit), 3),
        }))
        const source = new EditableViewportFixture({
            reader: { readConversationWindow },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 8,
        })
        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        const keyBefore = source.snapshot().keyAt(1)

        const undoMiddle = source.applyOptimisticRange(0, 1, [])
        expect(source.snapshot().totalMessages).toBe(2)
        undoMiddle()
        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        const rollback = source.applyOptimisticRange(2, 1, [])
        expect(source.snapshot()).toMatchObject({ totalMessages: 2 })
        expect(source.snapshot().rowAt(2)).toBeUndefined()
        expect(source.snapshot().keyAt(1)).not.toBe(keyBefore)
        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        expect(source.snapshot().rowAt(2)).toBeUndefined()

        const replace = source.applyOptimisticRange(1, 1, [
            message('one', 'replaced-one'),
            message('generated', 'streamed'),
        ])
        expect(source.snapshot()).toMatchObject({ totalMessages: 3 })
        expect(source.snapshot().rowAt(2)?.message).toMatchObject({ data: 'streamed' })
        replace()
        rollback()
        expect(source.snapshot()).toMatchObject({ totalMessages: 3 })
        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        expect(source.snapshot().rowAt(2)?.message).toMatchObject({ data: 'stored-two' })
    })

    it('derives owned keys in constant space and loads an exact persistent window', async () => {
        const readConversationWindow = vi.fn(
            async (input: ConversationWindowQuery): Promise<Versioned<ConversationWindow>> => ({
                revision: 7,
                value: {
                    characterId: input.characterId,
                    conversationId: input.conversationId,
                    messages: [message('message-999999998', 'loaded')],
                    startIndex: 999_999_998,
                    endIndex: 999_999_999,
                    totalMessages: 1_000_000_000,
                    hasMoreBefore: true,
                    hasMoreAfter: true,
                },
            }),
        )
        const source = new PersistentConversationViewportSource({
            reader: { readConversationWindow },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 1_000_000_000,
            rowBudget: 8,
        })
        const before = source.snapshot()
        const key = before.keyAt(999_999_998)!

        expect(before.indexOfKey(key)).toBe(999_999_998)
        expect(before.indexOfKey(`${key}x` as ConversationViewportKey)).toBe(-1)
        expect(before.keyAt(1_000_000_000)).toBeUndefined()

        await source.ensureRange({
            startIndex: 999_999_998,
            limit: 1,
            reason: 'viewport',
        })

        expect(readConversationWindow).toHaveBeenCalledWith({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            startIndex: 999_999_998,
            limit: 1,
        })
        expect(source.snapshot().rowAt(999_999_998)).toMatchObject({
            key,
            absoluteIndex: 999_999_998,
            message: { data: 'loaded' },
            sourceVersion: 0,
        })
        expect(source.captureMessageTarget(key)).toBeNull()
    })

    it('rejects structurally mismatched windows and discards another revision', async () => {
        const mismatches: Array<[string, Partial<ConversationWindow>]> = [
            ['character', { characterId: 'character-b' }],
            ['conversation', { conversationId: 'conversation-b' }],
            ['start', { startIndex: 1 }],
            ['end', { endIndex: 2 }],
            ['total', { totalMessages: 4 }],
            ['message count', { messages: [] }],
        ]

        for (const [label, overrides] of mismatches) {
            const source = new PersistentConversationViewportSource({
                reader: {
                    readConversationWindow: async () => ({
                        revision: 7,
                        value: persistentWindow(0, [message('zero', 'zero')], 3, overrides),
                    }),
                },
                characterId: 'character-a',
                conversationId: 'conversation-a',
                revision: 7,
                totalMessages: 3,
                rowBudget: 2,
            })

            await expect(source.ensureRange({
                startIndex: 0,
                limit: 1,
                reason: 'viewport',
            }), label).rejects.toThrow(/mismatched window/)
            expect(source.snapshot().rowAt(0)).toBeUndefined()
        }

        const listener = vi.fn()
        const staleRevisionSource = new PersistentConversationViewportSource({
            reader: {
                readConversationWindow: async () => ({
                    revision: 8,
                    value: persistentWindow(0, [message('zero', 'zero')], 3),
                }),
            },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 2,
        })
        staleRevisionSource.subscribe(listener)

        await staleRevisionSource.ensureRange({
            startIndex: 0,
            limit: 1,
            reason: 'viewport',
        })

        expect(staleRevisionSource.snapshot().rowAt(0)).toBeUndefined()
        expect(listener).not.toHaveBeenCalled()

        const missingSource = new PersistentConversationViewportSource({
            reader: { readConversationWindow: async () => null },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 2,
        })
        await expect(missingSource.ensureRange({
            startIndex: 0,
            limit: 1,
            reason: 'viewport',
        })).rejects.toThrow(/was not found/)
    })

    it('advances its epoch and discards pending old-epoch, aborted, and disposed reads', async () => {
        const pending = deferred<Versioned<ConversationWindow> | null>()
        const readConversationWindow = vi.fn(() => pending.promise)
        const source = new PersistentConversationViewportSource({
            reader: { readConversationWindow },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 2,
        })
        const listener = vi.fn()
        source.subscribe(listener)
        const oldKey = source.snapshot().keyAt(0)!
        const stale = source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })

        source.advanceRevision(8, 4)
        pending.resolve({
            revision: 7,
            value: persistentWindow(0, [message('zero', 'stale')], 3),
        })
        await stale

        const advanced = source.snapshot()
        expect(advanced).toMatchObject({ version: 1, totalMessages: 4 })
        expect(advanced.keyAt(0)).not.toBe(oldKey)
        expect(advanced.indexOfKey(oldKey)).toBe(-1)
        expect(advanced.rowAt(0)).toBeUndefined()
        expect(listener).toHaveBeenCalledTimes(1)

        const abortedPending = deferred<Versioned<ConversationWindow> | null>()
        readConversationWindow.mockImplementationOnce(() => abortedPending.promise)
        const controller = new AbortController()
        const aborted = source.ensureRange({
            startIndex: 0,
            limit: 1,
            reason: 'jump',
            signal: controller.signal,
        })
        controller.abort()
        abortedPending.resolve({
            revision: 8,
            value: persistentWindow(0, [message('zero', 'aborted')], 4),
        })
        await aborted
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(listener).toHaveBeenCalledTimes(1)

        const disposedPending = deferred<Versioned<ConversationWindow> | null>()
        readConversationWindow.mockImplementationOnce(() => disposedPending.promise)
        const disposed = source.ensureRange({ startIndex: 0, limit: 1, reason: 'streaming' })
        source.dispose()
        disposedPending.resolve({
            revision: 8,
            value: persistentWindow(0, [message('zero', 'disposed')], 4),
        })
        await disposed
        expect(source.snapshot().totalMessages).toBe(0)
        expect(listener).toHaveBeenCalledTimes(2)
    })

    it('retains pinned rows beyond the budget and evicts least-recent unpinned rows', async () => {
        const readConversationWindow = vi.fn(async (input: ConversationWindowQuery) => {
            const endIndex = Math.min(6, input.startIndex! + input.limit!)
            const messages = Array.from(
                { length: endIndex - input.startIndex! },
                (_, offset) => message(
                    `message-${input.startIndex! + offset}`,
                    `data-${input.startIndex! + offset}`,
                ),
            )
            return {
                revision: 7,
                value: persistentWindow(input.startIndex!, messages, 6),
            }
        })
        const source = new PersistentConversationViewportSource({
            reader: { readConversationWindow },
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 6,
            rowBudget: 2,
        })
        const listener = vi.fn()
        source.subscribe(listener)
        const oldViewport = source.acquireRangePin(0, 2, 'viewport')
        await source.ensureRange({ startIndex: 0, limit: 2, reason: 'viewport' })

        const nextViewport = source.acquireRangePin(2, 4, 'viewport')
        await source.ensureRange({ startIndex: 2, limit: 2, reason: 'viewport' })
        for (let index = 0; index < 4; index++) {
            expect(source.snapshot().rowAt(index)?.absoluteIndex).toBe(index)
        }

        oldViewport.release()
        oldViewport.release()
        expect(listener).toHaveBeenCalledTimes(3)
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(source.snapshot().rowAt(1)).toBeUndefined()
        expect(source.snapshot().rowAt(2)).toBeDefined()
        expect(source.snapshot().rowAt(3)).toBeDefined()

        nextViewport.release()
        expect(listener).toHaveBeenCalledTimes(3)
        source.snapshot().rowAt(2)
        await source.ensureRange({ startIndex: 4, limit: 1, reason: 'jump' })
        expect(listener).toHaveBeenCalledTimes(4)
        expect(source.snapshot().rowAt(2)).toBeDefined()
        expect(source.snapshot().rowAt(3)).toBeUndefined()
        expect(source.snapshot().rowAt(4)).toBeDefined()
    })

    it('rejects malformed or foreign keys and invalidates old-epoch pins', async () => {
        const reader = {
            readConversationWindow: vi.fn(async () => ({
                revision: 8,
                value: persistentWindow(0, [
                    message('zero', 'zero'),
                    message('one', 'one'),
                ], 2),
            })),
        }
        const source = new PersistentConversationViewportSource({
            reader,
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 2,
            rowBudget: 1,
        })
        const original = source.snapshot()
        const originalKey = original.keyAt(0)!
        const separator = originalKey.lastIndexOf('|')
        const prefix = originalKey.slice(0, separator + 1)
        const foreign = new PersistentConversationViewportSource({
            reader,
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 2,
            rowBudget: 1,
        })
        const malformed = [
            `${prefix}`,
            `${prefix}-1`,
            `${prefix}01`,
            `${prefix}1.0`,
            `${prefix}9007199254740992`,
            foreign.snapshot().keyAt(0)!,
        ] as ConversationViewportKey[]
        for (const key of malformed) expect(original.indexOfKey(key)).toBe(-1)

        const oldPin = source.acquireRangePin(0, 1, 'playing-media')
        source.advanceRevision(8, 2)
        await source.ensureRange({ startIndex: 0, limit: 2, reason: 'viewport' })

        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(source.snapshot().rowAt(1)).toBeDefined()
        expect(() => oldPin.release()).not.toThrow()
    })

    it('retains rows and live pins on unrelated revision advances and reads later pages at new authority', async () => {
        let revision = 7
        const reader = { readConversationWindow: vi.fn(async (query) => ({ revision,
            value: persistentWindow(query.startIndex, [message(`id-${query.startIndex}`, 'synthetic')], 4) })) }
        const source = new PersistentConversationViewportSource({ reader, characterId: 'character-a', conversationId: 'conversation-a', revision, totalMessages: 4, rowBudget: 1 })
        const pin = source.acquireRangePin(0, 1, 'editor')
        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        const before = source.snapshot()
        const row = before.rowAt(0)
        const listener = vi.fn()
        source.subscribe(listener)
        revision = 8
        source.advanceUnchangedRevision(revision)
        const advanced = source.snapshot()
        expect(advanced.storeRevision).toBe(8)
        expect(advanced.sourceToken).toBe(before.sourceToken)
        expect(advanced.version).toBe(before.version)
        expect(advanced.keyAt(0)).toBe(before.keyAt(0))
        expect(advanced.rowAt(0)).toBe(row)
        expect(listener).not.toHaveBeenCalled()
        await source.ensureRange({ startIndex: 3, limit: 1, reason: 'viewport' })
        expect(source.snapshot().rowAt(0)).toBe(row)
        expect(reader.readConversationWindow).toHaveBeenLastCalledWith({ characterId: 'character-a', conversationId: 'conversation-a', startIndex: 3, limit: 1 })
        pin.release()
        expect(() => source.advanceUnchangedRevision(7)).toThrow(/backwards/)
        source.dispose()
        expect(() => source.advanceUnchangedRevision(9)).toThrow()
    })

    it('keeps an earlier legacy position proven across a tail append and its acknowledgement', async () => {
        const original = [message(undefined, 'legacy'), message('b', 'second')]
        const source = new EditableViewportFixture({
            characterId: 'character-a', conversationId: 'conversation-a', revision: 1, totalMessages: 2, rowBudget: 2,
            reader: { readConversationWindow: async () => null },
        })
        const first = source.snapshot().positionKeyAt!(0)
        const oldSecond = source.snapshot().positionKeyAt!(1)
        const tail = message('tail', 'reply')
        source.applyOptimisticRange(2, 0, [tail])
        expect(source.snapshot().positionKeyAt!(0)).toBe(first)
        const plan = await source.edits.plan(source.edits.freeze(), { revision: 1, readConversationWindow: async () => null }, 2)
        source.edits.acknowledge(plan, 2)
        source.installProjection(2, 3, false)
        expect(source.snapshot().positionKeyAt!(0)).toBe(first)

        source.edits.applyRange(1, [original[1]], [], 2)
        source.refreshProjection({start: 1, end: 1, structureChanged: true})
        expect(source.snapshot().positionKeyAt!(0)).toBe(first)
        expect(source.snapshot().positionKeyAt!(1)).not.toBe(oldSecond)
        await source.edits.rebase({revision: 3, readConversationWindow: async () => null}, 3)
        source.installProjection(3, 3, true)
        expect(source.snapshot().positionKeyAt!(0)).not.toBe(first)
        source.dispose()
    })

    it('publishes a multi-range mutation with both dirty rows and one subscriber notification', async () => {
        const messages = [message('a', 'a'), message('b', 'b'), message('c', 'c')]
        const source = new EditableViewportFixture({
            characterId: 'character-a', conversationId: 'conversation-a', revision: 1, totalMessages: 3, rowBudget: 3,
            reader: { readConversationWindow: async () => ({ revision: 1, value: persistentWindow(0, messages, 3) }) },
        })
        await source.ensureRange({ startIndex: 0, limit: 3, reason: 'viewport' })
        const observed: string[][] = []
        source.subscribe(() => {
            const snapshot = source.snapshot()
            observed.push([snapshot.rowAt(0)!.message.data, snapshot.rowAt(2)!.message.data])
        })
        source.edits.applyRange(0, [messages[0]], [message('a', 'edited a')], 1)
        source.edits.applyRange(2, [messages[2]], [message('c', 'edited c')], 2)
        source.refreshProjections([{start: 0, end: 1, structureChanged: false}, {start: 2, end: 3, structureChanged: false}])
        expect(observed).toEqual([['edited a', 'edited c']])
    })

    it.each([false, true])('projects evicted replacements, removals and appends across acknowledgement (metadata=%s)', async metadata => {
        const store = new IndexedDbPersistentDataStore('viewport-edits', new IDBFactory(), IDBKeyRange)
        await store.open()
        const seed = structuredClone(fixtureDatabase)
        const character = seed.characters.find(owner => owner.chaId === 'char-a')!
        const conversation = character.chats.find(chat => chat.id === 'conv-long')!
        conversation.message = [
            { ...message('a', 'zero'), role: 'user' }, message('b', 'one'), message('c', 'two'),
            { ...message('d', 'three'), role: 'user' }, message('e', 'four'),
        ]
        const { revision } = await store.replaceFromDatabase(seed)
        const reader = { readConversationWindow: store.readConversationWindow.bind(store),
            readConversationMessageMetadataWindow: metadata ? store.readConversationMessageMetadataWindow.bind(store) : undefined }
        const source = new EditableViewportFixture({ reader, characterId: character.chaId, conversationId: conversation.id!,
            revision, totalMessages: 5, rowBudget: 1 })
        const read = async (index: number) => {
            await source.ensureRange({ startIndex: index, limit: 1, reason: 'viewport' })
            return source.snapshot().rowAt(index)?.message
        }
        const signal = new AbortController().signal
        const check = async () => {
            expect((await read(2))?.chatId).toBe('d')
            expect((await read(3))?.data).toBe('four')
            expect(source.snapshot().rowAt(0)).toBeUndefined()
            expect(source.snapshot().rowAt(4)).toBeUndefined()
            await expect(source.findLatestMessagePair(signal)).resolves.toEqual([0, 4])
            expect(await read(0)).toMatchObject({ data: 'edited zero', role: 'char' })
            expect(await read(4)).toMatchObject({ data: 'local tail', role: 'user' })
        }
        try {
            expect(await source.findLatestMessagePair(signal)).toEqual([3, 4])
            await read(1)
            source.applyOptimisticRange(1, 1, [])
            source.applyOptimisticRange(0, 1, [message('a', 'edited zero')])
            source.applyOptimisticRange(1, 3, [
                { ...message('c', 'two'), disabled: true },
                { ...message('d', 'three'), isComment: true },
                { ...message('e', 'four'), disabled: true },
            ])
            source.applyOptimisticRange(4, 0, [{ ...message('tail', 'local tail'), role: 'user' }])
            const projected = source.captureProjectedReader(reader)
            const anchor = (id: string, occurrence: 'first' | 'last') => projected.readConversationWindow({
                characterId: character.chaId, conversationId: conversation.id!, anchorMessageId: id, anchorOccurrence: occurrence,
                before: 1, after: 1,
            })
            await expect(anchor('b', 'first')).resolves.toBeNull()
            expect((await anchor('tail', 'last'))?.value.messages.map(row => row.chatId)).toEqual(['e', 'tail'])
            await check()
            const pair = source.findLatestMessagePair(signal)
            source.applyOptimisticRange(4, 1, [{ ...message('tail', 'local tail'), role: 'user', time: 123 }])
            expect(source.findLatestMessagePair(signal)).toBe(pair)
            const lease = await store.acquireRevision(revision)
            const plan = await source.edits.plan(source.edits.freeze(), lease, 5).finally(() => lease.release())
            const saved = await store.commit({ expectedRevision: revision, conversations: plan.mutations })
            source.edits.acknowledge(plan, saved.revision)
            source.installProjection(saved.revision, 5, false)
            await check()
        } finally { source.dispose() }
    })

    it('resolves the surviving first and last occurrences when a projection hides the stored extremes', async () => {
        const store = new IndexedDbPersistentDataStore('viewport-anchors', new IDBFactory(), IDBKeyRange)
        await store.open()
        const seed = structuredClone(fixtureDatabase)
        const character = seed.characters.find(owner => owner.chaId === 'char-a')!
        const conversation = character.chats.find(chat => chat.id === 'conv-long')!
        const rows = Array.from({ length: 132 }, (_, i) => message([0, 64, 128, 131].includes(i) ? 'duplicate' : `m${i}`, String(i)))
        conversation.message = rows
        const { revision } = await store.replaceFromDatabase(seed)
        const source = new EditableViewportFixture({ reader: store, characterId: character.chaId, conversationId: conversation.id!,
            revision, totalMessages: rows.length, rowBudget: 1 })
        await source.ensureRange({ startIndex: 131, limit: 1, reason: 'viewport' })
        source.applyOptimisticRange(131, 1, [message('renamed', '131')])
        await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
        source.applyOptimisticRange(0, 1, [])
        const reader = source.captureProjectedReader(store)
        for (const [anchorOccurrence, index] of [['first', 63], ['last', 127]] as const) {
            const result = await reader.readConversationWindow({ characterId: character.chaId, conversationId: conversation.id!,
                anchorMessageId: 'duplicate', anchorOccurrence })
            expect(result?.value).toMatchObject({ startIndex: index, endIndex: index + 1,
                messages: [message('duplicate', String(index + 1))] })
        }
        source.dispose()
    })

    it.each([false, true])('cancels a pair whose stored count changed when its pending tail was acknowledged (metadata=%s)', async metadata => {
        const released = deferred<void>()
        const stored = [message('a', 'first'), { ...message('b', 'second'), role: 'user' as const }]
        const readConversationWindow = vi.fn(async ({ startIndex = 0, limit = 2 }: ConversationWindowQuery) => {
            await released.promise
            return { revision: 1, value: persistentWindow(startIndex, stored.slice(startIndex, startIndex + limit), 2) }
        })
        const source = new EditableViewportFixture({ characterId: 'character-a', conversationId: 'conversation-a',
            revision: 1, totalMessages: 2, rowBudget: 1, reader: {
                readConversationWindow,
                readConversationMessageMetadataWindow: metadata ? async query => {
                    const { revision, value } = await readConversationWindow(query)
                    return { revision, value: { ...value, messages: value.messages.map(({ role }) => ({ role, parserInert: true })) } }
                } : undefined,
            } })
        source.applyOptimisticRange(2, 0, [message('tail', 'third')])
        const lookup = source.findLatestMessagePair(new AbortController().signal)
        await vi.waitFor(() => expect(readConversationWindow).toHaveBeenCalled())
        const prefix = source.edits.freeze()
        const plan = await source.edits.plan(prefix, { revision: 1, readConversationWindow: async () => null }, 2)
        source.edits.acknowledge(plan, 2)
        source.installProjection(2, 3, false)
        released.resolve()
        await expect(lookup).rejects.toMatchObject({ name: 'AbortError' })
        source.dispose()
    })

    it.each([false, true])('cancels a stale pair and window on count changes and same-count reorder (metadata=%s)', async metadata => {
        let gate = deferred<void>()
        const stored = [message('a', 'first'), { ...message('b', 'second'), role: 'user' as const }]
        let revision = 1
        const reader = { readConversationWindow: async ({ startIndex = 0, limit = 1 }: ConversationWindowQuery) => {
            const result = { revision, value: persistentWindow(startIndex, stored.slice(startIndex, startIndex + limit), stored.length) }
            await gate.promise
            return result
        } }
        const source = new EditableViewportFixture({ characterId: 'character-a', conversationId: 'conversation-a',
            reader: { ...reader, readConversationMessageMetadataWindow: metadata ? async query => {
                const { revision, value } = await reader.readConversationWindow(query)
                return { revision, value: { ...value, messages: value.messages.map(({ role }) => ({ role, parserInert: true })) } }
            } : undefined },
            revision, totalMessages: 2, rowBudget: 1 })
        const signal = new AbortController().signal
        const run = async (change: () => unknown) => {
            const old = source.snapshot().keyAt(0)!
            const lookup = source.findLatestMessagePair(signal)
            const load = source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
            await change()
            gate.resolve()
            await expect(lookup).rejects.toMatchObject({ name: 'AbortError' })
            await load
            expect(source.snapshot().indexOfKey(old)).toBe(-1)
            expect(source.snapshot().rowAt(0)).toBeUndefined()
            gate = deferred<void>()
        }
        const tail = message('tail', 'third')
        await run(() => source.applyOptimisticRange(2, 0, [tail]))
        await run(async () => {
            stored.reverse()
            revision++
            await source.edits.rebase({ revision, readConversationWindow: async query => {
                const index = stored.findIndex(row => row.chatId === query.anchorMessageId)
                return index < 0 ? null : { revision, value: persistentWindow(index, [stored[index]], stored.length) }
            } }, stored.length)
            source.installProjection(revision, 2, true)
        })
        gate.resolve()
        await expect(source.findLatestMessagePair(signal)).resolves.toEqual([0, 2])
        source.dispose()
    })

    it('validates authority, range, pin, and monotonic revision inputs', async () => {
        const reader = {
            readConversationWindow: vi.fn(async () => null),
        }
        expect(() => new PersistentConversationViewportSource({
            reader,
            characterId: '',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 2,
        })).toThrow(/Character ID/)
        expect(() => new PersistentConversationViewportSource({
            reader,
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 0,
        })).toThrow(/row budget/)

        const source = new PersistentConversationViewportSource({
            reader,
            characterId: 'character-a',
            conversationId: 'conversation-a',
            revision: 7,
            totalMessages: 3,
            rowBudget: 2,
        })
        await expect(source.ensureRange({
            startIndex: -1,
            limit: 1,
            reason: 'viewport',
        })).rejects.toThrow(/startIndex/)
        expect(reader.readConversationWindow).not.toHaveBeenCalled()
        expect(() => source.acquireRangePin(0, 0, 'editor')).toThrow(/must not be empty/)
        expect(() => source.acquireRangePin(0, 4, 'playing-media')).toThrow(/message count/)
        expect(() => source.advanceRevision(7, 4)).toThrow(/must advance/)
        expect(() => source.advanceRevision(8, -1)).toThrow(/message count/)

        const streaming = source.acquireRangePin(2, 3, 'streaming')
        expect(() => streaming.release()).not.toThrow()
        expect(() => streaming.release()).not.toThrow()
    })
})


it.each([7, 8])('keeps a latest-pair scan valid across an unrelated revision (reader revision %s)', async revision => {
    const pending = deferred<Versioned<ConversationWindow>>()
    const source = new PersistentConversationViewportSource({
        reader: { readConversationWindow: () => pending.promise },
        characterId: 'character-a', conversationId: 'conversation-a', revision: 7, totalMessages: 2, rowBudget: 2,
    })
    const lookup = source.findLatestMessagePair(new AbortController().signal)
    source.advanceUnchangedRevision(8)
    pending.resolve({ revision, value: persistentWindow(0, [{ ...message('u', 'synthetic'), role: 'user' }, message('c', 'synthetic')], 2) })
    await expect(lookup).resolves.toEqual([0, 1])
    expect(source.findLatestMessagePair(new AbortController().signal)).toBe(lookup)
    source.dispose()
})
