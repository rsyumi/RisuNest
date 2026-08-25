// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { character, Message } from 'src/ts/storage/database.svelte'

const imageMocks = vi.hoisted(() => ({
    mode: 'normal',
    staleReject: undefined as ((reason?: unknown) => void) | undefined,
    pendingResolve: undefined as ((value: string) => void) | undefined,
    getCharImage: vi.fn((source: string | undefined) => {
        if (source === 'reject.png') return Promise.reject(new Error('image rejected'))
        if (source === 'stale-reject.png') {
            return new Promise<string>((_resolve, reject) => {
                imageMocks.staleReject = reject
            })
        }
        if (source === 'pending.png') {
            return new Promise<string>((resolve) => {
                imageMocks.pendingResolve = resolve
            })
        }
        return Promise.resolve(`${imageMocks.mode}:${source ?? ''}`)
    }),
}))

vi.mock('src/ts/characters', () => ({ getCharImage: imageMocks.getCharImage }))
vi.mock('src/ts/globalApi.svelte', () => ({ chatFoldedStateMessageIndex: { index: -1 } }))
vi.mock('src/ts/chatLoadPages', () => ({ shouldContainChatMessage: () => true }))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: {
            db: {
                streamingDisplayOptimizationMode: 'balanced',
                autoScrollToNewMessage: false,
                alwaysScrollToNewMessage: false,
            },
        },
        selectedCharID: writable(0),
        ReloadChatPointer: writable({}),
        ReloadGUIPointer: writable(0),
        createSimpleCharacter: (char: character) => ({
            type: 'simple',
            chaId: char.chaId,
            virtualscript: char.virtualscript,
            customscript: char.customscript,
            additionalAssets: char.additionalAssets,
            emotionImages: char.emotionImages,
            triggerscript: char.triggerscript,
        }),
    }
})
vi.mock('./Chat.svelte', async () => ({ default: (await import('./ChatMountProbe.test.svelte')).default }))

import { ReloadGUIPointer } from 'src/ts/stores.svelte'
import ChatsHarness from './ChatsHarness.test.svelte'
import { chatMountProbe, resetChatMountProbe } from './chatMountProbe'

interface HarnessInstance {
    setMessages(messages: Message[]): void
    updateMessage(index: number, data: string): void
    setStreaming(isStreaming: boolean): void
    replaceParserDependencies(): void
    mutateAssetTuple(path: string): void
    mutateScriptOutput(output: string): void
    setImage(image: string): void
    switchCharacter(character: character, messages: Message[]): void
}

function makeMessage(index: number, overrides: Partial<Message> = {}): Message {
    return {
        role: index % 2 === 0 ? 'char' : 'user',
        data: `message-${index}`,
        chatId: `message-id-${index}`,
        ...overrides,
    }
}

function makeCharacter(messages: Message[], isStreaming = false): character {
    return {
        type: 'character',
        name: 'Character',
        image: 'character.png',
        chaId: 'character-id',
        chatPage: 0,
        chats: [{
            id: 'chat-room-id',
            message: messages,
            isStreaming,
            activeStreamingDisplayOptimizationMode: 'balanced',
        }],
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [],
    } as unknown as character
}

function probeElements(target: HTMLElement): HTMLElement[] {
    return [...target.querySelectorAll<HTMLElement>('[data-chat-probe]')]
}

function probeIdForMessage(target: HTMLElement, message: string): number {
    const element = probeElements(target).find((candidate) => candidate.dataset.message === message)
    if (!element) throw new Error(`No probe for ${message}`)
    return Number(element.dataset.chatProbe)
}

