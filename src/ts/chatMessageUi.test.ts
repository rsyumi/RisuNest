import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import {
    captureChatMessageTarget,
    captureChatMessageTargetById,
    editCapturedChatMessage,
    renameCapturedBookmark,
    resolveChatMessageTarget,
    toggleCapturedBookmark,
    toggleCapturedMessageDisabled,
    toggleCapturedMessageRole,
} from './chatMessageUi'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function fixture(messages: Message[], withSession = true) {
    const conversation = {
        id: 'chat-a',
        name: 'Chat A',
        note: '',
        localLore: [],
        message: messages,
    } as Chat
    const character = {
        type: 'character',
        chaId: 'character-a',
        chatPage: 0,
        chats: [conversation],
    } as Database['characters'][number]
    const onMutation = vi.fn()
    const session = new ActiveConversationSession({
        characterId: 'character-a',
        conversationId: 'chat-a',
        conversation,
        storeRevision: 1,
        onMutation,
    })
    let current = { character, conversation }
    let currentSession: ActiveConversationSession | null = withSession ? session : null
    const captureCurrent = () => current
    const getCurrentSession = () => currentSession
    return {
        character,
        conversation,
        session,
        onMutation,
        captureCurrent,
        getCurrentSession,
        navigateTo(next: ReturnType<typeof fixture>) {
            current = {
                character: next.character,
                conversation: next.conversation,
            }
            currentSession = next.session
        },
    }
}

function capture(target: ReturnType<typeof fixture>, absoluteIndex: number) {
    return captureChatMessageTarget({
        absoluteIndex,
        captureCurrent: target.captureCurrent,
        getCurrentSession: target.getCurrentSession,
    })!
}

