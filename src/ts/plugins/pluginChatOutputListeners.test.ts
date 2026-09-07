import { describe, expect, it, vi } from 'vitest'
import type { Chat } from '../storage/database.svelte'
import type { PluginCompleteCharacter } from './pluginDatabaseAccess'
import {
    dispatchChatOutputListeners,
    registerChatOutputListener,
    removeChatOutputListener,
    type ChatOutputListener,
    type ChatOutputListenerProvenance,
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
    return {
        listeners: new Set<ChatOutputListener>(),
        provenance: new WeakMap<ChatOutputListener, ChatOutputListenerProvenance>(),
    }
}

describe('plugin chat output listeners', () => {
    it('projects once and shares char and chat references across one scalable event', async () => {
        const { listeners, provenance } = registry()
        const first = vi.fn(({ char }) => { char.listenerMutation = 'visible' })
        const second = vi.fn(({ char }) => expect(char.listenerMutation).toBe('visible'))
        registerChatOutputListener(listeners, provenance, first, 'v3-legacy')
        registerChatOutputListener(listeners, provenance, second, 'v3-legacy')
        const projectScalable = vi.fn(async () => ({ char: exactChar, chat: exactChat }))

        await dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'scalable-v3',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot: structuredClone,
            projectScalable,
            onError: vi.fn(),
        })

        expect(projectScalable).toHaveBeenCalledOnce()
        expect(first.mock.calls[0][0].char).toBe(second.mock.calls[0][0].char)
        expect(first.mock.calls[0][0].chat).toBe(second.mock.calls[0][0].chat)
    })

    it('isolates scalable projection errors from the generation flow', async () => {
        const { listeners, provenance } = registry()
        const listener = vi.fn()
        const projectionError = new Error('projection failed')
        const onError = vi.fn()
        registerChatOutputListener(listeners, provenance, listener, 'v3-legacy')

        await expect(dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'scalable-v3',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot: structuredClone,
            projectScalable: vi.fn().mockRejectedValue(projectionError),
            onError,
        })).resolves.toBeUndefined()

        expect(onError).toHaveBeenCalledOnce()
        expect(onError).toHaveBeenCalledWith(projectionError)
        expect(listener).not.toHaveBeenCalled()
    })

    it('awaits sequentially and isolates listener errors', async () => {
        const { listeners, provenance } = registry()
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
        registerChatOutputListener(listeners, provenance, first, 'v3-legacy')
        registerChatOutputListener(listeners, provenance, second, 'v3-legacy')
        registerChatOutputListener(listeners, provenance, third, 'v3-legacy')

        await dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'maximum-compatibility',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot: structuredClone,
            projectScalable: vi.fn(),
            onError: (error) => order.push((error as Error).message),
        })

        expect(order).toEqual(['first-start', 'first-end', 'second', 'isolated', 'third'])
    })

    it('skips a listener removed while an earlier listener is pending', async () => {
        const { listeners, provenance } = registry()
        const started = deferred()
        const finish = deferred()
        const first = async () => {
            started.resolve()
            await finish.promise
        }
        const removed = vi.fn()
        registerChatOutputListener(listeners, provenance, first, 'v3-legacy')
        registerChatOutputListener(listeners, provenance, removed, 'v3-legacy')

        const dispatching = dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'maximum-compatibility',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot: structuredClone,
            projectScalable: vi.fn(),
            onError: vi.fn(),
        })
        await started.promise
        removeChatOutputListener(listeners, provenance, removed)
        finish.resolve()
        await dispatching

        expect(removed).not.toHaveBeenCalled()
    })

    it('snapshots maximum events once and preserves mixed registration order', async () => {
        const { listeners, provenance } = registry()
        const order: string[] = []
        const received: Array<{ char: unknown; chat: unknown }> = []
        const add = (name: string, source: ChatOutputListenerProvenance) => {
            const listener: ChatOutputListener = ({ char, chat }) => {
                order.push(name)
                received.push({ char, chat })
            }
            registerChatOutputListener(listeners, provenance, listener, source)
        }
        add('v2-first', 'v2.1-live')
        add('v3-second', 'v3-legacy')
        add('v2-third', 'v2.1-live')
        const snapshotCall = vi.fn()
        const snapshot = <T>(value: T): T => {
            snapshotCall(value)
            return structuredClone(value)
        }
        const projectScalable = vi.fn()

        await dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'maximum-compatibility',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot,
            projectScalable,
            onError: vi.fn(),
        })

        expect(order).toEqual(['v2-first', 'v3-second', 'v2-third'])
        expect(snapshotCall).toHaveBeenCalledTimes(2)
        expect(projectScalable).not.toHaveBeenCalled()
        expect(received[0].char).toBe(received[1].char)
        expect(received[0].chat).toBe(received[2].chat)
    })

    it('does not project for a stale v2-only listener in scalable mode', async () => {
        const { listeners, provenance } = registry()
        const listener = vi.fn()
        registerChatOutputListener(listeners, provenance, listener, 'v2.1-live')
        const snapshotCall = vi.fn()
        const snapshot = <T>(value: T): T => {
            snapshotCall(value)
            return structuredClone(value)
        }
        const projectScalable = vi.fn()

        await dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'scalable-v3',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot,
            projectScalable,
            onError: vi.fn(),
        })

        expect(projectScalable).not.toHaveBeenCalled()
        expect(snapshotCall).toHaveBeenCalledTimes(2)
        expect(listener).toHaveBeenCalledOnce()
    })

    it('does no event work when already aborted', async () => {
        const { listeners, provenance } = registry()
        const listener = vi.fn()
        registerChatOutputListener(listeners, provenance, listener, 'v3-legacy')
        const controller = new AbortController()
        controller.abort()
        const snapshotSpy = vi.fn()
        const snapshot = <T>(value: T): T => {
            snapshotSpy(value)
            return structuredClone(value)
        }
        const projectScalable = vi.fn()

        await dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'maximum-compatibility',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot,
            projectScalable,
            onError: vi.fn(),
            signal: controller.signal,
        })

        expect(snapshotSpy).not.toHaveBeenCalled()
        expect(projectScalable).not.toHaveBeenCalled()
        expect(listener).not.toHaveBeenCalled()
    })

    it('does not launch a listener when aborted during scalable projection', async () => {
        const { listeners, provenance } = registry()
        const listener = vi.fn()
        registerChatOutputListener(listeners, provenance, listener, 'v3-legacy')
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
            provenance,
            profile: 'scalable-v3',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot: structuredClone,
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
        const { listeners, provenance } = registry()
        const started = deferred()
        const finish = deferred()
        const first = async () => {
            started.resolve()
            await finish.promise
        }
        const second = vi.fn()
        registerChatOutputListener(listeners, provenance, first, 'v3-legacy')
        registerChatOutputListener(listeners, provenance, second, 'v3-legacy')
        const controller = new AbortController()

        const dispatching = dispatchChatOutputListeners({
            listeners,
            provenance,
            profile: 'maximum-compatibility',
            char: liveChar,
            chat: liveChat,
            characterIndex: 0,
            chatIndex: 0,
            messageIndex: 1,
            snapshot: structuredClone,
            projectScalable: vi.fn(),
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
