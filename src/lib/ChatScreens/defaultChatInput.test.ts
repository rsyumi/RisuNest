import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from '../../ts/storage/activeConversationSession'
import type { Chat, Database, Message } from '../../ts/storage/database.svelte'
import {
    captureConversationMutationTarget,
    isConversationMutationTargetCurrent,
} from '../../ts/conversationMutations'
import { appendDefaultChatInput } from './defaultChatInput'
import { createConversationOperationContext } from '../../ts/process/conversationOperationContext'

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

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((promiseResolve) => {
        resolve = promiseResolve
    })
    return { promise, resolve }
}

describe('DefaultChatScreen input append helper', () => {
    it.each([false, true])(
        'keeps plugin metadata saved during input processing (followed by script: %s)',
        async (followWithScript) => {
            const conversation = createConversation([{ role: 'char', data: 'before' }])
            const character = createCharacter(conversation)
            const session = new ActiveConversationSession({
                characterId: character.chaId,
                conversationId: conversation.id,
                conversation,
                storeRevision: 1,
            })
            const target = captureConversationMutationTarget(character, conversation, session)
            const result = await appendDefaultChatInput({
                target,
                runInputTrigger: async () => null,
                processInput: async (onCommitted) => {
                    const next = structuredClone(conversation)
                    next.scriptstate = { $bridge: 'on' }
                    expect(session.adoptPersistedMetadata(next, 2)).toBe(true)
                    if (followWithScript) {
                        const operation = createConversationOperationContext(
                            session,
                            conversation,
                            onCommitted,
                        )
                        operation.chat.scriptstate.$script = 'after plugin'
                        operation.commit(session)
                    }
                    return 'input'
                },
                isTargetCurrent: (current) =>
                    isConversationMutationTargetCurrent(current, character, conversation, session),
                createMessage: (data) => ({ role: 'user', data }),
            })
            expect(result).toBe(true)
            expect(conversation.scriptstate).toMatchObject({ $bridge: 'on' })
            if (followWithScript) expect(conversation.scriptstate.$script).toBe('after plugin')
            expect(conversation.message.map((message) => message.data)).toEqual(['before', 'input'])
        },
    )
    it('does not restore a stale trigger tail after the same session changes', async () => {
        const conversation = createConversation([{ role: 'char', data: 'before' }])
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)
        const trigger = deferred<{ chat: Chat } | null>()

        const pending = appendDefaultChatInput({
            target,
            runInputTrigger: () => trigger.promise,
            processInput: vi.fn(async () => 'stale input'),
            isTargetCurrent: () => isConversationMutationTargetCurrent(
                target,
                character,
                conversation,
                session,
            ),
            createMessage: (data) => ({ role: 'user', data }),
        })
        session.append({ role: 'char', data: 'newer message' })
        trigger.resolve({
            chat: createConversation([{ role: 'char', data: 'stale trigger result' }]),
        })

        await expect(pending).resolves.toBe(false)
        expect(conversation.message.map((message) => message.data)).toEqual([
            'before',
            'newer message',
        ])
    })

    it('does not append processed input after the same session changes', async () => {
        const conversation = createConversation([{ role: 'char', data: 'before' }])
        const character = createCharacter(conversation)
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const target = captureConversationMutationTarget(character, conversation, session)
        const processing = deferred<string>()
        const processingStarted = deferred<void>()

        const pending = appendDefaultChatInput({
            target,
            runInputTrigger: vi.fn(async () => null),
            processInput: () => {
                processingStarted.resolve()
                return processing.promise
            },
            isTargetCurrent: () => isConversationMutationTargetCurrent(
                target,
                character,
                conversation,
                session,
            ),
            createMessage: (data) => ({ role: 'user', data }),
        })
        await processingStarted.promise
        session.append({ role: 'char', data: 'newer message' })
        processing.resolve('processed stale input')

        await expect(pending).resolves.toBe(false)
        expect(conversation.message.map((message) => message.data)).toEqual([
            'before',
            'newer message',
        ])
    })
})
