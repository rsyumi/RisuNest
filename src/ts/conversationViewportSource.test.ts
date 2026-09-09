import { describe, expect, it, vi } from 'vitest'
import {
    PersistentConversationViewportSource,
    SynchronousSessionConversationViewportSource,
    type ConversationViewportKey,
    type ConversationViewportSource,
} from './conversationViewportSource'
import { ActiveConversationSession } from './storage/activeConversationSession'
import { createConversationOperationContext } from './process/conversationOperationContext'
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
        maxResidentBytes: 1024,
        measureMessage: () => 1,
    })
    const source = new SynchronousSessionConversationViewportSource({
        session,
        captureCurrent: () => ({ character: owner, conversation }),
    })
    return { conversation, owner, session, source }
}

describe('SynchronousSessionConversationViewportSource', () => {
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

    it('still invalidates rows for externally changed variables', async () => {
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
