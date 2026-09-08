import { describe, expect, it, vi } from 'vitest'

import { PersistentConversationViewportSource } from './conversationViewportSource'
import { readSelectedConversationLatestTail } from './selectedConversationTail'
import type { SelectedConversationTarget } from './storage/activeWorkingSet.svelte'
import type { Message } from './storage/database.svelte'

function message(index: number): Message {
    return {
        role: index % 2 === 0 ? 'user' : 'char',
        data: `message-${index}`,
    }
}

function harness(totalMessages = 25) {
    let revision = 7
    const messages = Array.from({ length: totalMessages }, (_, index) =>
        message(index),
    )
    const readConversationWindow = vi.fn(
        async ({ startIndex = 0, limit = 128 }) => ({
            revision,
            value: {
                characterId: 'character-a',
                conversationId: 'conversation-a',
                messages: structuredClone(
                    messages.slice(startIndex, startIndex + limit),
                ),
                startIndex,
                endIndex: Math.min(totalMessages, startIndex + limit),
                totalMessages,
                hasMoreBefore: startIndex > 0,
                hasMoreAfter: startIndex + limit < totalMessages,
            },
        }),
    )
    const source = new PersistentConversationViewportSource({
        reader: { readConversationWindow },
        characterId: 'character-a',
        conversationId: 'conversation-a',
        revision: 7,
        totalMessages,
        rowBudget: 16,
    })
    const target = {
        characterId: 'character-a',
        conversationId: 'conversation-a',
        navigationGeneration: 2,
        storeRevision: 7,
    } as SelectedConversationTarget
    let currentTarget: SelectedConversationTarget | null = target
    let currentSource = source
    return {
        source,
        target,
        readConversationWindow,
        runtime: {
            captureSelectedConversationTarget: () => currentTarget,
            getActiveConversationViewportSource: () => currentSource,
        },
        invalidateTarget() {
            currentTarget = null
        },
        replaceSource(next: PersistentConversationViewportSource) {
            currentSource = next
        },
        advanceRevision() {
            revision += 1
            currentTarget = { ...currentTarget!, storeRevision: revision }
            currentSource.advanceRevision(revision, totalMessages)
        },
        navigateAwayAndBack() {
            currentTarget = {
                ...currentTarget!,
                navigationGeneration: currentTarget!.navigationGeneration + 2,
            }
        },
    }
}

describe('selected conversation latest tail', () => {
    it('reads at most ten messages from the exact selected viewport revision', async () => {
        const source = harness()

        const tail = await readSelectedConversationLatestTail(
            source.runtime,
            10,
        )

        expect(tail.map((item) => item.data)).toEqual(
            Array.from({ length: 10 }, (_, index) => `message-${index + 15}`),
        )
        expect(source.readConversationWindow).toHaveBeenCalledOnce()
        expect(source.readConversationWindow).toHaveBeenCalledWith({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            startIndex: 15,
            limit: 10,
        })
    })

    it('rejects a tail read when the exact selected target changes', async () => {
        const source = harness()
        source.readConversationWindow.mockImplementationOnce(async (input) => {
            source.invalidateTarget()
            const startIndex = input.startIndex ?? 0
            const limit = input.limit ?? 10
            return {
                revision: 7,
                value: {
                    characterId: 'character-a',
                    conversationId: 'conversation-a',
                    messages: Array.from({ length: limit }, (_, index) =>
                        message(startIndex + index),
                    ),
                    startIndex,
                    endIndex: startIndex + limit,
                    totalMessages: 25,
                    hasMoreBefore: true,
                    hasMoreAfter: false,
                },
            }
        })

        await expect(
            readSelectedConversationLatestTail(source.runtime, 10),
        ).rejects.toThrow(
            'Selected conversation changed while reading its tail',
        )
    })

    it('retries the latest tail once when a save advances the selected revision', async () => {
        const source = harness()
        const read = source.readConversationWindow.getMockImplementation()!
        source.readConversationWindow.mockImplementationOnce(async (input) => {
            const result = await read(input)
            source.advanceRevision()
            return result
        })

        const tail = await readSelectedConversationLatestTail(
            source.runtime,
            10,
        )

        expect(tail.map((item) => item.data)).toEqual(
            Array.from({ length: 10 }, (_, index) => `message-${index + 15}`),
        )
        expect(source.readConversationWindow).toHaveBeenCalledTimes(2)
    })

    it('bounds retries when saves repeatedly invalidate the tail read', async () => {
        const source = harness()
        const read = source.readConversationWindow.getMockImplementation()!
        source.readConversationWindow.mockImplementation(async (input) => {
            const result = await read(input)
            source.advanceRevision()
            return result
        })

        await expect(
            readSelectedConversationLatestTail(source.runtime, 10),
        ).rejects.toThrow(
            'Selected conversation changed while reading its tail',
        )
        expect(source.readConversationWindow).toHaveBeenCalledTimes(2)
    })

    it('does not retry a newer revision after navigating away and back', async () => {
        const source = harness()
        const read = source.readConversationWindow.getMockImplementation()!
        source.readConversationWindow.mockImplementationOnce(async (input) => {
            const result = await read(input)
            source.advanceRevision()
            source.navigateAwayAndBack()
            return result
        })

        await expect(
            readSelectedConversationLatestTail(source.runtime, 10),
        ).rejects.toThrow(
            'Selected conversation changed while reading its tail',
        )
        expect(source.readConversationWindow).toHaveBeenCalledOnce()
    })

    it('does not retry a revision change after cancellation', async () => {
        const source = harness()
        const controller = new AbortController()
        const read = source.readConversationWindow.getMockImplementation()!
        source.readConversationWindow.mockImplementationOnce(async (input) => {
            const result = await read(input)
            source.advanceRevision()
            controller.abort()
            return result
        })

        await expect(
            readSelectedConversationLatestTail(
                source.runtime,
                10,
                controller.signal,
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(source.readConversationWindow).toHaveBeenCalledOnce()
    })

    it('preserves storage failures instead of retrying them after a save', async () => {
        const source = harness()
        const failure = new Error('Storage unavailable')
        source.readConversationWindow.mockImplementationOnce(async () => {
            source.advanceRevision()
            throw failure
        })

        await expect(
            readSelectedConversationLatestTail(source.runtime, 10),
        ).rejects.toBe(failure)
        expect(source.readConversationWindow).toHaveBeenCalledOnce()
    })

    it('rejects requests above the suggestion tail bound before reading', async () => {
        const source = harness()

        await expect(
            readSelectedConversationLatestTail(source.runtime, 11),
        ).rejects.toThrow('cannot exceed 10')
        expect(source.readConversationWindow).not.toHaveBeenCalled()
    })

    it('rejects a viewport source from a different selected revision', async () => {
        const source = harness()
        Object.assign(source.target, { storeRevision: 8 })

        await expect(
            readSelectedConversationLatestTail(source.runtime, 10),
        ).rejects.toThrow(
            'Selected conversation changed while reading its tail',
        )
        expect(source.readConversationWindow).not.toHaveBeenCalled()
    })
})
