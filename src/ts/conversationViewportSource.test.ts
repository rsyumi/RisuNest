import { describe, expect, it, vi } from 'vitest'
import {
    SynchronousSessionConversationViewportSource,
    type ConversationViewportKey,
    type ConversationViewportSource,
} from './conversationViewportSource'
import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Message, character } from './storage/database.svelte'

function message(chatId: string | undefined, data: string): Message {
    return { role: 'char', data, ...(chatId === undefined ? {} : { chatId }) }
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

        session.delete(session.locate(1))
        expect(source.captureMessageTarget(key)).toBeNull()
    })
})
