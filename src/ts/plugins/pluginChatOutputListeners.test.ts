import { describe, expect, it, vi } from 'vitest'
import type { Chat } from '../storage/database.svelte'
import type { PluginCompleteCharacter } from './pluginDatabaseAccess'
import {
    dispatchChatOutputListeners,
    registerChatOutputListener,
    removeChatOutputListener,
    type ChatOutputListener,
} from './pluginChatOutputListeners'

const liveChat = {
    id: 'chat-a',
    name: 'Live',
    note: '',
    localLore: [],
    message: [{ role: 'char', data: 'live' }],
} as Chat
const liveChar = {
    type: 'character',
    chaId: 'char-a',
    name: 'Live owner',
    chatPage: 0,
    chats: [liveChat],
} as PluginCompleteCharacter
const exactChat = structuredClone(liveChat)
const exactChar = { ...structuredClone(liveChar), chats: [exactChat] }

function deferred() {
    let resolve!: () => void
    const promise = new Promise<void>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function registry() {
    return { listeners: new Set<ChatOutputListener>() }
}

function projector() {
    return vi.fn(async () => ({ char: exactChar, chat: exactChat }))
}

describe('plugin chat output listeners', () => {
    it('projects once and shares char and chat references across one event', async () => {
        const { listeners } = registry()
        const first = vi.fn(({ char }) => { char.listenerMutation = 'visible' })
        const second = vi.fn(({ char }) => expect(char.listenerMutation).toBe('visible'))
        registerChatOutputListener(listeners, first)
        registerChatOutputListener(listeners, second)
        const projectScalable = projector()

        await dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable,
            onError: vi.fn(),
        })

        expect(projectScalable).toHaveBeenCalledOnce()
        expect(first.mock.calls[0][0].char).toBe(second.mock.calls[0][0].char)
        expect(first.mock.calls[0][0].chat).toBe(second.mock.calls[0][0].chat)
    })

    it('isolates projection errors from the generation flow', async () => {
        const { listeners } = registry()
        const listener = vi.fn()
        const projectionError = new Error('projection failed')
        const onError = vi.fn()
        registerChatOutputListener(listeners, listener)

        await expect(dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable: vi.fn().mockRejectedValue(projectionError),
            onError,
        })).resolves.toBeUndefined()

        expect(onError).toHaveBeenCalledOnce()
        expect(onError).toHaveBeenCalledWith(projectionError)
        expect(listener).not.toHaveBeenCalled()
    })

    it('awaits sequentially and isolates listener errors', async () => {
        const { listeners } = registry()
        const order: string[] = []
        const first = async () => {
            order.push('first-start')
            await Promise.resolve()
            order.push('first-end')
        }
        const second = async () => {
            order.push('second')
            throw new Error('isolated')
        }
        const third = async () => { order.push('third') }
        registerChatOutputListener(listeners, first)
        registerChatOutputListener(listeners, second)
        registerChatOutputListener(listeners, third)

        await dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable: projector(),
            onError: (error) => order.push((error as Error).message),
        })

        expect(order).toEqual(['first-start', 'first-end', 'second', 'isolated', 'third'])
    })

    it('skips a listener removed while an earlier listener is pending', async () => {
        const { listeners } = registry()
        const started = deferred()
        const finish = deferred()
        const first = async () => {
            started.resolve()
            await finish.promise
        }
        const removed = vi.fn()
        registerChatOutputListener(listeners, first)
        registerChatOutputListener(listeners, removed)

        const dispatching = dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable: projector(),
            onError: vi.fn(),
        })
        await started.promise
        removeChatOutputListener(listeners, removed)
        finish.resolve()
        await dispatching

        expect(removed).not.toHaveBeenCalled()
    })

    it('preserves registration order across one projected event', async () => {
        const { listeners } = registry()
        const order: string[] = []
        const received: Array<{ char: unknown; chat: unknown }> = []
        const add = (name: string) => {
            const listener: ChatOutputListener = ({ char, chat }) => {
                order.push(name)
                received.push({ char, chat })
            }
            registerChatOutputListener(listeners, listener)
        }
        add('first')
        add('second')
        add('third')
        const projectScalable = projector()

        await dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable,
            onError: vi.fn(),
        })

        expect(order).toEqual(['first', 'second', 'third'])
        expect(projectScalable).toHaveBeenCalledOnce()
        expect(received[0].char).toBe(received[1].char)
        expect(received[0].chat).toBe(received[2].chat)
    })

    it('does no event work when already aborted', async () => {
        const { listeners } = registry()
        const listener = vi.fn()
        registerChatOutputListener(listeners, listener)
        const controller = new AbortController()
        controller.abort()
        const projectScalable = projector()

        await dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable,
            onError: vi.fn(),
            signal: controller.signal,
        })

        expect(projectScalable).not.toHaveBeenCalled()
        expect(listener).not.toHaveBeenCalled()
    })

    it('does not launch a listener when aborted during projection', async () => {
        const { listeners } = registry()
        const listener = vi.fn()
        registerChatOutputListener(listeners, listener)
        const controller = new AbortController()
        let resolveProjection!: (value: { char: PluginCompleteCharacter; chat: Chat }) => void
        const projection = new Promise<{
            char: PluginCompleteCharacter
            chat: Chat
        }>((resolve) => {
            resolveProjection = resolve
        })
        const projectScalable = vi.fn(() => projection)

        const dispatching = dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable,
            onError: vi.fn(),
            signal: controller.signal,
        })
        await vi.waitFor(() => expect(projectScalable).toHaveBeenCalledOnce())
        controller.abort()
        resolveProjection({ char: exactChar, chat: exactChat })
        await dispatching

        expect(listener).not.toHaveBeenCalled()
    })

    it('does not launch later listeners after aborting a pending listener', async () => {
        const { listeners } = registry()
        const started = deferred()
        const finish = deferred()
        const first = async () => {
            started.resolve()
            await finish.promise
        }
        const second = vi.fn()
        registerChatOutputListener(listeners, first)
        registerChatOutputListener(listeners, second)
        const controller = new AbortController()

        const dispatching = dispatchChatOutputListeners({
            listeners,
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable: projector(),
            onError: vi.fn(),
            signal: controller.signal,
        })
        await started.promise
        controller.abort()
        finish.resolve()
        await dispatching

        expect(second).not.toHaveBeenCalled()
    })
})
