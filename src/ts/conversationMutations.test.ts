import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import { encodeLegacyConversationProjection } from './conversationCompatibility.testUtils'
import {
    appendConversationComment,
    appendCurrentConversationMessage,
    appendConversationMessage,
    captureConversationMutationTarget,
    ConversationMutationTargetStaleError,
    ensureCurrentConversationMessageIds,
    cutConversationMessages,
    isConversationMutationTargetCurrent,
    resetConversationWithMessage,
    retainConversationDeleteSlice,
} from './conversationMutations'

function createConversation(messages: Message[] = []): Chat {
    return {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: messages,
    }
}

function createCharacter(conversation: Chat): Database['characters'][number] {
    return {
        type: 'character',
        chaId: 'character-a',
        chatPage: 0,
        chats: [conversation],
    } as Database['characters'][number]
}

describe('conversation mutations', () => {
    it('routes generation-start ID assignment through the matching session', () => {
        const conversation = createConversation([
            { role: 'user', data: 'missing' },
            { role: 'char', data: 'empty', chatId: '' },
        ])
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })

        expect(ensureCurrentConversationMessageIds(
            character,
            conversation,
            session,
            () => 'generated',
        )).toBe(1)

        expect(conversation.message.map((message) => message.chatId)).toEqual(['generated', ''])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['edit'],
        }))
    })

    it('routes a current interactive append through the matching session', () => {
        const conversation = createConversation([{ role: 'user', data: 'before' }])
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })

        appendCurrentConversationMessage(
            character,
            conversation,
            session,
            { role: 'char', data: 'interactive append' },
        )

        expect(conversation.message.map((message) => message.data)).toEqual([
            'before',
            'interactive append',
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['append'],
        }))
    })

    it('rejects an interactive append owned by another active session', () => {
        const conversation = createConversation([{ role: 'user', data: 'before' }])
        const character = createCharacter(conversation)
        const otherConversation = createConversation()
        otherConversation.id = 'conversation-b'
        const otherSession = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: otherConversation.id,
            conversation: otherConversation,
            storeRevision: 1,
        })

        expect(() => appendCurrentConversationMessage(
            character,
            conversation,
            otherSession,
            { role: 'char', data: 'wrong owner' },
        )).toThrow(ConversationMutationTargetStaleError)
        expect(conversation.message.map((message) => message.data)).toEqual(['before'])
    })

    it('routes append through a matching session and preserves the direct fallback', () => {
        const sessionConversation = createConversation([{ role: 'user', data: 'before' }])
        const sessionCharacter = createCharacter(sessionConversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: sessionCharacter.chaId,
            conversationId: sessionConversation.id,
            conversation: sessionConversation,
            storeRevision: 1,
            onMutation,
        })
        const sessionTarget = captureConversationMutationTarget(
            sessionCharacter,
            sessionConversation,
            session,
        )

        appendConversationMessage(sessionTarget, { role: 'char', data: 'session append' })

        expect(sessionConversation.message.map((message) => message.data)).toEqual([
            'before',
            'session append',
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['append'],
        }))

        const fallbackConversation = createConversation([{ role: 'user', data: 'before' }])
        const fallbackTarget = captureConversationMutationTarget(
            createCharacter(fallbackConversation),
            fallbackConversation,
            null,
        )

        appendConversationMessage(fallbackTarget, { role: 'char', data: 'fallback append' })

        expect(fallbackConversation.message.map((message) => message.data)).toEqual([
            'before',
            'fallback append',
        ])
        expect(encodeLegacyConversationProjection({ message: sessionConversation.message })).toEqual(
            encodeLegacyConversationProjection({
                message: [
                    { role: 'user', data: 'before' },
                    { role: 'char', data: 'session append' },
                ],
            }),
        )
    })

    it('rejects a captured target after character, conversation, or session navigation', () => {
        const conversation = createConversation()
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        expect(isConversationMutationTargetCurrent(
            target,
            character,
            conversation,
            session,
        )).toBe(true)
        expect(isConversationMutationTargetCurrent(
            target,
            createCharacter(conversation),
            conversation,
            session,
        )).toBe(false)
        expect(isConversationMutationTargetCurrent(
            target,
            character,
            createConversation(),
            session,
        )).toBe(false)
        expect(isConversationMutationTargetCurrent(
            target,
            character,
            conversation,
            null,
        )).toBe(false)
    })

    it('preserves the command processor numeric cut splice result through the session', () => {
        const conversation = createConversation([
            { role: 'user', data: 'zero', chatId: 'duplicate' },
            { role: 'char', data: 'one' },
            { role: 'user', data: 'two', chatId: 'duplicate' },
        ])
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        cutConversationMessages(target, '1')

        expect(conversation.message).toEqual([{ role: 'char', data: 'one' }])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail'],
        }))
    })

    it.each([
        ['range cut', 'cut', '1-3', ['one', 'two']],
        ['duplicate ID cut', 'cut', 'duplicate', ['one', 'three']],
        ['missing ID cut', 'cut', 'missing', ['zero', 'one', 'two', 'three']],
        ['positive del', 'del', '2', ['two', 'three']],
        ['oversized del', 'del', '5', ['three']],
        ['zero del', 'del', '0', []],
    ])('preserves the %s selection result', (_name, command, argument, expected) => {
        const conversation = createConversation([
            { role: 'user', data: 'zero', chatId: 'duplicate' },
            { role: 'char', data: 'one' },
            { role: 'user', data: 'two', chatId: 'duplicate' },
            { role: 'char', data: 'three', chatId: 'other' },
        ])
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        if (command === 'cut') cutConversationMessages(target, argument)
        else retainConversationDeleteSlice(target, argument)

        expect(conversation.message.map((message) => message.data)).toEqual(expected)
    })

    it('edits only the last message for the comment command', () => {
        const conversation = createConversation([
            { role: 'user', data: 'first', chatId: 'duplicate' },
            { role: 'char', data: 'last' },
        ])
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        appendConversationComment(target, '<Comment>note</Comment>')

        expect(conversation.message).toEqual([
            { role: 'user', data: 'first', chatId: 'duplicate' },
            { role: 'char', data: 'last<Comment>note</Comment>' },
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['edit'],
        }))
    })

    it('publishes clear and append as one session transaction', () => {
        const conversation = createConversation([
            { role: 'user', data: 'old user' },
            { role: 'char', data: 'old reply' },
        ])
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        resetConversationWithMessage(target, { role: 'user', data: 'new user' })

        expect(conversation.message).toEqual([{ role: 'user', data: 'new user' }])
        expect(onMutation).toHaveBeenCalledTimes(1)
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail', 'append'],
        }))
    })

    it('appends to an awaited trigger result without restoring the old array', () => {
        const conversation = createConversation([{ role: 'user', data: 'before trigger' }])
        const character = createCharacter(conversation)
        const onMutation = vi.fn()
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })
        const target = captureConversationMutationTarget(character, conversation, session)
        const triggerMessages: Message[] = [{ role: 'char', data: 'trigger result' }]

        appendConversationMessage(
            target,
            { role: 'user', data: 'new input' },
            triggerMessages,
        )

        expect(conversation.message).toEqual([
            { role: 'char', data: 'trigger result' },
            { role: 'user', data: 'new input' },
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail', 'append'],
        }))
    })

    it('rejects an awaited append after the same session version advances', () => {
        const conversation = createConversation([{ role: 'user', data: 'before trigger' }])
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)
        const triggerMessages: Message[] = [{ role: 'char', data: 'stale trigger result' }]

        session.append({ role: 'char', data: 'newer session message' })

        expect(isConversationMutationTargetCurrent(
            target,
            character,
            conversation,
            session,
        )).toBe(false)
        expect(() => appendConversationMessage(
            target,
            { role: 'user', data: 'stale input' },
            triggerMessages,
        )).toThrow(ConversationMutationTargetStaleError)
        expect(conversation.message.map((message) => message.data)).toEqual([
            'before trigger',
            'newer session message',
        ])
    })
})
