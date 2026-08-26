import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import { encodeLegacyConversationProjection } from './conversationCompatibility.testUtils'
import { captureConversationMutationTarget } from './conversationMutations'
import {
    applyConversationRerollTail,
    captureConversationRerollTail,
    replaceConversationRerollLastData,
    truncateConversationForReroll,
} from './conversationReroll'

function createConversation(messages: Message[]): Chat {
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

function messages(...data: string[]): Message[] {
    return data.map((value, index) => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: value,
        ...(index === 0 || index === 2 ? { chatId: 'duplicate' } : {}),
    }))
}

function legacyTailOverlay(current: Message[], tail: readonly Message[]): Message[] {
    const result = structuredClone(current)
    const replacement = structuredClone(tail)
    for (let index = 0; index < replacement.length; index++) {
        result[result.length - replacement.length + index] = replacement[index]
    }
    return result
}

describe('conversation reroll mutations', () => {
    it.each([
        ['same length', messages('new-a', 'new-b')],
        ['shorter tail', messages('new-only')],
        ['empty tail', []],
    ])('matches the legacy %s overlay through a strict reroll command', (_name, tail) => {
        const original = messages('zero', 'one', 'two')
        const expected = legacyTailOverlay(original, tail)
        const conversation = createConversation(structuredClone(original))
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

        applyConversationRerollTail(target, tail, 'reroll')

        expect(conversation.message).toEqual(expected)
        expect(encodeLegacyConversationProjection({ message: conversation.message })).toEqual(
            encodeLegacyConversationProjection({ message: expected }),
        )
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['reroll'],
        }))
    })

    it.each([
        [
            'ordinary assistant tail',
            [
                { role: 'user', data: 'user' },
                { role: 'char', data: 'assistant' },
            ] as Message[],
            ['user'],
        ],
        [
            'same-speaker group tail',
            [
                { role: 'user', data: 'user' },
                { role: 'char', data: 'first', saying: 'speaker-a' },
                { role: 'char', data: 'second', saying: 'speaker-a' },
            ] as Message[],
            ['user', 'first'],
        ],
        [
            'user tail',
            [{ role: 'user', data: 'user' }] as Message[],
            ['user'],
        ],
    ])('preserves the legacy truncation point for an %s', (_name, input, expected) => {
        const conversation = createConversation(input)
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        expect(truncateConversationForReroll(target)).toBe(true)

        expect(conversation.message.map((message) => message.data)).toEqual(expected)
    })

    it('replaces cached reroll data through a strict tail command with a missing ID', () => {
        const conversation = createConversation([
            { role: 'user', data: 'user', chatId: 'duplicate' },
            { role: 'char', data: 'old cached response' },
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

        expect(replaceConversationRerollLastData(
            target,
            'new cached response',
            'unreroll',
        )).toBe(true)

        expect(conversation.message).toEqual([
            { role: 'user', data: 'user', chatId: 'duplicate' },
            { role: 'char', data: 'new cached response' },
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail'],
        }))
    })

    it('captures only the generated tail as detached reroll history', () => {
        const conversation = createConversation(messages('zero', 'one', 'two', 'three'))
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)

        const tail = captureConversationRerollTail(target, 2)
        tail[0].data = 'changed snapshot'

        expect(tail.map((message) => message.data)).toEqual(['changed snapshot', 'three'])
        expect(conversation.message.map((message) => message.data)).toEqual([
            'zero',
            'one',
            'two',
            'three',
        ])
    })

    it('leaves an empty conversation unchanged and reports no reroll truncation', () => {
        const conversation = createConversation([])
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

        expect(truncateConversationForReroll(target)).toBe(false)
        expect(conversation.message).toEqual([])
        expect(onMutation).not.toHaveBeenCalled()
    })

    it('uses replace-tail for unreroll history', () => {
        const conversation = createConversation(messages('zero', 'one', 'two'))
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

        applyConversationRerollTail(target, messages('restored'), 'unreroll')

        expect(conversation.message.map((message) => message.data)).toEqual([
            'zero',
            'one',
            'restored',
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail'],
        }))
    })
})
