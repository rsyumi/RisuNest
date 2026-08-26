import { describe, expect, it, vi } from 'vitest'
import type { Chat, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import {
    captureGenerationConversationOperation,
    type GenerationConversationOperation,
} from './generationConversationOperation'

function message(data: string, chatId: string): Message {
    return { role: 'char', data, chatId, time: 1 }
}

function chat(messages: Message[]): Chat {
    return { id: 'chat-1', message: messages, note: '', localLore: [], fmIndex: -1 }
}

function createHarness(messages: Message[]) {
    let currentChat = chat(messages)
    let currentSession: ActiveConversationSession | null = new ActiveConversationSession({
        characterId: 'character-1',
        conversationId: 'chat-1',
        conversation: currentChat,
        storeRevision: 1,
    })
    const capture = (options: {
        append?: Message
        continueLast?: boolean
        messageId?: string
    }): GenerationConversationOperation => captureGenerationConversationOperation({
        session: currentSession,
        getCurrentSession: () => currentSession,
        chat: currentChat,
        getCurrentChat: () => currentChat,
        ...options,
    })
    return {
        capture,
        get chat() { return currentChat },
        get session() { return currentSession },
        replace(nextMessages: Message[]) {
            currentSession?.invalidate()
            currentChat = chat(nextMessages)
            currentSession = new ActiveConversationSession({
                characterId: 'character-1',
                conversationId: 'chat-1',
                conversation: currentChat,
                storeRevision: 2,
            })
        },
    }
}

describe('generation conversation operation', () => {
    it('preserves append, streaming, final, and async postprocessing order through locators', async () => {
        const harness = createHarness([message('user prompt', 'user-1')])
        const operation = harness.capture({ append: message('', 'generation-1') })

        expect(operation.snapshot()?.data).toBe('')
        expect(operation.commitData('preview')).toBe(true)
        expect(operation.commitData('final')).toBe(true)

        await Promise.resolve()
        expect(operation.commitData('final with inlay')).toBe(true)

        expect(harness.chat.message.map((entry) => entry.data)).toEqual([
            'user prompt',
            'final with inlay',
        ])
        expect(operation.absoluteIndex).toBe(1)
        expect(operation.messageId).toBe('generation-1')
        expect(harness.session?.pinCount('transaction')).toBe(1)

        operation.release()
        expect(harness.session?.pinCount('transaction')).toBe(0)
    })

    it('continues the captured final message without retargeting another message', () => {
        const harness = createHarness([
            message('user prompt', 'user-1'),
            message('existing response', 'response-1'),
        ])
        const operation = harness.capture({ continueLast: true })

        expect(operation.snapshot()).toMatchObject({
            data: 'existing response',
            chatId: 'response-1',
        })
        expect(operation.commitMessage({
            ...operation.snapshot()!,
            data: 'existing response continued',
        })).toBe(true)

        expect(harness.chat.message[1]).toMatchObject({
            data: 'existing response continued',
            chatId: 'response-1',
        })
        operation.release()
    })

    it('fails closed when async work completes after the conversation is replaced', async () => {
        const harness = createHarness([message('old', 'generation-1')])
        const operation = harness.capture({ messageId: 'generation-1' })
        let finish!: () => void
        const delayed = new Promise<void>((resolve) => { finish = resolve })

        const completion = (async () => {
            await delayed
            return operation.commitData('stale completion')
        })()
        harness.replace([message('replacement', 'generation-1')])
        finish()

        await expect(completion).resolves.toBe(false)
        expect(harness.chat.message[0].data).toBe('replacement')
        operation.release()
    })

    it('requires an explicit refresh after a synchronous compatibility transform', () => {
        const harness = createHarness([message('before', 'generation-1')])
        const operation = harness.capture({ messageId: 'generation-1' })

        harness.chat.message = harness.chat.message.map((entry) => ({
            ...entry,
            data: 'compatibility transform',
        }))

        expect(operation.commitData('must not retarget')).toBe(false)
        expect(operation.refresh()).toBe(true)
        expect(operation.commitData('after refresh')).toBe(true)
        expect(harness.chat.message[0].data).toBe('after refresh')
        operation.release()
    })

    it('keeps a named full-array fallback when no active session is available', () => {
        const currentChat = chat([message('user prompt', 'user-1')])
        const mutation = vi.fn()
        const operation = captureGenerationConversationOperation({
            session: null,
            getCurrentSession: () => null,
            chat: currentChat,
            getCurrentChat: () => currentChat,
            append: message('', 'generation-1'),
            onFallbackMutation: mutation,
        })

        expect(operation.usesFullArrayFallback).toBe(true)
        expect(operation.commitData('fallback final')).toBe(true)
        expect(currentChat.message.at(-1)?.data).toBe('fallback final')
        expect(mutation).toHaveBeenCalledTimes(2)
        operation.release()
    })
})
