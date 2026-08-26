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

class TestResizeObserver {
    static instances: TestResizeObserver[] = []
    readonly observed = new Set<Element>()
    disconnected = false

    constructor(private readonly callback: ResizeObserverCallback) {
        TestResizeObserver.instances.push(this)
    }

    observe(target: Element) {
        this.observed.add(target)
    }

    unobserve(target: Element) {
        this.observed.delete(target)
    }

    disconnect() {
        this.disconnected = true
        this.observed.clear()
    }

    emit(target: Element, height: number) {
        this.callback([{
            target,
            contentRect: { height } as DOMRectReadOnly,
        } as ResizeObserverEntry], this as unknown as ResizeObserver)
    }
}

vi.mock('src/ts/characters', () => ({ getCharImage: imageMocks.getCharImage }))
vi.mock('src/ts/globalApi.svelte', () => ({ chatFoldedStateMessageIndex: { index: -1 } }))
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
vi.mock('./CreatorQuote.svelte', async () => ({ default: (await import('./ChatMountProbe.test.svelte')).default }))

import { ReloadGUIPointer } from 'src/ts/stores.svelte'
import { setRuntimePerformanceProfile } from 'src/ts/runtimePerformanceProfile'
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
    jumpTo(index: number, options?: { align?: 'start' | 'center'; highlight?: boolean }): Promise<boolean>
    jumpToLatestMessage(): Promise<void>
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
        firstMessage: 'first greeting',
        alternateGreetings: [],
        creatorNotes: '',
        removedQuotes: false,
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [],
    } as unknown as character
}

function probeElements(target: HTMLElement): HTMLElement[] {
    return [...target.querySelectorAll<HTMLElement>('[data-chat-probe]')]
        .filter((element) => element.dataset.index !== '-1')
}