describe('Chats imperative mount lifecycle', () => {
    let mounted: ReturnType<typeof mount> | undefined
    let target: HTMLDivElement

    beforeEach(() => {
        resetChatMountProbe()
        imageMocks.mode = 'normal'
        imageMocks.staleReject = undefined
        imageMocks.pendingResolve = undefined
        imageMocks.getCharImage.mockClear()
        target = document.createElement('div')
        document.body.appendChild(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
    })

    test('keeps DOM order and settled component state, then cleans up a removed message', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        expect(probeElements(target).map((element) => element.dataset.message)).toEqual(
            [...messages].reverse().map((message) => message.data),
        )
        const oldestInstance = probeIdForMessage(target, 'message-0')

        const appended = [...messages, makeMessage(8)]
        ;(mounted as HarnessInstance).setMessages(appended)
        await tick()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(9))
        expect(probeIdForMessage(target, 'message-0')).toBe(oldestInstance)

        ;(mounted as HarnessInstance).setMessages(appended.slice(1))
        await tick()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        expect(chatMountProbe.unmounts).toContain(oldestInstance)
        expect(probeElements(target).map((element) => element.dataset.message)).toEqual(
            appended.slice(1).reverse().map((message) => message.data),
        )

        const remainingInstances = probeElements(target).map((element) => Number(element.dataset.chatProbe))
        await unmount(mounted)
        mounted = undefined
        expect(remainingInstances.every((instanceId) => chatMountProbe.unmounts.includes(instanceId))).toBe(true)
    })

    test('mounts duplicate and missing IDs, including the same object twice, as distinct occurrences', async () => {
        const repeated = makeMessage(0, { chatId: undefined, data: 'repeated' })
        const messages = [
            repeated,
            repeated,
            makeMessage(2, { chatId: 'duplicate', data: 'duplicate-a' }),
            makeMessage(3, { chatId: 'duplicate', data: 'duplicate-b' }),
        ]
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(4))
        expect(new Set(probeElements(target).map((element) => element.dataset.chatProbe)).size).toBe(4)
        expect(probeElements(target).filter((element) => element.dataset.message === 'repeated')).toHaveLength(2)
    })

    test('updates an optimized streaming mount in place and remounts it when streaming settles', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        messages[7].role = 'char'
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages, true) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const streamingInstance = probeIdForMessage(target, 'message-7')

        ;(mounted as HarnessInstance).updateMessage(7, 'streamed chunk')
        await tick()
        await vi.waitFor(() => expect(chatMountProbe.streamingUpdates.some((update) => (
            update.instanceId === streamingInstance && update.rawStreamingText === 'streamed chunk'
        ))).toBe(true))
        expect(Number(probeElements(target)[0].dataset.chatProbe)).toBe(streamingInstance)

        ;(mounted as HarnessInstance).setStreaming(false)
        await tick()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'streamed chunk')).not.toBe(streamingInstance))
        expect(chatMountProbe.unmounts).toContain(streamingInstance)
    })

    test('remounts when resolved image mode or parser dependency identity changes', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index, { role: 'char' }))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const initialInstance = probeIdForMessage(target, 'message-0')
        expect(probeElements(target).at(-1)?.dataset.image).toBe('normal:character.png')

        imageMocks.mode = 'alternate'
        ReloadGUIPointer.update((value) => value + 1)
        await vi.waitFor(() => expect(probeElements(target).at(-1)?.dataset.image).toBe('alternate:character.png'))
        const imageInstance = probeIdForMessage(target, 'message-0')
        expect(imageInstance).not.toBe(initialInstance)

        ;(mounted as HarnessInstance).replaceParserDependencies()
        await tick()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(imageInstance))
    })

    test('remounts after parser-relevant asset tuples and scripts mutate in place', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index, { role: 'char' }))
        const currentCharacter = makeCharacter(messages)
        currentCharacter.additionalAssets = [['Portrait', 'portrait.png', 'png']]
        currentCharacter.customscript = [{ type: 'editdisplay', in: 'before', out: 'after', comment: '' }]
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: currentCharacter },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const initialInstance = probeIdForMessage(target, 'message-0')

        ;(mounted as HarnessInstance).mutateAssetTuple('changed.png')
        await tick()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(initialInstance))
        const assetEditInstance = probeIdForMessage(target, 'message-0')

        ;(mounted as HarnessInstance).mutateScriptOutput('changed')
        await tick()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(assetEditInstance))
    })

    test('renders with a safe fallback when initial image resolution rejects', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index, { role: 'char' }))
        const currentCharacter = makeCharacter(messages)
        currentCharacter.image = 'reject.png'
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: currentCharacter },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        expect(probeElements(target)[0].dataset.image).toBe('')
    })

    test('ignores a stale image rejection after a newer image resolves', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index, { role: 'char' }))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)[0]?.dataset.image).toBe('normal:character.png'))

        ;(mounted as HarnessInstance).setImage('stale-reject.png')
        await tick()
        await vi.waitFor(() => expect(imageMocks.staleReject).toBeTypeOf('function'))
        ;(mounted as HarnessInstance).setImage('latest.png')
        await tick()
        await vi.waitFor(() => expect(probeElements(target)[0]?.dataset.image).toBe('normal:latest.png'))

        imageMocks.staleReject?.(new Error('stale image rejected'))
        await tick()
        expect(probeElements(target)[0]?.dataset.image).toBe('normal:latest.png')
    })

    test('publishes a new character immediately while its image resolution is pending', async () => {
        const oldMessages = [makeMessage(0, { data: 'old-character-message', role: 'char' })]
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: oldMessages, initialCharacter: makeCharacter(oldMessages) },
        })
        await vi.waitFor(() => expect(probeElements(target)[0]?.dataset.message).toBe('old-character-message'))
        const oldInstance = probeIdForMessage(target, 'old-character-message')

        const newMessages = [makeMessage(1, { data: 'new-character-message', role: 'char' })]
        const newCharacter = makeCharacter(newMessages)
        newCharacter.chaId = 'new-character-id'
        newCharacter.name = 'New Character'
        newCharacter.image = 'pending.png'
        newCharacter.chats[0].id = 'new-chat-room-id'
        ;(mounted as HarnessInstance).switchCharacter(newCharacter, newMessages)
        await tick()
        await vi.waitFor(() => expect(imageMocks.pendingResolve).toBeTypeOf('function'))

        await vi.waitFor(() => expect(probeElements(target).map((element) => element.dataset.message)).toEqual([
            'new-character-message',
        ]))
        expect(target.textContent).not.toContain('old-character-message')
        expect(chatMountProbe.unmounts).toContain(oldInstance)
        expect(probeElements(target)[0]?.dataset.image).toBe('')

        imageMocks.pendingResolve?.('resolved:pending.png')
        await vi.waitFor(() => expect(probeElements(target)[0]?.dataset.image).toBe('resolved:pending.png'))
    })

    test('memoizes a 10k asset stamp across streaming chunks and rescans after an in-place edit', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        messages[7].role = 'char'
        const scan = { reads: 0 }
        const assets = Array.from({ length: 10_000 }, (_, index) => {
            let name = `Asset ${index}`
            const tuple = [name, `asset-${index}.png`, 'png'] as [string, string, string]
            Object.defineProperty(tuple, 0, {
                configurable: true,
                enumerable: true,
                get: () => {
                    scan.reads++
                    return name
                },
                set: (value: string) => {
                    name = value
                },
            })
            return tuple
        })
        const currentCharacter = makeCharacter(messages, true)
        currentCharacter.additionalAssets = assets
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: currentCharacter },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const initialReads = scan.reads
        expect(initialReads).toBeGreaterThanOrEqual(10_000)

        ;(mounted as HarnessInstance).updateMessage(7, 'streamed without parser changes')
        await tick()
        await vi.waitFor(() => expect(chatMountProbe.streamingUpdates.some((update) => (
            update.rawStreamingText === 'streamed without parser changes'
        ))).toBe(true))
        expect(scan.reads).toBe(initialReads)

        const initialInstance = probeIdForMessage(target, 'message-0')
        ;(mounted as HarnessInstance).mutateAssetTuple('changed.png')
        await tick()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(initialInstance))
        expect(scan.reads).toBeGreaterThanOrEqual(initialReads + 10_000)
    })
})
