import { expect, it, vi } from 'vitest'
import type { CharacterDetail, PersistentDataStore } from './persistentDataStore'
import {
    PersistentConversationReadCancelledError,
    PersistentConversationReadStaleError,
    readPinnedSelectedConversationWindow,
} from './persistentConversationRead'
import type { Message } from './database.svelte'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function createHarness(messages: Message[]) {
    const revision = 7
    let navigationGeneration = 3
    const release = vi.fn(async () => undefined)
    const readConversationWindow = vi.fn(async (input: {
        characterId: string
        conversationId: string
        startIndex?: number
        limit?: number
    }) => {
        const startIndex = Math.min(messages.length, input.startIndex ?? 0)
        const endIndex = Math.min(messages.length, startIndex + (input.limit ?? 128))
        return {
            revision,
            value: {
                characterId: input.characterId,
                conversationId: input.conversationId,
                messages: structuredClone(messages.slice(startIndex, endIndex)),
                startIndex,
                endIndex,
                totalMessages: messages.length,
                hasMoreBefore: startIndex > 0,
                hasMoreAfter: endIndex < messages.length,
            },
        }
    })
    const character = {
        chaId: 'char-a',
        name: 'Alpha',
        type: 'character',
        chatPage: 0,
    } as CharacterDetail
    const lease = {
        revision,
        readCharacter: vi.fn(async () => ({ revision, value: character })),
        queryConversations: vi.fn(async () => ({
            revision,
            items: [{
                id: 'conv-a',
                characterId: 'char-a',
                name: 'Conversation',
                configuredIndex: 0,
                recentAt: 0,
                messageCount: messages.length,
            }],
        })),
        readConversationWindow,
        release,
    }
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision, value: {} })),
        acquireRevision: vi.fn(async () => lease),
        readConversation: vi.fn(),
        materializeDatabase: vi.fn(),
    } as unknown as PersistentDataStore
    const flushPendingData = vi.fn(async () => undefined)
    const read = (signal?: AbortSignal, count = 3, offset = 2) =>
        readPinnedSelectedConversationWindow({
            store,
            flushPendingData,
            getNavigationGeneration: () => navigationGeneration,
        }, {
            characterId: 'char-a',
            count,
            offset,
            reason: 'test-read',
            signal,
        })
    return {
        flushPendingData,
        lease,
        read,
        readConversationWindow,
        release,
        setNavigationGeneration(value: number) {
            navigationGeneration = value
        },
        store,
    }
}

it('reads a bounded newest-first page from one pinned absolute range', async () => {
    const messages = Array.from({ length: 10_000 }, (_, index) => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: `message-${index}`,
        chatId: index === 9_996 || index === 9_997 ? 'duplicate' : undefined,
    } as Message))
    const harness = createHarness(messages)

    const result = await harness.read()

    expect(result).toMatchObject({
        revision: 7,
        character: { chaId: 'char-a', name: 'Alpha' },
        conversation: {
            startIndex: 9_995,
            endIndex: 9_998,
            totalMessages: 10_000,
        },
    })
    expect(result?.conversation?.messages.map((message) => message.data)).toEqual([
        'message-9995',
        'message-9996',
        'message-9997',
    ])
    expect(result?.conversation?.messages.map((message) => message.chatId)).toEqual([
        undefined,
        'duplicate',
        'duplicate',
    ])
    expect(harness.readConversationWindow).toHaveBeenCalledOnce()
    expect(harness.lease.queryConversations).toHaveBeenCalledOnce()
    expect(harness.readConversationWindow).toHaveBeenCalledWith({
        characterId: 'char-a',
        conversationId: 'conv-a',
        startIndex: 9_995,
        limit: 3,
    })
    expect(harness.store.readConversation).not.toHaveBeenCalled()
    expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
    expect(harness.release).toHaveBeenCalledOnce()
})

it('rejects an oversized page before flushing or opening persistence', async () => {
    const harness = createHarness([])

    await expect(harness.read(undefined, 4_097, 0)).rejects.toBeInstanceOf(RangeError)
    expect(harness.flushPendingData).not.toHaveBeenCalled()
    expect(harness.store.open).not.toHaveBeenCalled()
})

