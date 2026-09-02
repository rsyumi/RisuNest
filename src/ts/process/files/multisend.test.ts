import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

import { ActiveConversationSession } from '../../storage/activeConversationSession'
import type { Chat, Database } from '../../storage/database.svelte'

const mocks = vi.hoisted(() => ({
    dbState: { db: null as Database | null },
    selectedCharacterIndex: 0,
    session: null as ActiveConversationSession | null,
    sendChat: vi.fn(async () => undefined),
    downloadFile: vi.fn(async () => undefined),
    selectedTarget: null as any,
    acquireCompleteConversation: vi.fn(),
    postInlayAsset: vi.fn(),
    UnsupportedAnimatedInlayError: class UnsupportedAnimatedInlayError extends Error {},
    alertError: vi.fn(),
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
    const { doingChat } = await import('../generationState')
    return {
        doingChat,
        sendChat: mocks.sendChat,
    }
})
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: mocks.downloadFile }))
vi.mock('src/lang', () => ({
    language: { risuNest: { inlay: { unsupportedAnimated: 'unsupported' } } },
}))
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
vi.mock('./inlays', () => ({
    postInlayAsset: mocks.postInlayAsset,
    UnsupportedAnimatedInlayError: mocks.UnsupportedAnimatedInlayError,
}))
vi.mock('src/ts/alert', () => ({ alertError: mocks.alertError }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    getActiveConversationSession: () => mocks.session,
    capturePersistentMutationToken: () => undefined,
    acquireDestructiveReplacementFence: () => undefined,
}))

import { postChatFile } from './multisend'
import { UnsupportedAnimatedInlayError } from './inlays'
import { doingChat, reserveGeneration } from '../generationState'

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
        mocks.selectedTarget = null
        mocks.acquireCompleteConversation.mockReset()
        mocks.postInlayAsset.mockReset()
        mocks.alertError.mockReset()
        doingChat.set(false)
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

    it('refreshes the target between owned PO session appends', async () => {
        const character = mocks.dbState.db!.characters[0]
        const conversation = character.chats[0]
        mocks.session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        const input = new TextEncoder().encode(
            'msgid "first"\nmsgstr ""\n\nmsgid "second"\nmsgstr ""\n\n',
        )

        await postChatFile({ name: 'input.po', data: input })

        expect(conversation.message.map((message) => message.data)).toEqual([
            'before',
            'first',
            'second',
        ])
        expect(mocks.sendChat).toHaveBeenCalledTimes(2)
    })

    it('promotes before the first PO append and holds the exact lease across generation', async () => {
        const character = mocks.dbState.db!.characters[0]
        const conversation = character.chats[0]
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id,
            conversation,
            storeRevision: 1,
        })
        mocks.session = session
        const target = { characterId: character.chaId, conversationId: conversation.id }
        mocks.selectedTarget = target
        let resolvePromotion!: (lease: any) => void
        const promotion = new Promise<any>((resolve) => { resolvePromotion = resolve })
        mocks.acquireCompleteConversation.mockReturnValue(promotion)
        let resolveGeneration!: () => void
        mocks.sendChat.mockImplementationOnce(() => new Promise<void>((resolve) => {
            resolveGeneration = resolve
        }))
        let releaseCount = 0
        const input = new TextEncoder().encode('msgid "hello"\nmsgstr ""\n\n')

        const posting = postChatFile({ name: 'input.po', data: input })
        await Promise.resolve()

        expect(conversation.message.map((message) => message.data)).toEqual(['before'])
        expect(mocks.sendChat).not.toHaveBeenCalled()

        const pin = session.acquirePin('compatibility')
        resolvePromotion({
            session,
            target,
            release() {
                releaseCount += 1
                pin.release()
            },
        })
        while (mocks.sendChat.mock.calls.length === 0) await Promise.resolve()

        expect(conversation.message.map((message) => message.data)).toEqual(['before', 'hello'])
        expect(session.pinCount('compatibility')).toBe(1)

        resolveGeneration()
        await expect(posting).resolves.toEqual([{ type: 'void' }])
        expect(releaseCount).toBe(1)
        expect(session.pinCount('compatibility')).toBe(0)
    })

    it('aborts a PO append when the promoted lease does not own the selected conversation', async () => {
        const character = mocks.dbState.db!.characters[0]
        const conversation = character.chats[0]
        const otherConversation = { ...conversation, id: 'chat-b', message: [] } as Chat
        const otherSession = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: otherConversation.id,
            conversation: otherConversation,
            storeRevision: 1,
        })
        const target = { characterId: character.chaId, conversationId: conversation.id }
        mocks.selectedTarget = target
        let releaseCount = 0
        mocks.acquireCompleteConversation.mockResolvedValue({
            session: otherSession,
            target,
            release() { releaseCount += 1 },
        })
        const input = new TextEncoder().encode('msgid "hello"\nmsgstr ""\n\n')

        await expect(postChatFile({ name: 'input.po', data: input })).resolves.toEqual([
            { type: 'void' },
        ])

        expect(conversation.message.map((message) => message.data)).toEqual(['before'])
        expect(mocks.sendChat).not.toHaveBeenCalled()
        expect(releaseCount).toBe(1)
    })

    it('does not cancel a provider-waiting generation or append when PO overlaps it', async () => {
        const conversation = mocks.dbState.db!.characters[0].chats[0]
        const providerGeneration = reserveGeneration()
        expect(providerGeneration).not.toBeNull()
        doingChat.set(false)
        const input = new TextEncoder().encode('msgid "overlap"\nmsgstr ""\n\n')

        try {
            await expect(postChatFile({ name: 'input.po', data: input })).resolves.toEqual([
                { type: 'void' },
            ])

            expect(providerGeneration!.isCurrent()).toBe(true)
            expect(get(doingChat)).toBe(true)
            expect(conversation.message.map((message) => message.data)).toEqual(['before'])
            expect(mocks.acquireCompleteConversation).not.toHaveBeenCalled()
            expect(mocks.sendChat).not.toHaveBeenCalled()
        } finally {
            providerGeneration?.release()
        }
    })
})