function conversationStartProbe(target: HTMLElement): HTMLElement | null {
    return target.querySelector<HTMLElement>('[data-chat-probe][data-index="-1"]')
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
        setRuntimePerformanceProfile('normal')
        TestResizeObserver.instances = []
        vi.stubGlobal('ResizeObserver', TestResizeObserver)
        target = document.createElement('div')
        document.body.appendChild(target)
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.unstubAllGlobals()
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

    test('keeps 10,000 settled turns within the profile mount budget across direct jumps', async () => {
        const messages = Array.from({ length: 10_000 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        expect(target.querySelectorAll('[data-chat-gap]')).toHaveLength(1)

        for (const index of [100, 5_000, 0, 9_999, 4_321]) {
            await expect((mounted as HarnessInstance).jumpTo(index)).resolves.toBe(true)
            expect(probeElements(target).length).toBeLessThanOrEqual(64)
            expect(probeElements(target).some((element) => element.dataset.message === `message-${index}`)).toBe(true)
        }

        expect(chatMountProbe.mounts.length - chatMountProbe.unmounts.length).toBeLessThanOrEqual(65)
    })

    test('replaces bounded rows while reverse-flex scrolling crosses measured gaps', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({ top: 0, bottom: 500, height: 500 } as DOMRect)
        let gaps = [...target.querySelectorAll<HTMLElement>('[data-chat-gap]')]
        expect(gaps).toHaveLength(1)
        gaps[0].getBoundingClientRect = () => ({ top: 0, bottom: 100, height: 100 } as DOMRect)

        scrollParent.scrollTop = -100
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-135'),
        ).toBe(true))
        expect(probeElements(target)).toHaveLength(64)

        gaps = [...target.querySelectorAll<HTMLElement>('[data-chat-gap]')]
        expect(gaps).toHaveLength(2)
        for (const gap of gaps) {
            const isTrailing = Number(gap.dataset.chatGapStart) > 136
            gap.getBoundingClientRect = () => isTrailing
                ? ({ top: 300, bottom: 400, height: 100 } as DOMRect)
                : ({ top: -1_000, bottom: -900, height: 100 } as DOMRect)
        }

        scrollParent.scrollTop = -50
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-191'),
        ).toBe(true))
        expect(probeElements(target)).toHaveLength(64)
    })

    test('uses the low-spec mounted-message budget', async () => {
        setRuntimePerformanceProfile('low-spec')
        const messages = Array.from({ length: 10_000 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(40))
    })

    test('mounts the measured conversation-start row only near the oldest turn', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        expect(conversationStartProbe(target)).toBeNull()

        await expect((mounted as HarnessInstance).jumpTo(0)).resolves.toBe(true)
        expect(conversationStartProbe(target)).not.toBeNull()
        expect(probeElements(target).length).toBeLessThan(64)

        await (mounted as HarnessInstance).jumpToLatestMessage()
        expect(conversationStartProbe(target)).toBeNull()
    })

    test('pins a focused editor while navigation replaces settled rows, then releases it', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(0)
        const message = probeElements(target).find((element) => element.dataset.message === 'message-0')!
        const editor = document.createElement('textarea')
        message.append(editor)
        editor.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))

        await (mounted as HarnessInstance).jumpToLatestMessage()
        expect(probeElements(target).some((element) => element.dataset.message === 'message-0')).toBe(true)
        expect(probeElements(target).length).toBeLessThanOrEqual(64)

        editor.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: null }))
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-0'),
        ).toBe(false))
    })

    test('keeps actual editor focus without detaching its retained row during navigation', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(0)

        const message = probeElements(target).find((element) => element.dataset.message === 'message-0')!
        const row = message.closest<HTMLElement>('[data-chat-render-key]')!
        const editor = document.createElement('textarea')
        message.append(editor)
        editor.focus()
        expect(document.activeElement).toBe(editor)

        const removedRows: Node[] = []
        const observer = new MutationObserver((records) => {
            for (const record of records) removedRows.push(...record.removedNodes)
        })
        observer.observe(row.parentElement!, { childList: true })

        await (mounted as HarnessInstance).jumpToLatestMessage()
        await Promise.resolve()
        observer.disconnect()

        expect(document.activeElement).toBe(editor)
        expect(removedRows).not.toContain(row)
        expect(row.isConnected).toBe(true)
    })

    test('pins only actually playing media and releases it on pause', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(0)
        const message = probeElements(target).find((element) => element.dataset.message === 'message-0')!
        const row = message.closest<HTMLElement>('[data-chat-render-key]')!
        const media = document.createElement('audio')
        message.append(media)
        media.dispatchEvent(new Event('play'))
        const removedRows: Node[] = []
        const observer = new MutationObserver((records) => {
            for (const record of records) removedRows.push(...record.removedNodes)
        })
        observer.observe(row.parentElement!, { childList: true })

        await (mounted as HarnessInstance).jumpToLatestMessage()
        await Promise.resolve()
        observer.disconnect()
        expect(probeElements(target).some((element) => element.dataset.message === 'message-0')).toBe(true)
        expect(media.isConnected).toBe(true)
        expect(removedRows).not.toContain(row)

        media.dispatchEvent(new Event('pause'))
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-0'),
        ).toBe(false))
    })

    test('drops stale playing-media state when a row remounts', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(0)

        const message = probeElements(target).find((element) => element.dataset.message === 'message-0')!
        const originalInstance = Number(message.dataset.chatProbe)
        const media = document.createElement('audio')
        message.append(media)
        media.dispatchEvent(new Event('play'))

        ReloadGUIPointer.update((value) => value + 1)
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(originalInstance))
        expect(media.isConnected).toBe(false)
        expect(chatMountProbe.unmounts).toContain(originalInstance)

        await (mounted as HarnessInstance).jumpToLatestMessage()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-0'),
        ).toBe(false))
    })

    test('pins the newest streaming row during an old-history jump and releases it when settled', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        messages.at(-1)!.role = 'char'
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages, true) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))

        await expect((mounted as HarnessInstance).jumpTo(0)).resolves.toBe(true)
        expect(probeElements(target).some((element) => element.dataset.message === 'message-199')).toBe(true)
        expect(probeElements(target).length).toBeLessThanOrEqual(64)

        ;(mounted as HarnessInstance).setStreaming(false)
        await tick()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-199'),
        ).toBe(false))
    })

    test('corrects the stable-key anchor after a measured height change', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(100)

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        const wrappers = [...target.querySelectorAll<HTMLElement>('[data-chat-render-key]')]
        const anchor = wrappers.find((element) => element.dataset.chatIndex === '100')!
        let anchorTop = 120
        scrollParent.getBoundingClientRect = () => ({
            top: 0,
            bottom: 500,
            height: 500,
        } as DOMRect)
        scrollParent.scrollBy = vi.fn()
        for (const wrapper of wrappers) {
            wrapper.getBoundingClientRect = () => ({
                top: wrapper === anchor ? anchorTop : 1_000,
                bottom: wrapper === anchor ? anchorTop + 100 : 1_100,
                height: 100,
            } as DOMRect)
        }

        TestResizeObserver.instances[0].emit(anchor, 100)
        anchorTop = 170

        await vi.waitFor(() => expect(scrollParent.scrollBy).toHaveBeenCalledWith({
            top: 50,
            behavior: 'instant',
        }))
    })

    test('forgets deleted row heights before the same message ID is reused', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(0)

        const oldMessage = probeElements(target).find(
            (element) => element.dataset.message === 'message-0',
        )!
        const oldRow = oldMessage.closest<HTMLElement>('[data-chat-render-key]')!
        TestResizeObserver.instances[0].emit(oldRow, 1_000)
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
        await (mounted as HarnessInstance).jumpToLatestMessage()
        expect([...target.querySelectorAll<HTMLElement>('[data-chat-gap]')].map((gap) => (
            (gap as HTMLElement).style.height
        ))).toEqual([`${137 * 256 + 744}px`])

        const withoutOldMessage = messages.slice(1)
        ;(mounted as HarnessInstance).setMessages(withoutOldMessage)
        await tick()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-0'),
        ).toBe(false))

        const replacement = makeMessage(0, { data: 'replacement-message-0' })
        const withReusedId = [replacement, ...withoutOldMessage]
        ;(mounted as HarnessInstance).setMessages(withReusedId)
        await tick()
        await (mounted as HarnessInstance).jumpToLatestMessage()

        expect([...target.querySelectorAll<HTMLElement>('[data-chat-gap]')].map((gap) => ({
            start: gap.dataset.chatGapStart,
            end: gap.dataset.chatGapEnd,
            height: gap.style.height,
        }))).toEqual([{ start: '0', end: '137', height: `${137 * 256}px` }])
    })

    test('disconnects the shared observer and releases mounted rows on teardown', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const observer = TestResizeObserver.instances[0]
        const activeInstances = probeElements(target).map((element) => Number(element.dataset.chatProbe))
        const media = document.createElement('audio')
        media.pause = vi.fn()
        probeElements(target)[0].append(media)
        media.dispatchEvent(new Event('play'))

        await unmount(mounted)
        mounted = undefined

        expect(observer.disconnected).toBe(true)
        expect(observer.observed.size).toBe(0)
        expect(media.pause).toHaveBeenCalledOnce()
        expect(activeInstances.every((instance) => chatMountProbe.unmounts.includes(instance))).toBe(true)
    })

    test('settles an in-flight jump when teardown cancels its layout frame', async () => {
        const pendingFrames = new Map<number, FrameRequestCallback>()
        let nextFrame = 1
        const cancelFrame = vi.fn((frame: number) => pendingFrames.delete(frame))
        vi.stubGlobal('requestAnimationFrame', vi.fn((callback: FrameRequestCallback) => {
            const frame = nextFrame++
            pendingFrames.set(frame, callback)
            return frame
        }))
        vi.stubGlobal('cancelAnimationFrame', cancelFrame)
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))

        const jumping = (mounted as HarnessInstance).jumpTo(0)
        await tick()
        await Promise.resolve()
        await unmount(mounted)
        mounted = undefined

        const result = await Promise.race([
            jumping,
            new Promise<'timeout'>((resolve) => setTimeout(() => resolve('timeout'), 50)),
        ])
        expect(result).toBe(false)
        expect(cancelFrame).toHaveBeenCalled()
    })

    test('rejects a pending jump after switching owners with the same imported chat ID', async () => {
        const pendingFrames = new Map<number, FrameRequestCallback>()
        let nextFrame = 1
        vi.stubGlobal('requestAnimationFrame', vi.fn((callback: FrameRequestCallback) => {
            const frame = nextFrame++
            pendingFrames.set(frame, callback)
            return frame
        }))
        vi.stubGlobal('cancelAnimationFrame', vi.fn((frame: number) => pendingFrames.delete(frame)))
        const oldMessages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: oldMessages, initialCharacter: makeCharacter(oldMessages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))

        const jumping = (mounted as HarnessInstance).jumpTo(0)
        await tick()
        const newMessages = Array.from({ length: 200 }, (_, index) => makeMessage(index, {
            data: `new-owner-message-${index}`,
        }))
        const newCharacter = makeCharacter(newMessages)
        newCharacter.chaId = 'new-owner-id'
        ;(mounted as HarnessInstance).switchCharacter(newCharacter, newMessages)
        await tick()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message?.startsWith('new-owner-message-')),
        ).toBe(true))

        for (const [frame, callback] of [...pendingFrames]) {
            pendingFrames.delete(frame)
            callback(0)
        }
        await expect(jumping).resolves.toBe(false)
    })
})