it('rejects a stale navigation generation and releases the pinned revision', async () => {
    const messages = Array.from({ length: 4 }, (_, index) => ({
        role: 'user',
        data: `message-${index}`,
    } as Message))
    const harness = createHarness(messages)
    const pendingWindow = deferred<Awaited<ReturnType<typeof harness.readConversationWindow>>>()
    harness.readConversationWindow.mockImplementationOnce(() => pendingWindow.promise)

    const reading = harness.read()
    await vi.waitFor(() => expect(harness.readConversationWindow).toHaveBeenCalledOnce())
    harness.setNavigationGeneration(4)
    pendingWindow.resolve({
        revision: 7,
        value: {
            characterId: 'char-a',
            conversationId: 'conv-a',
            messages: [{ role: 'user', data: 'one' }],
            startIndex: 0,
            endIndex: 1,
            totalMessages: 1,
            hasMoreBefore: false,
            hasMoreAfter: false,
        },
    })

    await expect(reading).rejects.toBeInstanceOf(PersistentConversationReadStaleError)
    expect(harness.release).toHaveBeenCalledOnce()
})

it('honors cancellation during a range read and releases the pinned revision', async () => {
    const messages = Array.from({ length: 4 }, (_, index) => ({
        role: 'user',
        data: `message-${index}`,
    } as Message))
    const harness = createHarness(messages)
    const pendingWindow = deferred<Awaited<ReturnType<typeof harness.readConversationWindow>>>()
    harness.readConversationWindow.mockImplementationOnce(() => pendingWindow.promise)
    const controller = new AbortController()

    const reading = harness.read(controller.signal)
    await vi.waitFor(() => expect(harness.readConversationWindow).toHaveBeenCalledOnce())
    controller.abort()
    pendingWindow.resolve({
        revision: 7,
        value: {
            characterId: 'char-a',
            conversationId: 'conv-a',
            messages: [{ role: 'user', data: 'one' }],
            startIndex: 0,
            endIndex: 1,
            totalMessages: 1,
            hasMoreBefore: false,
            hasMoreAfter: false,
        },
    })

    await expect(reading).rejects.toBeInstanceOf(PersistentConversationReadCancelledError)
    expect(harness.release).toHaveBeenCalledOnce()
})

it('releases a lease acquired after cancellation', async () => {
    const harness = createHarness([{ role: 'user', data: 'one' } as Message])
    const pendingLease = deferred<Awaited<ReturnType<typeof harness.store.acquireRevision>>>()
    vi.mocked(harness.store.acquireRevision).mockImplementationOnce(() => pendingLease.promise)
    const controller = new AbortController()

    const reading = harness.read(controller.signal)
    await vi.waitFor(() => expect(harness.store.acquireRevision).toHaveBeenCalledOnce())
    controller.abort()
    pendingLease.resolve(
        harness.lease as unknown as Awaited<ReturnType<typeof harness.store.acquireRevision>>,
    )

    await expect(reading).rejects.toBeInstanceOf(PersistentConversationReadCancelledError)
    expect(harness.release).toHaveBeenCalledOnce()
})

it('preserves cancellation when releasing the just-acquired lease fails', async () => {
    const harness = createHarness([{ role: 'user', data: 'one' } as Message])
    const releaseError = new Error('release failed')
    harness.release.mockRejectedValue(releaseError)
    const pendingLease = deferred<Awaited<ReturnType<typeof harness.store.acquireRevision>>>()
    vi.mocked(harness.store.acquireRevision).mockImplementationOnce(() => pendingLease.promise)
    const controller = new AbortController()
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)

    const reading = harness.read(controller.signal)
    await vi.waitFor(() => expect(harness.store.acquireRevision).toHaveBeenCalledOnce())
    controller.abort()
    pendingLease.resolve(
        harness.lease as unknown as Awaited<ReturnType<typeof harness.store.acquireRevision>>,
    )

    await expect(reading).rejects.toBeInstanceOf(PersistentConversationReadCancelledError)
    expect(harness.release).toHaveBeenCalledTimes(2)
    expect(consoleError).toHaveBeenCalledWith(
        'Persistent conversation revision release failed after current-read validation failed',
        releaseError,
    )
    consoleError.mockRestore()
})
