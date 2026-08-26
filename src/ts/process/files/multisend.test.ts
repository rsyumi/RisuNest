import { beforeEach, describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from '../../storage/activeConversationSession'
import type { Chat, Database } from '../../storage/database.svelte'

const mocks = vi.hoisted(() => ({
    dbState: { db: null as Database | null },
    selectedCharacterIndex: 0,
    session: null as ActiveConversationSession | null,
    sendChat: vi.fn(async () => undefined),
    downloadFile: vi.fn(async () => undefined),
}))

vi.mock('src/ts/stores.svelte', () => ({
    DBState: mocks.dbState,
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(mocks.selectedCharacterIndex)
            return () => undefined
        },
    },
}))
vi.mock('../index.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        doingChat: writable(false),
        sendChat: mocks.sendChat,
    }
})
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: mocks.downloadFile }))
vi.mock('src/ts/platform', () => ({ isTauri: false }))
vi.mock('../memory/hypamemory', () => ({
    HypaProcesser: class {
        addText() {}
        async similaritySearch() { return [] }
    },
}))
vi.mock('src/ts/util', () => ({
    BufferToText: (value: Uint8Array) => new TextDecoder().decode(value),
    selectMultipleFile: vi.fn(),
}))
vi.mock('./inlays', () => ({ postInlayAsset: vi.fn() }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => mocks.session,
}))

import { postChatFile } from './multisend'

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

describe('postChatFile PO append', () => {
    beforeEach(() => {
        mocks.dbState.db = createDatabase()
        mocks.selectedCharacterIndex = 0
        mocks.session = null
        mocks.sendChat.mockClear()
        mocks.downloadFile.mockClear()
    })

    it('routes each PO user message through the matching session', async () => {
        const character = mocks.dbState.db!.characters[0]
        const conversation = character.chats[0]
        const onMutation = vi.fn()
        mocks.session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
            onMutation,
        })
        const input = new TextEncoder().encode('msgid "hello"\nmsgstr ""\n\n')

        await expect(postChatFile({ name: 'input.po', data: input })).resolves.toEqual([
            { type: 'void' },
        ])

        expect(conversation.message.map((message) => message.data)).toEqual(['before', 'hello'])
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({ commands: ['append'] }))
        expect(mocks.sendChat).toHaveBeenCalledTimes(1)
    })
})
