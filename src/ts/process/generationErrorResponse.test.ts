import { describe, expect, it, vi } from 'vitest'
import type { Chat, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { applyGenerationErrorResponse } from './generationErrorResponse'

function chat(messages: Message[]): Chat {
    return {
        id: 'chat-1',
        name: 'Conversation',
        message: messages,
        note: '',
        localLore: [],
        fmIndex: -1,
    }
}

function message(role: Message['role'], data: string, chatId: string): Message {
    return { role, data, chatId, time: 1 }
}

function harness(messages: Message[]) {
    const conversation = chat(messages)
    let currentChat: Chat = conversation
    let currentSession: ActiveConversationSession | null = new ActiveConversationSession({
        characterId: 'character-1',
        conversationId: conversation.id!,
        conversation,
        storeRevision: 1,
    })
    const apply = () => applyGenerationErrorResponse({
        session: currentSession,
        getCurrentSession: () => currentSession,
        characterId: 'character-1',
        chat: conversation,
        getCurrentChat: () => currentChat,
        suffix: '\n```risuerror\nfailed\n```',
        appendMessage: message('char', '```risuerror\nfailed\n```', 'error-1'),
    })
    return {
        apply,
        conversation,
        get session() { return currentSession },
        replaceSession(session: ActiveConversationSession | null) {
            currentSession = session
        },
        replaceChat(replacement: Chat) {
            currentChat = replacement
        },
    }
}

describe('generation error response', () => {
    it('edits an existing character tail through the matching active session', () => {
        const source = harness([message('char', 'partial', 'response-1')])
        const onMutation = vi.fn()
        const tracked = new ActiveConversationSession({
            characterId: 'character-1',
            conversationId: source.conversation.id!,
            conversation: source.conversation,
            storeRevision: 1,
            onMutation,
        })
        source.replaceSession(tracked)

        expect(source.apply()).toBe(true)
        expect(source.conversation.message[0].data).toBe(
            'partial\n```risuerror\nfailed\n```',
        )
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 1,
            commands: ['edit'],
            mutations: [expect.objectContaining({ start: 0, deleteCount: 1 })],
        }))
        expect(tracked.pinCount('transaction')).toBe(0)
    })

    it('appends a new character error through the matching active session', () => {
        const source = harness([message('user', 'prompt', 'user-1')])
        const onMutation = vi.fn()
        const tracked = new ActiveConversationSession({
            characterId: 'character-1',
            conversationId: source.conversation.id!,
            conversation: source.conversation,
            storeRevision: 1,
            onMutation,
        })
        source.replaceSession(tracked)

        expect(source.apply()).toBe(true)
        expect(source.conversation.message.at(-1)).toMatchObject({
            role: 'char',
            data: '```risuerror\nfailed\n```',
            chatId: 'error-1',
        })
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 1,
            commands: ['append'],
            mutations: [expect.objectContaining({ start: 1, deleteCount: 0 })],
        }))
        expect(tracked.pinCount('transaction')).toBe(0)
    })

    it('fails closed when the active session or selected chat is no longer the captured owner', () => {
        const source = harness([message('char', 'partial', 'response-1')])
        const replacementChat = chat([message('char', 'replacement', 'replacement-1')])
        const replacementSession = new ActiveConversationSession({
            characterId: 'character-1',
            conversationId: replacementChat.id!,
            conversation: replacementChat,
            storeRevision: 2,
        })

        source.replaceSession(replacementSession)
        source.replaceChat(replacementChat)

        expect(source.apply()).toBe(false)
        expect(source.conversation.message[0].data).toBe('partial')
        expect(replacementChat.message[0].data).toBe('replacement')
        expect(replacementSession.pinCount('transaction')).toBe(0)
    })

    it('does not retarget the error after the session version changes during tail capture', () => {
        const source = harness([message('char', 'partial', 'response-1')])
        const session = source.session!
        const readLatest = session.readLatest.bind(session)
        vi.spyOn(session, 'readLatest').mockImplementation((limit) => {
            const window = readLatest(limit)
            session.append(message('char', 'concurrent', 'response-2'))
            return window
        })

        expect(source.apply()).toBe(false)
        expect(source.conversation.message).toEqual([
            message('char', 'partial', 'response-1'),
            message('char', 'concurrent', 'response-2'),
        ])
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('preserves the direct full-array fallback when no active session exists', () => {
        const existing = harness([message('char', 'partial', 'response-1')])
        existing.replaceSession(null)

        expect(existing.apply()).toBe(true)
        expect(existing.conversation.message[0].data).toBe(
            'partial\n```risuerror\nfailed\n```',
        )

        const appended = harness([message('user', 'prompt', 'user-1')])
        appended.replaceSession(null)
        expect(appended.apply()).toBe(true)
        expect(appended.conversation.message.at(-1)?.chatId).toBe('error-1')
    })
})
