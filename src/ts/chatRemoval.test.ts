import { describe, expect, it, vi } from 'vitest'

import type { Chat, Database, Message } from './storage/database.svelte'
import { removeChatMessage } from './chatRemoval'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function currentTarget(id: string) {
    const messages: Message[] = [
        { role: 'user', data: `${id}-zero`, chatId: `${id}-zero` },
        { role: 'char', data: `${id}-one`, chatId: `${id}-one` },
    ]
    const conversation = {
        id: `${id}-chat`,
        message: messages,
    } as Chat
    const character = {
        type: 'character',
        chaId: `${id}-character`,
        chatPage: 0,
        chats: [conversation],
    } as Database['characters'][number]
    return { character, conversation }
}

describe('removeChatMessage', () => {
    it('does not fall back to the absolute index when a viewport target is unavailable', async () => {
        const current = currentTarget('current')

        await expect(removeChatMessage({
            absoluteIndex: 0,
            captureTarget: () => null,
            shiftKey: false,
            recursive: false,
            askRemoval: false,
            instantRemove: false,
            captureCurrent: () => current,
            getCurrentSession: () => null,
            confirmRemoval: vi.fn(async () => true),
            confirmInstantRemoval: vi.fn(async () => true),
        })).resolves.toBe(false)

        expect(current.conversation.message).toHaveLength(2)
    })

    it('aborts a no-session delete when navigation changes during the first confirmation', async () => {
        const original = currentTarget('original')
        const replacement = currentTarget('replacement')
        let current = original
        const confirmation = deferred<boolean>()
        const confirmInstantRemoval = vi.fn(async () => true)

        const removing = removeChatMessage({
            absoluteIndex: 1,
            shiftKey: false,
            recursive: false,
            askRemoval: true,
            instantRemove: false,
            captureCurrent: () => current,
            getCurrentSession: () => null,
            confirmRemoval: () => confirmation.promise,
            confirmInstantRemoval,
        })
        current = replacement
        confirmation.resolve(true)

        await expect(removing).resolves.toBe(false)
        expect(original.conversation.message.map((message) => message.data)).toEqual([
            'original-zero',
            'original-one',
        ])
        expect(replacement.conversation.message.map((message) => message.data)).toEqual([
            'replacement-zero',
            'replacement-one',
        ])
        expect(confirmInstantRemoval).not.toHaveBeenCalled()
    })

    it('aborts a no-session truncate when navigation changes during the second confirmation', async () => {
        const original = currentTarget('original')
        const replacement = currentTarget('replacement')
        let current = original
        const confirmation = deferred<boolean>()

        const removing = removeChatMessage({
            absoluteIndex: 1,
            shiftKey: false,
            recursive: false,
            askRemoval: false,
            instantRemove: true,
            captureCurrent: () => current,
            getCurrentSession: () => null,
            confirmRemoval: vi.fn(async () => true),
            confirmInstantRemoval: () => confirmation.promise,
        })
        current = replacement
        confirmation.resolve(false)

        await expect(removing).resolves.toBe(false)
        expect(original.conversation.message.map((message) => message.data)).toEqual([
            'original-zero',
            'original-one',
        ])
        expect(replacement.conversation.message.map((message) => message.data)).toEqual([
            'replacement-zero',
            'replacement-one',
        ])
    })
})
