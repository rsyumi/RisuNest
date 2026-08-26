import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import {
    captureChatMessageTarget,
    captureChatMessageTargetById,
    editCapturedChatMessage,
    LatestChatScrollRequestGuard,
    navigateCapturedChatMessage,
    renameCapturedBookmark,
    resolveRetainedChatMessageTarget,
    resolveChatMessageTarget,
    saveCapturedChatMessage,
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

    it('keeps the current canonical display after stale full and partial saves', () => {
        const target = fixture([{ role: 'user', data: 'old', chatId: 'target-id' }])
        const fullEdit = capture(target, 0)
        const partialEdit = capture(target, 0)
        target.session.edit(target.session.locate(0), {
            role: 'user',
            data: 'new canonical',
            chatId: 'target-id',
        })
        let fullDisplay = target.conversation.message[0].data
        let partialDisplay = target.conversation.message[0].data

        const fullResult = saveCapturedChatMessage(fullEdit, target, 'full draft')
        if (fullResult.saved) fullDisplay = fullResult.displayData
        const partialResult = saveCapturedChatMessage(partialEdit, target, 'partial draft')
        if (partialResult.saved) partialDisplay = partialResult.displayData

        expect(fullResult).toEqual({ saved: false })
        expect(partialResult).toEqual({ saved: false })
        expect(fullDisplay).toBe('new canonical')
        expect(partialDisplay).toBe('new canonical')
        expect(target.conversation.message[0].data).toBe('new canonical')
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

    it('aborts a fallback bookmark rename when the message ID changes during the prompt', async () => {
        const target = fixture([{
            role: 'char',
            data: 'message',
            chatId: 'before-id',
        }], false)
        target.conversation.bookmarks = ['before-id']
        target.conversation.bookmarkNames = { 'before-id': 'Before' }
        const prompt = deferred<string>()
        const renaming = renameCapturedBookmark(
            capture(target, 0),
            target,
            () => prompt.promise,
        )
        target.conversation.message[0].chatId = 'after-id'
        target.conversation.bookmarks = ['after-id']
        prompt.resolve('Must not apply')

        await expect(renaming).resolves.toBe(false)
        expect(target.conversation.bookmarkNames).toEqual({ 'before-id': 'Before' })
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

    it('clears a retained fold target when its locator becomes stale', () => {
        const target = fixture([{ role: 'user', data: 'fold me' }])
        const retained = { data: capture(target, 0) }
        target.session.edit(target.session.locate(0), { role: 'user', data: 'changed' })

        expect(resolveRetainedChatMessageTarget(retained, target)).toBeNull()
        expect(retained.data).toBeNull()
    })

    it('lets only the latest scroll request apply after overlapping waits', async () => {
        const guard = new LatestChatScrollRequestGuard()
        const firstWait = deferred<void>()
        const secondWait = deferred<void>()
        const applied: string[] = []
        const applyAfter = async (
            wait: Promise<void>,
            request: number,
            value: string,
        ) => {
            await wait
            if (guard.isCurrent(request)) applied.push(value)
        }
        const first = guard.begin()
        const firstCompletion = applyAfter(firstWait.promise, first, 'first')
        const second = guard.begin()
        const secondCompletion = applyAfter(secondWait.promise, second, 'second')

        secondWait.resolve()
        await secondCompletion
        firstWait.resolve()
        await firstCompletion

        expect(guard.isCurrent(first)).toBe(false)
        expect(guard.isCurrent(second)).toBe(true)
        expect(applied).toEqual(['second'])
    })

    it('navigates a captured locator through the bounded viewport without retargeting it', async () => {
        const target = fixture(Array.from({ length: 200 }, (_, index) => ({
            role: index % 2 === 0 ? 'char' : 'user',
            data: `message-${index}`,
            chatId: `id-${index}`,
        } as Message)))
        const captured = capture(target, 17)
        const guard = new LatestChatScrollRequestGuard()
        const viewport = { jumpTo: vi.fn().mockResolvedValue(true) }

        await expect(navigateCapturedChatMessage({
            target: captured,
            context: target,
            guard,
            requestGeneration: guard.begin(),
            viewport,
        })).resolves.toBe(true)
        expect(viewport.jumpTo).toHaveBeenCalledWith(17, { align: 'start', highlight: true })

        const stale = capture(target, 18)
        target.session.edit(target.session.locate(18), { role: 'char', data: 'changed' })
        await expect(navigateCapturedChatMessage({
            target: stale,
            context: target,
            guard,
            requestGeneration: guard.begin(),
            viewport,
        })).resolves.toBe(false)
        expect(viewport.jumpTo).toHaveBeenCalledTimes(1)
    })
})
