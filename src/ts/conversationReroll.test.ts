import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import { encodeLegacyConversationProjection } from './conversationCompatibility.testUtils'
import { captureConversationMutationTarget } from './conversationMutations'
import {
    applyConversationRerollTail,
    appendConversationRerollHistory,
    captureConversationRerollTail,
    captureConversationRerollTransition,
    createConversationRerollHistory,
    isConversationRerollHistoryCurrent,
    moveConversationRerollHistory,
    refreshConversationRerollHistory,
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

        applyConversationRerollTail(
            target,
            captureConversationRerollTransition(target, tail, 'reroll'),
        )

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

        applyConversationRerollTail(
            target,
            captureConversationRerollTransition(
                target,
                messages('restored'),
                'unreroll',
            ),
        )

        expect(conversation.message.map((message) => message.data)).toEqual([
            'zero',
            'one',
            'restored',
        ])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['replace-tail'],
        }))
    })

    it('rejects history after switching conversations within the same character', () => {
        const conversationA = createConversation(messages('a-user', 'a-old'))
        const conversationB = {
            ...createConversation(messages('b-user', 'b-current')),
            id: 'conversation-b',
        }
        const character = createCharacter(conversationA)
        character.chats.push(conversationB)
        const sessionA = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversationA.id,
            conversation: conversationA,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(character, conversationA, sessionA)
        let history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
        )
        sessionA.reroll(sessionA.positionAt(1), [{ role: 'char', data: 'a-current' }])
        const currentTarget = captureConversationMutationTarget(
            character,
            conversationA,
            sessionA,
        )
        history = appendConversationRerollHistory(
            history,
            currentTarget,
            captureConversationRerollTail(currentTarget, 1),
            oldTarget,
        )

        character.chatPage = 1
        const sessionB = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversationB.id,
            conversation: conversationB,
            storeRevision: 1,
        })
        const switchedTarget = captureConversationMutationTarget(
            character,
            conversationB,
            sessionB,
        )

        expect(moveConversationRerollHistory(
            history,
            switchedTarget,
            'unreroll',
        )).toBeNull()
        expect(conversationA.message.map((message) => message.data)).toEqual([
            'a-user',
            'a-current',
        ])
        expect(conversationB.message.map((message) => message.data)).toEqual([
            'b-user',
            'b-current',
        ])
    })

    it('rebinds history after the same persisted conversation is demoted and promoted', () => {
        const conversation = createConversation(messages('user', 'old'))
        const character = createCharacter(conversation)
        const firstSession = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(character, conversation, firstSession)
        let history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
            4,
        )
        firstSession.reroll(firstSession.positionAt(1), [{ role: 'char', data: 'current' }])
        const currentTarget = captureConversationMutationTarget(
            character,
            conversation,
            firstSession,
        )
        history = appendConversationRerollHistory(
            history,
            currentTarget,
            captureConversationRerollTail(currentTarget, 1),
            oldTarget,
        )
        firstSession.invalidate()
        const returnedConversation = createConversation(
            structuredClone(conversation.message),
        )
        const returnedCharacter = createCharacter(returnedConversation)
        const returnedSession = new ActiveConversationSession({
            characterId: returnedCharacter.chaId,
            conversationId: returnedConversation.id,
            conversation: returnedConversation,
            storeRevision: 1,
        })
        const returnedTarget = captureConversationMutationTarget(
            returnedCharacter,
            returnedConversation,
            returnedSession,
        )

        expect(isConversationRerollHistoryCurrent(history, returnedTarget, 4)).toBe(true)
        const rebound = refreshConversationRerollHistory(history, returnedTarget)
        const moved = moveConversationRerollHistory(
            rebound!,
            returnedTarget,
            'unreroll',
        )

        expect(moved).not.toBeNull()
        expect(returnedConversation.message.map((message) => message.data)).toEqual([
            'user',
            'old',
        ])
        expect(conversation.message.map((message) => message.data)).toEqual([
            'user',
            'current',
        ])
    })

    it('invalidates history after leaving and returning to the same conversation', () => {
        const conversationA = createConversation(messages('a-user', 'a-old'))
        const conversationB = createConversation(messages('b-user', 'b-current'))
        conversationB.id = 'conversation-b'
        const character = createCharacter(conversationA)
        character.chats.push(conversationB)
        const firstSession = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversationA.id,
            conversation: conversationA,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(
            character,
            conversationA,
            firstSession,
        )
        const history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
            1,
        )

        character.chatPage = 1
        firstSession.invalidate()
        character.chatPage = 0
        const returnedConversation = createConversation(
            structuredClone(conversationA.message),
        )
        character.chats[0] = returnedConversation
        const returnedSession = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: returnedConversation.id,
            conversation: returnedConversation,
            storeRevision: 1,
        })
        const returnedTarget = captureConversationMutationTarget(
            character,
            returnedConversation,
            returnedSession,
        )

        expect(isConversationRerollHistoryCurrent(history, returnedTarget, 3)).toBe(false)
        expect(returnedConversation.message.map((message) => message.data)).toEqual([
            'a-user',
            'a-old',
        ])
    })

    it('invalidates rebound history after an intervening tail mutation', () => {
        const conversation = createConversation(messages('user', 'old'))
        const character = createCharacter(conversation)
        const firstSession = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(character, conversation, firstSession)
        let history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
        )
        firstSession.reroll(firstSession.positionAt(1), [{ role: 'char', data: 'current' }])
        const currentTarget = captureConversationMutationTarget(
            character,
            conversation,
            firstSession,
        )
        history = appendConversationRerollHistory(
            history,
            currentTarget,
            captureConversationRerollTail(currentTarget, 1),
            oldTarget,
        )
        firstSession.invalidate()
        const returnedConversation = createConversation([
            { role: 'user', data: 'user' },
            { role: 'char', data: 'intervening mutation' },
        ])
        const returnedCharacter = createCharacter(returnedConversation)
        const returnedSession = new ActiveConversationSession({
            characterId: returnedCharacter.chaId,
            conversationId: returnedConversation.id,
            conversation: returnedConversation,
            storeRevision: 2,
        })
        const returnedTarget = captureConversationMutationTarget(
            returnedCharacter,
            returnedConversation,
            returnedSession,
        )

        expect(isConversationRerollHistoryCurrent(history, returnedTarget)).toBe(false)
        expect(refreshConversationRerollHistory(history, returnedTarget)).toBeNull()
        expect(moveConversationRerollHistory(
            history,
            returnedTarget,
            'unreroll',
        )).toBeNull()
        expect(returnedConversation.message.map((message) => message.data)).toEqual([
            'user',
            'intervening mutation',
        ])
    })

    it('rejects a stored tail position after an unmigrated direct tail replacement', () => {
        const conversation = createConversation(messages('user', 'old'))
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(character, conversation, session)
        let history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
        )
        session.reroll(session.positionAt(1), [{ role: 'char', data: 'current' }])
        const currentTarget = captureConversationMutationTarget(character, conversation, session)
        history = appendConversationRerollHistory(
            history,
            currentTarget,
            captureConversationRerollTail(currentTarget, 1),
            oldTarget,
        )
        conversation.message[1] = { role: 'char', data: 'direct replacement' }
        const directlyMutatedTarget = captureConversationMutationTarget(
            character,
            conversation,
            session,
        )

        expect(moveConversationRerollHistory(
            history,
            directlyMutatedTarget,
            'unreroll',
        )).toBeNull()
        expect(conversation.message.map((message) => message.data)).toEqual([
            'user',
            'direct replacement',
        ])
    })

    it('rejects history after the same session version advances', () => {
        const conversation = createConversation(messages('user', 'old'))
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(character, conversation, session)
        let history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
        )
        session.reroll(session.positionAt(1), [{ role: 'char', data: 'current' }])
        const currentTarget = captureConversationMutationTarget(character, conversation, session)
        history = appendConversationRerollHistory(
            history,
            currentTarget,
            captureConversationRerollTail(currentTarget, 1),
            oldTarget,
        )
        session.append({ role: 'char', data: 'newer session message' })
        const advancedTarget = captureConversationMutationTarget(character, conversation, session)

        expect(moveConversationRerollHistory(
            history,
            advancedTarget,
            'unreroll',
        )).toBeNull()
        expect(conversation.message.map((message) => message.data)).toEqual([
            'user',
            'current',
            'newer session message',
        ])
    })

    it('preserves end-relative overlay behavior across undo and forward history', () => {
        const conversation = createConversation(messages('base', 'old-a', 'old-b'))
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const oldTarget = captureConversationMutationTarget(character, conversation, session)
        let history = createConversationRerollHistory(
            oldTarget,
            captureConversationRerollTail(oldTarget, 1),
        )
        session.reroll(session.positionAt(1), [{ role: 'char', data: 'new-only' }])
        let currentTarget = captureConversationMutationTarget(character, conversation, session)
        history = appendConversationRerollHistory(
            history,
            currentTarget,
            captureConversationRerollTail(currentTarget, 1),
            oldTarget,
        )
        const beforeUndo = structuredClone(conversation.message)

        const undone = moveConversationRerollHistory(history, currentTarget, 'unreroll')

        expect(undone).not.toBeNull()
        expect(conversation.message).toEqual(legacyTailOverlay(
            beforeUndo,
            history.entries[0],
        ))
        currentTarget = captureConversationMutationTarget(character, conversation, session)
        const beforeForward = structuredClone(conversation.message)

        const forwarded = moveConversationRerollHistory(undone!, currentTarget, 'reroll')

        expect(forwarded?.index).toBe(1)
        expect(conversation.message).toEqual(legacyTailOverlay(
            beforeForward,
            history.entries[1],
        ))
    })
})