describe('chat message UI targets', () => {
    it('routes role and both disabled-state toggles through session edits', () => {
        const target = fixture([{ role: 'char', data: 'message', disabled: false }])

        expect(toggleCapturedMessageRole(capture(target, 0), target)).toBe(true)
        expect(target.conversation.message[0].role).toBe('user')
        expect(toggleCapturedMessageDisabled(capture(target, 0), target, 'message')).toBe(true)
        expect(target.conversation.message[0].disabled).toBe(true)
        expect(toggleCapturedMessageDisabled(capture(target, 0), target, 'allBefore')).toBe(true)
        expect(target.conversation.message[0].disabled).toBe('allBefore')
        expect(target.onMutation.mock.calls.map(([event]) => event.commands)).toEqual([
            ['edit'],
            ['edit'],
            ['edit'],
        ])
    })

    it('does not retarget two captured edits around an insertion', () => {
        const target = fixture([
            { role: 'user', data: 'zero' },
            { role: 'char', data: 'one' },
            { role: 'user', data: 'two' },
        ])
        const firstEdit = capture(target, 0)
        const secondEdit = capture(target, 2)
        target.conversation.message.splice(1, 0, { role: 'char', data: 'inserted' })

        expect(editCapturedChatMessage(firstEdit, target, 'edited zero')).toBe(true)
        expect(editCapturedChatMessage(secondEdit, target, 'must not retarget')).toBe(false)
        expect(target.conversation.message.map((message) => message.data)).toEqual([
            'edited zero',
            'inserted',
            'one',
            'two',
        ])
    })

    it('preserves missing and duplicate IDs when bookmarks use the current last-match policy', async () => {
        const target = fixture([
            { role: 'user', data: 'first duplicate', chatId: 'duplicate' },
            { role: 'char', data: 'missing ID' },
            { role: 'user', data: 'last duplicate', chatId: 'duplicate' },
        ])
        target.conversation.bookmarks = ['duplicate']
        target.conversation.bookmarkNames = { duplicate: 'Duplicate' }

        expect(captureChatMessageTargetById(
            target,
            'duplicate',
            'last',
        )?.absoluteIndex).toBe(2)

        expect(await toggleCapturedBookmark(capture(target, 2), target, {
            requestName: vi.fn(),
            createMessageId: () => 'unused',
            defaultName: () => 'unused',
        })).toBe(true)
        expect(target.conversation.bookmarks).toEqual([])

        expect(await toggleCapturedBookmark(capture(target, 1), target, {
            requestName: async () => '',
            createMessageId: () => 'assigned',
            defaultName: () => 'Default name',
        })).toBe(true)
        expect(target.conversation.message[1].chatId).toBe('assigned')
        expect(target.conversation.bookmarks).toEqual(['assigned'])
        expect(target.conversation.bookmarkNames).toEqual({ assigned: 'Default name' })
    })

    it('keeps the direct-array fallback narrow and identity-safe across a bookmark prompt', async () => {
        const original = fixture([{ role: 'char', data: 'original' }], false)
        const replacement = fixture([{ role: 'user', data: 'replacement' }], false)
        const prompt = deferred<string>()
        const toggling = toggleCapturedBookmark(capture(original, 0), original, {
            requestName: () => prompt.promise,
            createMessageId: () => 'assigned',
            defaultName: () => 'Default',
        })
        original.navigateTo(replacement)
        prompt.resolve('Original bookmark')

        await expect(toggling).resolves.toBe(false)
        expect(original.conversation.message[0].chatId).toBeUndefined()
        expect(original.conversation.bookmarks).toBeUndefined()
        expect(replacement.conversation.bookmarks).toBeUndefined()
    })

    it('aborts bookmark assignment and rename when navigation changes during prompts', async () => {
        const original = fixture([{ role: 'char', data: 'original', chatId: 'original-id' }])
        const replacement = fixture([{ role: 'char', data: 'replacement', chatId: 'replacement-id' }])
        const bookmarkPrompt = deferred<string>()
        const bookmark = toggleCapturedBookmark(capture(original, 0), original, {
            requestName: () => bookmarkPrompt.promise,
            createMessageId: () => 'unused',
            defaultName: () => 'Default name',
        })
        original.navigateTo(replacement)
        bookmarkPrompt.resolve('Old target')

        await expect(bookmark).resolves.toBe(false)
        expect(original.conversation.bookmarks).toBeUndefined()
        expect(replacement.conversation.bookmarks).toBeUndefined()

        replacement.conversation.bookmarks = ['replacement-id']
        replacement.conversation.bookmarkNames = { 'replacement-id': 'Before' }
        const renamePrompt = deferred<string>()
        const rename = renameCapturedBookmark(capture(replacement, 0), replacement, () => renamePrompt.promise)
        replacement.navigateTo(fixture([{ role: 'user', data: 'later' }]))
        renamePrompt.resolve('After')

        await expect(rename).resolves.toBe(false)
        expect(replacement.conversation.bookmarkNames).toEqual({ 'replacement-id': 'Before' })
    })

    it('rejects stale scroll, fold, and bookmark targets without changing canonical output', () => {
        const target = fixture([
            { role: 'user', data: 'zero', chatId: 'duplicate' },
            { role: 'char', data: 'one' },
            { role: 'user', data: 'two', chatId: 'duplicate' },
        ])
        const scrollTarget = capture(target, 0)
        const foldTarget = capture(target, 1)
        const bookmarkTarget = capture(target, 2)
        target.session.edit(target.session.locate(1), { role: 'char', data: 'edited one' })

        expect(resolveChatMessageTarget(scrollTarget, target)).toBeNull()
        expect(resolveChatMessageTarget(foldTarget, target)).toBeNull()
        expect(resolveChatMessageTarget(bookmarkTarget, target)).toBeNull()
        expect(target.conversation).toEqual(expect.objectContaining({
            message: [
                { role: 'user', data: 'zero', chatId: 'duplicate' },
                { role: 'char', data: 'edited one' },
                { role: 'user', data: 'two', chatId: 'duplicate' },
            ],
        }))
    })
})
