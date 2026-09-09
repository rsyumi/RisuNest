import { beforeEach, describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { Chat, Database } from '../storage/database.svelte'
import { setCurrentChat } from '../storage/database.svelte'
import { createConversationOperationContext } from './conversationOperationContext'
import { runTrigger } from './triggers'

const mocks = vi.hoisted(() => ({
    database: null as Database | null,
    session: null as ActiveConversationSession | null,
    selectedCharacterIndex: 0,
    sendChat: vi.fn(async () => undefined),
    setDatabase: vi.fn(),
}))

vi.mock('../storage/database.svelte', () => ({
    getCurrentCharacter: () => mocks.database!.characters[mocks.selectedCharacterIndex],
    getCurrentChat: () => {
        const character = mocks.database!.characters[mocks.selectedCharacterIndex]
        return character.chats[character.chatPage]
    },
    getDatabase: () => mocks.database,
    setCurrentChat: vi.fn(),
    setDatabase: mocks.setDatabase,
}))
vi.mock('../stores.svelte', () => ({
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(mocks.selectedCharacterIndex)
            return () => undefined
        },
    },
}))
vi.mock('../alert', () => ({
    alertInput: vi.fn(async () => ''),
    alertMd: vi.fn(),
    alertNormal: vi.fn(),
    alertSelect: vi.fn(async () => ''),
}))
vi.mock('../parser/parser.svelte', () => ({ risuChatParser: (value: string) => value }))
vi.mock('./index.svelte', () => ({ sendChat: mocks.sendChat }))
vi.mock('./lorebook.svelte', () => ({ loadLoreBookV3Prompt: vi.fn() }))
vi.mock('./triggers', () => ({ runTrigger: vi.fn() }))
vi.mock('./tts', () => ({ sayTTS: vi.fn() }))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => mocks.session,
}))

import { processMultiCommand } from './command'

function createDatabase(): Database {
    const conversation = {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: [{ role: 'char', data: 'before' }],
    } as Chat
    return {
        characters: [{
            type: 'character',
            chaId: 'character-a',
            chatPage: 0,
            chats: [conversation],
        }],
    } as Database
}

function createCharacter(id: string, messageData: string): Database['characters'][number] {
    return {
        type: 'character',
        chaId: `${id}-character`,
        chatPage: 0,
        chats: [{
            id: `${id}-conversation`,
            name: id,
            note: '',
            localLore: [],
            message: [{ role: 'char', data: messageData }],
        }],
    } as Database['characters'][number]
}

describe('processMultiCommand conversation mutations', () => {
    beforeEach(() => {
        mocks.database = createDatabase()
        mocks.selectedCharacterIndex = 0
        mocks.sendChat.mockClear()
        mocks.setDatabase.mockClear()
        mocks.session = null
    })

    it('routes send through the matching active conversation session', async () => {
        const character = mocks.database!.characters[0]
        const conversation = character.chats[0]
        const onMutation = vi.fn()
        mocks.session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })

        await expect(processMultiCommand('/send hello')).resolves.toBe('')

        expect(conversation.message.map((message) => message.data)).toEqual(['before', 'hello'])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({ commands: ['append'] }))
    })

    it('passes the parent operation through a nested trigger command', async () => {
        const character = mocks.database!.characters[0]
        const chat = character.chats[0]
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 1,
        })
        mocks.session = session
        const operation = createConversationOperationContext(session, chat)
        vi.mocked(setCurrentChat).mockClear()
        vi.mocked(runTrigger).mockResolvedValueOnce({
            chat: operation.chat,
        } as never)
        try {
            await processMultiCommand('/trigger inner', operation)
            expect(runTrigger).toHaveBeenLastCalledWith(character, 'manual', {
                chat: operation.chat,
                manualName: 'inner',
                conversationOperation: operation,
            })
            expect(setCurrentChat).not.toHaveBeenCalled()
        } finally {
            operation.release()
        }
    })

    it('rejects a nested trigger command after selecting another conversation', async () => {
        const character = mocks.database!.characters[0]
        const chat = character.chats[0]
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 1,
        })
        const operation = createConversationOperationContext(session, chat)
        mocks.database!.characters.push(createCharacter('replacement', 'untouched'))
        mocks.selectedCharacterIndex = 1
        vi.mocked(runTrigger).mockClear()
        try {
            await expect(
                processMultiCommand('/trigger inner', operation),
            ).rejects.toThrow(/inactive/i)
            expect(runTrigger).not.toHaveBeenCalled()
        } finally {
            operation.release()
        }
    })

    it('stops fallback multisend when navigation changes during generation', async () => {
        const original = createCharacter('original', 'original before')
        const replacement = createCharacter('replacement', 'replacement before')
        mocks.database = {
            ...mocks.database!,
            characters: [original, replacement],
        }
        mocks.sendChat.mockImplementationOnce(async () => {
            mocks.selectedCharacterIndex = 1
        })

        await expect(processMultiCommand('/multisend first|||second')).resolves.toBe(false)

        expect(original.chats[0].message.map((message) => message.data)).toEqual([
            'original before',
            'first',
        ])
        expect(replacement.chats[0].message.map((message) => message.data)).toEqual([
            'replacement before',
        ])
        expect(mocks.sendChat).toHaveBeenCalledTimes(1)
    })

    it('refreshes the target after each owned session multisend mutation', async () => {
        const character = mocks.database!.characters[0]
        const conversation = character.chats[0]
        mocks.session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })

        await expect(processMultiCommand('/multisend "first|||second"')).resolves.toBe('')

        expect(conversation.message.map((message) => message.data)).toEqual([
            'before',
            '"first',
            'second"',
        ])
        expect(mocks.sendChat).toHaveBeenCalledTimes(2)
    })

    it.each([
        ['/cut 1-3', ['one', 'two']],
        ['/cut 1', ['one']],
        ['/cut duplicate', ['one', 'three']],
        ['/cut missing', ['zero', 'one', 'two', 'three']],
        ['/del 2', ['two', 'three']],
        ['/del 0', []],
    ])('preserves command selection semantics for %s', async (command, expected) => {
        const character = mocks.database!.characters[0]
        const conversation = character.chats[0]
        conversation.message = [
            { role: 'user', data: 'zero', chatId: 'duplicate' },
            { role: 'char', data: 'one' },
            { role: 'user', data: 'two', chatId: 'duplicate' },
            { role: 'char', data: 'three', chatId: 'other' },
        ]
        mocks.session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })

        await processMultiCommand(command)

        expect(conversation.message.map((message) => message.data)).toEqual(expected)
    })

})