describe('postChatFile attachment errors', () => {
    beforeEach(() => {
        mocks.dbState.db = createDatabase()
        mocks.sendChat.mockClear()
        mocks.postInlayAsset.mockReset()
        mocks.alertError.mockReset()
    })

    it.each(['gif', 'avif'])('skips typed unsupported %s attachments without appending an asset or message', async (extension) => {
        mocks.postInlayAsset.mockRejectedValueOnce(new UnsupportedAnimatedInlayError('unsupported animation'))
        const conversation = mocks.dbState.db!.characters[0].chats[0]

        await expect(postChatFile({ name: `animated.${extension}`, data: new Uint8Array([1]) })).resolves.toEqual([])

        expect(mocks.alertError).toHaveBeenCalledWith('unsupported')
        expect(conversation.message).toEqual([{ role: 'char', data: 'before' }])
        expect(mocks.sendChat).not.toHaveBeenCalled()
    })

    it.each(['png', 'mp3'])('propagates ordinary %s attachment failures', async (extension) => {
        const failure = new Error('storage failed')
        mocks.postInlayAsset.mockRejectedValueOnce(failure)

        await expect(postChatFile({ name: `attachment.${extension}`, data: new Uint8Array([1]) }))
            .rejects.toBe(failure)

        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('keeps null unsupported-extension results as a silent skip', async () => {
        mocks.postInlayAsset.mockResolvedValueOnce(null)

        await expect(postChatFile({ name: 'attachment.avi', data: new Uint8Array([1]) }))
            .resolves.toEqual([])

        expect(mocks.alertError).not.toHaveBeenCalled()
    })
})
