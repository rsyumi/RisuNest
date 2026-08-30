import { describe, expect, it, vi } from 'vitest'

import { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import {
    captureChatMessageTarget,
    captureChatMessageTargetById,
    captureChatMessageTargetsByIds,
    editCapturedChatMessage,
    LatestChatScrollRequestGuard,
    navigateCapturedChatMessage,
    queryChatMessageTargetAt,
    queryChatMessageTargetById,
    queryChatMessageTargetsByIds,
    removeCapturedBookmark,
    renameCapturedBookmark,
    resolveRetainedChatMessageTarget,
    resolveChatMessageTarget,
    saveCapturedChatMessage,
    toggleCapturedBookmark,
    toggleCapturedMessageDisabled,
    toggleCapturedMessageRole,
} from './chatMessageUi'
import { createMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'

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
    it('uses the last far duplicate for bookmark display and navigation', async () => {
        const completeMessages = Array.from({ length: 10_000 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' : 'char',
            data: `message-${index}`,
            chatId: `id-${index}`,
        } as Message))
        completeMessages[10].chatId = 'duplicate'
        completeMessages[9000].chatId = 'duplicate'
        const complete = fixture(completeMessages)
        const shell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = shell
        const selection = {
            characterId: complete.character.chaId,
            conversationId: shell.id,
            navigationGeneration: 3,
            storeRevision: 7,
        } as any
        const release = vi.fn()
        const readConversationWindow = vi.fn(async ({
            anchorMessageId,
            anchorOccurrence,
            startIndex,
            limit,
        }: any) => {
            limit ??= 1
            let absoluteIndex = startIndex
            if (anchorMessageId !== undefined) {
                absoluteIndex = anchorOccurrence === 'last'
                    ? completeMessages.findLastIndex((message) => message.chatId === anchorMessageId)
                    : completeMessages.findIndex((message) => message.chatId === anchorMessageId)
                if (absoluteIndex === -1) return null
            } else if (startIndex === undefined) {
                absoluteIndex = Math.max(0, completeMessages.length - limit)
            }
            if (startIndex === 8888) absoluteIndex = 8887
            const messages = completeMessages.slice(absoluteIndex, absoluteIndex + limit)
            return {
                revision: 7,
                value: {
                    characterId: complete.character.chaId,
                    conversationId: shell.id,
                    startIndex: absoluteIndex,
                    endIndex: absoluteIndex + messages.length,
                    totalMessages: 10_000,
                    messages,
                    hasMoreBefore: absoluteIndex > 0,
                    hasMoreAfter: absoluteIndex + messages.length < 10_000,
                },
            }
        })
        const context = {
            captureCurrent: () => ({ character: complete.character, conversation: shell }),
            getCurrentSession: () => null,
            captureSelectedConversationTarget: () => selection,
            acquirePersistentRevision: vi.fn(async () => ({
                revision: 7,
                readConversationWindow,
                release,
            })),
            acquireCompleteConversation: vi.fn(),
        }

        const targets = await queryChatMessageTargetsByIds(
            context as any,
            ['id-9123', 'id-42'],
            'first',
        )

        expect(targets.map((target) => [target.absoluteIndex, target.message.chatId])).toEqual([
            [9123, 'id-9123'],
            [42, 'id-42'],
        ])
        expect(context.acquirePersistentRevision).toHaveBeenCalledWith(7)
        expect(readConversationWindow).toHaveBeenCalledTimes(2)
        const indexed = await queryChatMessageTargetAt(context as any, 7777)
        expect(indexed).toMatchObject({
            absoluteIndex: 7777,
            message: { chatId: 'id-7777', data: 'message-7777' },
        })
        expect(readConversationWindow).toHaveBeenCalledTimes(3)
        expect(release).toHaveBeenCalledTimes(2)
        await expect(queryChatMessageTargetAt(context as any, 8888)).resolves.toBeNull()
        expect(release).toHaveBeenCalledTimes(3)

        readConversationWindow.mockClear()
        const duplicates = await queryChatMessageTargetsByIds(
            context as any,
            ['duplicate', 'duplicate'],
            'last',
        )
        expect(duplicates).toHaveLength(2)
        expect(duplicates[0]).toMatchObject({
            absoluteIndex: 9000,
            message: { chatId: 'duplicate', data: 'message-9000' },
        })
        expect(duplicates[1]).toBe(duplicates[0])
        expect(readConversationWindow).toHaveBeenCalledOnce()
        expect(readConversationWindow).toHaveBeenCalledWith({
            characterId: complete.character.chaId,
            conversationId: shell.id,
            anchorMessageId: 'duplicate',
            anchorOccurrence: 'last',
            before: 0,
            after: 0,
        })

        readConversationWindow.mockClear()
        await expect(queryChatMessageTargetById(context as any, 'absent', 'last'))
            .resolves.toBeNull()
        expect(readConversationWindow).toHaveBeenCalledOnce()
        expect(readConversationWindow.mock.calls[0][0]).not.toHaveProperty('startIndex')
    })

    it('rejects every anchored result when selection changes during a multi-ID query', async () => {
        const complete = fixture([
            { role: 'user', data: 'first', chatId: 'first-id' },
            { role: 'char', data: 'second', chatId: 'second-id' },
        ])
        const shell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = shell
        const initialSelection = {
            characterId: complete.character.chaId,
            conversationId: shell.id,
            navigationGeneration: 1,
            storeRevision: 4,
        } as any
        const laterSelection = { ...initialSelection, navigationGeneration: 2 } as any
        let selection = initialSelection
        const secondRead = deferred<any>()
        const release = vi.fn()
        const windowFor = (absoluteIndex: number, message: Message) => ({
            revision: 4,
            value: {
                characterId: complete.character.chaId,
                conversationId: shell.id,
                startIndex: absoluteIndex,
                endIndex: absoluteIndex + 1,
                totalMessages: 2,
                messages: [message],
                hasMoreBefore: absoluteIndex > 0,
                hasMoreAfter: absoluteIndex < 1,
            },
        })
        const readConversationWindow = vi.fn(({ anchorMessageId }: any) =>
            anchorMessageId === 'first-id'
                ? Promise.resolve(windowFor(0, complete.conversation.message[0]))
                : secondRead.promise,
        )
        const context = {
            captureCurrent: () => ({ character: complete.character, conversation: shell }),
            getCurrentSession: () => null,
            captureSelectedConversationTarget: () => selection,
            acquirePersistentRevision: vi.fn(async () => ({
                revision: 4,
                readConversationWindow,
                release,
            })),
            acquireCompleteConversation: vi.fn(),
        }

        const pending = queryChatMessageTargetsByIds(
            context as any,
            ['first-id', 'second-id'],
        )
        await vi.waitFor(() => expect(readConversationWindow).toHaveBeenCalledTimes(2))
        selection = laterSelection
        secondRead.resolve(windowFor(1, complete.conversation.message[1]))

        await expect(pending).resolves.toEqual([])
        expect(release).toHaveBeenCalledOnce()
    })

    it('rejects mismatched pinned revision evidence and always disposes the lease', async () => {
        const complete = fixture([{ role: 'user', data: 'far', chatId: 'far-id' }])
        const shell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = shell
        const selection = {
            characterId: complete.character.chaId,
            conversationId: shell.id,
            navigationGeneration: 1,
            storeRevision: 4,
        } as any
        const release = vi.fn()
        const context = {
            captureCurrent: () => ({ character: complete.character, conversation: shell }),
            getCurrentSession: () => null,
            captureSelectedConversationTarget: () => selection,
            acquirePersistentRevision: vi.fn(async () => ({
                revision: 4,
                readConversationWindow: async () => ({ revision: 5, value: {} }),
                release,
            })),
            acquireCompleteConversation: vi.fn(),
        }

        await expect(queryChatMessageTargetById(context as any, 'far-id')).rejects.toThrow(
            'mismatched revision',
        )
        expect(release).toHaveBeenCalledOnce()
    })

    it('rejects ID and index results when selection changes during revision release', async () => {
        const complete = fixture([{ role: 'user', data: 'message', chatId: 'message-id' }])
        const shell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = shell
        const initialSelection = {
            characterId: complete.character.chaId,
            conversationId: shell.id,
            navigationGeneration: 1,
            storeRevision: 4,
        } as any
        const laterSelection = { ...initialSelection, navigationGeneration: 2 } as any
        const window = {
            revision: 4,
            value: {
                characterId: complete.character.chaId,
                conversationId: shell.id,
                startIndex: 0,
                endIndex: 1,
                totalMessages: 1,
                messages: [{ role: 'user', data: 'message', chatId: 'message-id' }],
                hasMoreBefore: false,
                hasMoreAfter: false,
            },
        }

        for (const query of [
            (context: any) => queryChatMessageTargetById(context, 'message-id'),
            (context: any) => queryChatMessageTargetAt(context, 0),
        ]) {
            let selection = initialSelection
            const releasing = deferred<void>()
            const release = vi.fn(() => releasing.promise)
            const context = {
                captureCurrent: () => ({ character: complete.character, conversation: shell }),
                getCurrentSession: () => null,
                captureSelectedConversationTarget: () => selection,
                acquirePersistentRevision: vi.fn(async () => ({
                    revision: 4,
                    readConversationWindow: async () => window,
                    release,
                })),
                acquireCompleteConversation: vi.fn(),
            }

            const pending = query(context)
            await vi.waitFor(() => expect(release).toHaveBeenCalledOnce())
            selection = laterSelection
            releasing.resolve()

            await expect(pending).resolves.toBeNull()
        }
    })

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

    it('uses the same empty message ID lookup semantics with and without an active session', () => {
        const messages = [
            { role: 'user', data: 'empty ID', chatId: '' },
            { role: 'char', data: 'named ID', chatId: 'named' },
        ] as Message[]
        const sessionTarget = fixture(structuredClone(messages))
        const legacyTarget = fixture(structuredClone(messages), false)

        expect(captureChatMessageTargetById(sessionTarget, '')?.absoluteIndex).toBe(0)
        expect(captureChatMessageTargetById(legacyTarget, '')?.absoluteIndex).toBe(0)
    })

    it('resolves bookmark targets through bounded session pages without retaining full history', () => {
        const messages = Array.from({ length: 300 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' : 'char',
            data: `message-${index}`,
            chatId: `id-${index}`,
        } as Message))
        messages[10].chatId = 'duplicate'
        messages[280].chatId = 'duplicate'
        const target = fixture(messages)
        const sharedLocator = target.session.locate(1)
        const findTargets = vi.spyOn(target.session, 'findMessageTargetsByIds')

        const captured = captureChatMessageTargetsByIds(
            target,
            ['id-129', 'missing', 'duplicate', 'id-0'],
            'last',
        )

        expect(captured.map((entry) => ({
            id: entry.message.chatId,
            index: entry.absoluteIndex,
            data: entry.message.data,
        }))).toEqual([
            { id: 'id-129', index: 129, data: 'message-129' },
            { id: 'duplicate', index: 280, data: 'message-280' },
            { id: 'id-0', index: 0, data: 'message-0' },
        ])
        expect(findTargets).toHaveBeenCalledWith(
            ['id-129', 'missing', 'duplicate', 'id-0'],
            'last',
        )
        expect(target.session.ownsMessageLocator(sharedLocator)).toBe(true)
        expect(captured.every(
            (entry) => entry.locator && target.session.ownsMessageLocator(entry.locator),
        )).toBe(true)
        expect(captured.every((entry) => !('messages' in entry))).toBe(true)
    })

    it('refreshes a session target after a compatibility path mutates the live message in place', () => {
        const target = fixture([{
            role: 'char',
            data: 'captured',
            chatId: 'message-id',
            name: 'old name',
        }])
        const captured = capture(target, 0)
        target.conversation.message[0].data = 'latest'
        target.conversation.message[0].name = 'new name'

        const resolved = resolveChatMessageTarget(captured, target)

        expect(resolved?.message).toMatchObject({
            data: 'latest',
            name: 'new name',
        })
        expect(captured.message).toMatchObject({
            data: 'captured',
            name: 'old name',
        })
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

    it('holds short complete leases for persistent bookmark rename and removal', async () => {
        const complete = fixture([{ role: 'user', data: 'far', chatId: 'far-id' }])
        complete.conversation.bookmarks = ['far-id']
        complete.conversation.bookmarkNames = { 'far-id': 'Before' }
        const shell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = shell
        const initialSelection = {
            characterId: complete.character.chaId,
            conversationId: shell.id,
            navigationGeneration: 1,
            storeRevision: 4,
        } as any
        const completeSelection = { ...initialSelection, storeRevision: 4 } as any
        let currentConversation: Chat = shell
        let currentSession: ActiveConversationSession | null = null
        let selection = initialSelection
        const persistentRelease = vi.fn()
        const renameRelease = vi.fn()
        const removeRelease = vi.fn()
        const acquireCompleteConversation = vi.fn(async (reason: string) => {
            currentConversation = complete.conversation
            complete.character.chats[0] = complete.conversation
            currentSession = complete.session
            selection = completeSelection
            return {
                reason,
                session: complete.session,
                target: completeSelection,
                release: reason === 'rename-bookmark' ? renameRelease : removeRelease,
            }
        })
        const context = {
            captureCurrent: () => ({
                character: complete.character,
                conversation: currentConversation,
            }),
            getCurrentSession: () => currentSession,
            captureSelectedConversationTarget: () => selection,
            acquirePersistentRevision: vi.fn(async () => ({
                revision: 4,
                readConversationWindow: async () => ({
                    revision: 4,
                    value: {
                        characterId: complete.character.chaId,
                        conversationId: shell.id,
                        startIndex: 0,
                        endIndex: 1,
                        totalMessages: 1,
                        messages: [{ role: 'user', data: 'far', chatId: 'far-id' }],
                        hasMoreBefore: false,
                        hasMoreAfter: false,
                    },
                }),
                release: persistentRelease,
            })),
            acquireCompleteConversation,
        }
        const target = await queryChatMessageTargetById(context as any, 'far-id')
        expect(target).not.toBeNull()

        await expect(renameCapturedBookmark(target!, context, async () => 'After')).resolves.toBe(true)
        expect(complete.conversation.bookmarkNames).toEqual({ 'far-id': 'After' })
        expect(renameRelease).toHaveBeenCalledOnce()

        const updatedShell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = updatedShell
        currentConversation = updatedShell
        currentSession = null
        selection = initialSelection
        const removalTarget = await queryChatMessageTargetById(context as any, 'far-id')

        await expect(removeCapturedBookmark(removalTarget!, context)).resolves.toBe(true)
        expect(complete.conversation.bookmarks).toEqual([])
        expect(removeRelease).toHaveBeenCalledOnce()
        expect(persistentRelease).toHaveBeenCalledTimes(2)
    })

    it('holds short complete leases for persistent bookmark toggling in both directions', async () => {
        const complete = fixture([{ role: 'user', data: 'far', chatId: 'far-id' }])
        complete.conversation.bookmarks = ['far-id']
        complete.conversation.bookmarkNames = { 'far-id': 'Before' }
        const shell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = shell
        const initialSelection = {
            characterId: complete.character.chaId,
            conversationId: shell.id,
            navigationGeneration: 1,
            storeRevision: 4,
        } as any
        let currentConversation: Chat = shell
        let currentSession: ActiveConversationSession | null = null
        let selection = initialSelection
        const toggleRelease = vi.fn()
        const acquireCompleteConversation = vi.fn(async (reason: string) => {
            currentConversation = complete.conversation
            complete.character.chats[0] = complete.conversation
            currentSession = complete.session
            return {
                reason,
                session: complete.session,
                target: selection,
                release: toggleRelease,
            }
        })
        const context = {
            captureCurrent: () => ({
                character: complete.character,
                conversation: currentConversation,
            }),
            getCurrentSession: () => currentSession,
            captureSelectedConversationTarget: () => selection,
            acquirePersistentRevision: vi.fn(async () => ({
                revision: 4,
                readConversationWindow: async () => ({
                    revision: 4,
                    value: {
                        characterId: complete.character.chaId,
                        conversationId: shell.id,
                        startIndex: 0,
                        endIndex: 1,
                        totalMessages: 1,
                        messages: [{ role: 'user', data: 'far', chatId: 'far-id' }],
                        hasMoreBefore: false,
                        hasMoreAfter: false,
                    },
                }),
                release: vi.fn(),
            })),
            acquireCompleteConversation,
        }
        const toggleOptions = {
            requestName: async () => 'Toggled',
            createMessageId: () => 'unused-created-id',
            defaultName: () => 'Default',
        }
        const offTarget = await queryChatMessageTargetById(context as any, 'far-id')
        expect(offTarget?.kind).toBe('persistent')

        await expect(toggleCapturedBookmark(offTarget!, context, toggleOptions)).resolves.toBe(true)
        expect(acquireCompleteConversation).toHaveBeenCalledWith('toggle-bookmark', selection)
        expect(complete.conversation.bookmarks).toEqual([])
        expect(toggleRelease).toHaveBeenCalledOnce()

        const updatedShell = createMetadataOnlySelectedConversation(complete.conversation)
        complete.character.chats[0] = updatedShell
        currentConversation = updatedShell
        currentSession = null
        const onTarget = await queryChatMessageTargetById(context as any, 'far-id')
        expect(onTarget?.kind).toBe('persistent')

        await expect(toggleCapturedBookmark(onTarget!, context, toggleOptions)).resolves.toBe(true)
        expect(complete.conversation.bookmarks).toEqual(['far-id'])
        expect(complete.conversation.bookmarkNames).toEqual({ 'far-id': 'Toggled' })
        expect(acquireCompleteConversation).toHaveBeenCalledTimes(2)
        expect(toggleRelease).toHaveBeenCalledTimes(2)
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
