import { describe, expect, it, vi } from 'vitest'

vi.mock('../parser/chatVar.svelte', () => ({
    getChatVarFromConversation: (
        _database: Database,
        _characterId: string,
        chat: Chat,
        key: string,
    ) => chat.scriptstate?.[`$${key}`]?.toString() ?? 'null',
    setChatVarOnConversation: (chat: Chat, key: string, value: string) => {
        chat.scriptstate ??= {}
        chat.scriptstate[`$${key}`] = value
        return true
    },
}))

import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { Chat, Database, Message, character } from '../storage/database.svelte'
import { runCurrentChatParserPass } from './currentChatParserPass'

function fixture(messageCount = 300) {
    const messages = Array.from({ length: messageCount }, (_value, index): Message => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: index === 0 || index === 129 || index === 299
            ? `parse-${index}`
            : `plain-${index}`,
        chatId: `message-${index}`,
    }))
    const chat: Chat = {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: messages,
    }
    const selectedCharacter = {
        type: 'character',
        chaId: 'character-a',
        name: 'Character A',
        chatPage: 0,
        chats: [chat],
        defaultVariables: '',
    } as character
    const database = {
        characters: [selectedCharacter],
        templateDefaultVariables: '',
    } as Database
    const onMutation = vi.fn()
    const session = new ActiveConversationSession({
        characterId: selectedCharacter.chaId,
        conversationId: chat.id!,
        conversation: chat,
        storeRevision: 7,
        onMutation,
    })
    return { chat, database, onMutation, selectedCharacter, session }
}

describe('current chat parser pass', () => {
    it('parses bounded pages and atomically commits ordered variables with disjoint ranges', () => {
        const target = fixture()
        const readRange = vi.spyOn(target.session, 'readRange')
        const acquirePin = vi.spyOn(target.session, 'acquirePin')
        const applyOperation = vi.spyOn(target.session, 'applyOperation')
        const parser = vi.fn((data: string, context: {
            db?: Database
            getChatVar?: (key: string) => string
            setChatVar?: (key: string, value: string) => void
        }) => {
            if (!data.startsWith('parse-')) return data
            const previous = context.getChatVar?.('counter') ?? 'null'
            const next = previous === 'null' ? 1 : Number(previous) + 1
            context.setChatVar?.('counter', String(next))
            if (data === 'parse-129') {
                expect(context.db?.characters[0].chats[0].message[0].data).toBe('parsed-1')
                expect(target.chat.message[0].data).toBe('parse-0')
            }
            return `parsed-${next}`
        })

        const result = runCurrentChatParserPass({
            chat: target.chat,
            database: target.database,
            ownerCharacterId: target.selectedCharacter.chaId,
            parserCharacter: target.selectedCharacter,
            session: target.session,
            parser,
        })

        expect(result).toBe(target.chat)
        expect(parser).toHaveBeenCalledTimes(300)
        expect(readRange.mock.calls.map(([start, limit]) => [start, limit])).toEqual([
            [0, 128],
            [128, 128],
            [256, 44],
        ])
        expect(acquirePin).toHaveBeenCalledOnce()
        expect(acquirePin).toHaveBeenCalledWith('compatibility')
        expect(applyOperation).toHaveBeenCalledOnce()
        expect(applyOperation.mock.calls[0][0].ranges?.map((range) => ({
            start: range.position.absoluteIndex,
            deleteCount: range.deleteCount,
            messages: range.messages.map((message) => message.data),
        }))).toEqual([
            { start: 0, deleteCount: 1, messages: ['parsed-1'] },
            { start: 129, deleteCount: 1, messages: ['parsed-2'] },
            { start: 299, deleteCount: 1, messages: ['parsed-3'] },
        ])
        expect(target.chat.message[0].data).toBe('parsed-1')
        expect(target.chat.message[129].data).toBe('parsed-2')
        expect(target.chat.message[299].data).toBe('parsed-3')
        expect(target.chat.scriptstate).toEqual({ '$counter': '3' })
        expect(target.session.version).toBe(3)
        expect(target.session.activePinReasons).toEqual([])
        expect(target.onMutation).toHaveBeenCalledOnce()
    })

    it('releases the full-operation pin without publishing partial messages or variables on failure', () => {
        const target = fixture()
        const acquirePin = vi.spyOn(target.session, 'acquirePin')
        const parser = vi.fn((data: string, context: {
            setChatVar?: (key: string, value: string) => void
        }) => {
            if (data === 'parse-0') {
                context.setChatVar?.('counter', '1')
                return 'parsed-1'
            }
            if (data === 'parse-129') throw new Error('parser failed')
            return data
        })

        expect(() => runCurrentChatParserPass({
            chat: target.chat,
            database: target.database,
            ownerCharacterId: target.selectedCharacter.chaId,
            parserCharacter: target.selectedCharacter,
            session: target.session,
            parser,
        })).toThrow('parser failed')

        expect(acquirePin).toHaveBeenCalledWith('compatibility')
        expect(target.chat.message[0].data).toBe('parse-0')
        expect(target.chat.scriptstate).toBeUndefined()
        expect(target.session.version).toBe(0)
        expect(target.session.activePinReasons).toEqual([])
        expect(target.onMutation).not.toHaveBeenCalled()
    })

    it('coalesces alternating changes into at most one replacement range per page', () => {
        const target = fixture(256)
        const applyOperation = vi.spyOn(target.session, 'applyOperation')

        runCurrentChatParserPass({
            chat: target.chat,
            database: target.database,
            ownerCharacterId: target.selectedCharacter.chaId,
            parserCharacter: target.selectedCharacter,
            session: target.session,
            parser: (data) => {
                const index = Number(data.split('-').at(-1))
                return index % 2 === 0 ? `changed-${index}` : data
            },
        })

        expect(applyOperation.mock.calls[0][0].ranges?.map((range) => ({
            start: range.position.absoluteIndex,
            deleteCount: range.deleteCount,
        }))).toEqual([
            { start: 0, deleteCount: 127 },
            { start: 128, deleteCount: 127 },
        ])
        expect(target.chat.message[0].data).toBe('changed-0')
        expect(target.chat.message[1].data).toBe('plain-1')
        expect(target.chat.message[254].data).toBe('changed-254')
        expect(target.chat.message[255].data).toBe('plain-255')
    })
})
