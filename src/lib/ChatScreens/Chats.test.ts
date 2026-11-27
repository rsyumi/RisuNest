// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, settled as domSettled, unmount } from 'svelte'
import { alertNormal } from 'src/ts/alert'
import type { character, Message } from 'src/ts/storage/database.svelte'
import { chatScreenState } from 'src/ts/ui/chatScreenState.svelte'
import { ActiveConversationSession } from 'src/ts/storage/activeConversationSession'
import { createCatalogCharacterStub } from 'src/ts/storage/workingSetCatalog'
import {
    PersistentConversationViewportSource,
    SynchronousSessionConversationViewportSource,
} from 'src/ts/conversationViewportSource'
import { EditableViewportFixture } from 'src/ts/conversationViewportSource.testSupport'
import type { ConversationViewportSource } from 'src/ts/conversationViewportSource'
import type {
    LiveChatParserProjection,
    LiveChatParserProjectionResolver,
} from 'src/ts/selectedConversationLiveParserProjection'
import { createSelectedConversationLiveParserProjectionResolver } from 'src/ts/selectedConversationLiveParserProjection'
import type { ProcessScriptCaptureContext } from 'src/ts/process/scripts'
import { captureResidentCbsHistory } from 'src/ts/prepareCbsHistory'
import { planChatParserHistory } from 'src/ts/chatParserHistory'
import { createChatParserHistoryProjection } from 'src/ts/chatParserHistoryProjection'

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

const schedulingMocks = vi.hoisted(() => {
    const pending: Array<() => void> = []
    const state = { controlled: false }
    return {
        state,
        pending,
        yieldToMainThread: vi.fn(() => {
            if (!state.controlled) return Promise.resolve()
            return new Promise<void>((resolve) => pending.push(resolve))
        }),
        releaseNext() {
            pending.shift()?.()
        },
        releaseAll() {
            for (const resolve of pending.splice(0)) resolve()
        },
    }
})

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
            borderBoxSize: [{ blockSize: height, inlineSize: 500 }],
            contentRect: { height } as DOMRectReadOnly,
        } as unknown as ResizeObserverEntry], this as unknown as ResizeObserver)
    }
}

vi.mock('src/ts/setting/utils', () => ({ resolveLanguagePath: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertNormal: vi.fn() }))
vi.mock('src/ts/characters', () => ({ getCharImage: imageMocks.getCharImage }))
vi.mock('src/ts/globalApi.svelte', () => ({ chatFoldedStateMessageIndex: { index: -1 } }))
vi.mock('src/ts/ui/yieldToUi', () => ({ yieldToMainThread: schedulingMocks.yieldToMainThread }))
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
vi.mock('../UI/GUI/TextAreaInput.svelte', async () => ({ default: (await import('../UI/GUI/TextInput.svelte')).default }))

import { DBState, ReloadGUIPointer } from 'src/ts/stores.svelte'
import { setRuntimePerformanceProfile } from 'src/ts/runtimePerformanceProfile'
import ChatsHarness from './ChatsHarness.test.svelte'
import ChatsRegexHarness from './ChatsRegexHarness.test.svelte'
import { chatMountProbe, resetChatMountProbe } from './chatMountProbe.testSupport'
import { discardEditorDraftsExcept, pendingEditorDrafts } from 'src/ts/chatEditorDrafts'
import { openTextEditorPopup, textEditorPopup } from 'src/ts/gui/textEditorPopup.svelte'

interface HarnessInstance {
    setMessages(messages: Message[]): void
    updateMessage(index: number, data: string): void
    setStreaming(isStreaming: boolean): void
    replaceParserDependencies(): void
    mutateAssetTuple(path: string): void
    mutateScriptOutput(output: string): void
    setImage(image: string): void
    switchCharacter(character: character, messages: Message[]): void
    switchCharacterAndSource(character: character, source: ConversationViewportSource): void
    jumpTo(index: number, options?: { align?: 'start' | 'center' | 'end'; bottomInset?: number; highlight?: boolean }): Promise<boolean>
    jumpToLatestMessage(): Promise<void>
    jumpToTop(): Promise<boolean>
    jumpToBottom(): Promise<boolean>
    navigateMessage(direction: 'previous' | 'next', bottomInset?: number): Promise<boolean>
    setViewportSource(source: ConversationViewportSource | null): void
    setViewportNavigationGeneration(generation: number): void
    hasUnreadMessage(): boolean
    getCurrentCharacter(): character
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

function makeViewportSource(currentCharacter: character) {
    const conversation = currentCharacter.chats[currentCharacter.chatPage]
    const session = new ActiveConversationSession({
        characterId: currentCharacter.chaId,
        conversationId: conversation.id!,
        conversation,
        storeRevision: 1,
        measureMessage: () => 1,
    })
    const source = new SynchronousSessionConversationViewportSource({
        session,
        captureCurrent: () => ({ character: currentCharacter, conversation }),
    })
    return { session, source }
}

function makeMetadataOnlyCharacter(): character {
    const conversation = {
        id: 'chat-room-id',
        isStreaming: false,
        activeStreamingDisplayOptimizationMode: 'balanced',
    } as character['chats'][number]
    Object.defineProperty(conversation, 'message', {
        get() {
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    return { ...makeCharacter([]), chats: [conversation] } as character
}

function makePersistentViewportSource(messages: readonly Message[]) {
    return new EditableViewportFixture({
        reader: {
            async readConversationWindow({ characterId, conversationId, startIndex, limit }) {
                const endIndex = Math.min(messages.length, startIndex + limit)
                return {
                    revision: 1,
                    value: {
                        characterId,
                        conversationId,
                        startIndex: Math.min(startIndex, messages.length),
                        endIndex,
                        totalMessages: messages.length,
                        messages: messages.slice(startIndex, endIndex),
                        hasMoreBefore: startIndex > 0,
                        hasMoreAfter: endIndex < messages.length,
                    },
                }
            },
        },
        characterId: 'character-id',
        conversationId: 'chat-room-id',
        revision: 1,
        totalMessages: messages.length,
        rowBudget: 64,
    })
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => { resolve = done })
    return { promise, resolve }
}

function boundedProjection(
    currentCharacter: character,
    absoluteIndex: number,
): LiveChatParserProjection {
    const historyOffset = Math.max(0, absoluteIndex - 1)
    const projectedCharacter = makeCharacter(
        Array.from(
            { length: absoluteIndex - historyOffset + 1 },
            (_, offset) => makeMessage(historyOffset + offset),
        ),
    )
    const context: ProcessScriptCaptureContext = {
        presetRegex: [],
        moduleRegexScripts: [],
        moduleAssets: [],
        dynamicAssets: false,
        dynamicAssetsEditDisplay: false,
        parserContext: {
            database: { characters: [projectedCharacter] } as any,
            character: projectedCharacter,
            userName: 'User',
            personaPrompt: '',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
            historyOffset,
        },
    }
    return {
        kind: 'bounded',
        characterId: currentCharacter.chaId,
        conversationId: currentCharacter.chats[0].id!,
        revision: 1,
        totalMessages: absoluteIndex + 1,
        chatID: absoluteIndex,
        projectedChatID: absoluteIndex - historyOffset,
        historyOffset,
        messages: projectedCharacter.chats[0].message,
        context,
    }
}

// The production resolver over resident rows, recording each row's latest projection.
function residentRowParserResolver(
    messages: readonly Message[],
    source: ReturnType<typeof makePersistentViewportSource>,
    currentCharacter: character,
    onPrepare: () => void = () => {},
) {
    const selection = { characterId: 'character-id', conversationId: 'chat-room-id', navigationGeneration: 0, storeRevision: 1 }
    const resolver = createSelectedConversationLiveParserProjectionResolver({
        runtime: {
            store: { readConversationWindow: async () => { throw new Error('All test rows are resident') } } as any,
            captureSelectedConversationTarget: () => ({ ...selection, storeRevision: source.snapshot().storeRevision }) as any,
            captureSelectedConversationAuthority: () => ({ ...selection, storeRevision: source.snapshot().storeRevision, totalMessages: messages.length,
                sessionToken: 'session', sessionVersion: 0, persistedSessionVersion: 0 }) as any,
            getActiveConversationViewportSource: () => source,
            acquireCompleteConversation: async () => { throw new Error('Unexpected complete promotion') },
        },
        maxProjectionMessages: 4,
        captureCurrent: () => ({ character: currentCharacter, conversation: currentCharacter.chats[0] }) as any,
        createBoundedContextSeed: () => {
            const seed = boundedProjection(currentCharacter, 0)
            if (seed.kind !== 'bounded') throw new Error('Expected bounded seed')
            seed.context.parserContext.character.chats[0].message = []
            onPrepare()
            return seed.context
        },
        createCompleteContext: () => { throw new Error('Unexpected complete promotion') },
        parserSource: () => '',
        unsafeDependencies: () => [],
    })
    const projections = new Map<number, LiveChatParserProjection>()
    const resolve = vi.fn(async (input: Parameters<LiveChatParserProjectionResolver['resolve']>[0]) => {
        const projection = await resolver.resolve(input)
        projections.set(input.row.absoluteIndex, projection)
        return projection
    })
    return { resolve, projections }
}

function probeElements(target: HTMLElement): HTMLElement[] {
    return [...target.querySelectorAll<HTMLElement>('[data-chat-probe]')]
        .filter((element) => element.dataset.index !== '-1')
}

async function settleParserProjection(): Promise<void> {
    await domSettled()
    await vi.advanceTimersByTimeAsync(0)
    await domSettled()
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
        chatScreenState.clear()
        resetChatMountProbe()
        discardEditorDraftsExcept(null, null)
        textEditorPopup.request = null
        vi.mocked(alertNormal).mockClear()
        imageMocks.mode = 'normal'
        imageMocks.staleReject = undefined
        imageMocks.pendingResolve = undefined
        imageMocks.getCharImage.mockClear()
        DBState.db.autoScrollToNewMessage = false
        DBState.db.alwaysScrollToNewMessage = false
        setRuntimePerformanceProfile('normal')
        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        schedulingMocks.yieldToMainThread.mockClear()
        TestResizeObserver.instances = []
        vi.stubGlobal('ResizeObserver', TestResizeObserver)
        target = document.createElement('div')
        document.body.appendChild(target)
    })

    afterEach(async () => {
        try {
            if (mounted) await unmount(mounted)
        } finally {
            mounted = undefined
            vi.useRealTimers()
            schedulingMocks.state.controlled = false
            schedulingMocks.releaseAll()
            await Promise.resolve()
            document.body.replaceChildren()
            vi.unstubAllGlobals()
            vi.restoreAllMocks()
        }
    })

    test.each(['persistent', 'session'] as const)('clears the %s viewport when Home releases its character before unmounting Chats', async (kind) => {
        const messages = [makeMessage(0), makeMessage(1)]
        const character = makeCharacter(messages)
        const sessionSource = makeViewportSource(character)
        const source = kind === 'persistent' ? makePersistentViewportSource(messages) : sessionSource.source
        const greetingRelease = vi.fn()
        const resolver: LiveChatParserProjectionResolver = {
            resolve: async ({ row }) => boundedProjection(character, row.absoluteIndex),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: character, initialViewportSource: source,
                parserProjectionResolver: resolver,
                acquireConversationStartParserLease: async () => ({ release: greetingRelease }),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(conversationStartProbe(target)).not.toBeNull()
        const previousMounts = [...chatMountProbe.mounts]
        if (kind === 'session') expect(sessionSource.session.pinCount('viewport')).toBeGreaterThan(0)
        const released = createCatalogCharacterStub({
            id: character.chaId, name: character.name, type: 'character',
            configuredIndex: 0, recentAt: 0, conversationCount: 1, trashed: false,
        }) as character
        ;(mounted as HarnessInstance).switchCharacter(released, [])
        ;(mounted as HarnessInstance).setViewportSource(null)
        await domSettled()
        expect(probeElements(target)).toHaveLength(0)
        expect(conversationStartProbe(target)).toBeNull()
        await vi.waitFor(() => expect(greetingRelease).toHaveBeenCalledOnce())
        for (const previousMount of previousMounts) {
            expect(chatMountProbe.unmounts).toContain(previousMount.instanceId)
            expect(previousMount.parserAbortSignal?.aborted).toBe(true)
        }
        expect(sessionSource.session.pinCount('viewport')).toBe(0)

        ;(mounted as HarnessInstance).switchCharacterAndSource(character, source)
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(conversationStartProbe(target)).not.toBeNull()
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
        await domSettled()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(9))
        expect(probeIdForMessage(target, 'message-0')).toBe(oldestInstance)

        ;(mounted as HarnessInstance).setMessages(appended.slice(1))
        await domSettled()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        expect(chatMountProbe.unmounts).toContain(oldestInstance)
        expect(probeElements(target).map((element) => element.dataset.message)).toEqual(
            appended.slice(1).reverse().map((message) => message.data),
        )
        await vi.waitFor(() =>
            expect(
                target.querySelectorAll('[data-chat-mount-pending]'),
            ).toHaveLength(0),
        )

        const remainingInstances = probeElements(target).map((element) => Number(element.dataset.chatProbe))
        await unmount(mounted)
        mounted = undefined
        expect(remainingInstances.every((instanceId) => chatMountProbe.unmounts.includes(instanceId))).toBe(true)
    })

    test('mounts dialogs from chat rows and the conversation start in the chat screen window layer', async () => {
        // Rows past the first mount batch mount after a yield, outside any component context.
        const messages = Array.from({ length: 6 }, (_, index) => makeMessage(index))
        const windowLayer = document.createElement('div')
        document.body.append(windowLayer)
        chatMountProbe.windowDialogs = true
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages), windowLayer },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(6))
        expect(conversationStartProbe(target)).not.toBeNull()
        // The chat pane clips fixed layers inside it, so each mounted row reaches the layer outside it.
        await vi.waitFor(() =>
            expect(windowLayer.querySelectorAll('[data-window-dialog]')).toHaveLength(target.querySelectorAll('[data-chat-probe]').length),
        )
        expect(target.querySelector('[data-window-dialog]')).toBeNull()
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
        await domSettled()
        await vi.waitFor(() => expect(chatMountProbe.streamingUpdates.some((update) => (
            update.instanceId === streamingInstance && update.rawStreamingText === 'streamed chunk'
        ))).toBe(true))
        expect(Number(probeElements(target)[0].dataset.chatProbe)).toBe(streamingInstance)

        ;(mounted as HarnessInstance).setStreaming(false)
        await domSettled()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'streamed chunk')).not.toBe(streamingInstance))
        expect(chatMountProbe.unmounts).toContain(streamingInstance)
    })

    test.each(['off', 'balanced', 'strong'] as const)(
        'retains the compact thought row through the %s stream completion',
        async (mode) => {
            const original = '<Thoughts>Reasoning</Thoughts>Answer'
            const messages = [makeMessage(0, { role: 'char', data: original })]
            const character = makeCharacter(messages, true)
            character.chats[0].activeStreamingDisplayOptimizationMode = mode
            mounted = mount(ChatsHarness, {
                target,
                props: { initialMessages: messages, initialCharacter: character },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
            const node = probeElements(target)[0]
            const instance = Number(node.dataset.chatProbe)
            ;(mounted as HarnessInstance).setStreaming(false)
            await vi.waitFor(() =>
                expect(
                    chatMountProbe.streamingUpdates.some(
                        (update) =>
                            update.instanceId === instance &&
                            !update.isOptimizedStreamingMessage,
                    ),
                ).toBe(true),
            )
            expect(probeElements(target)[0]).toBe(node)
            expect(node.dataset.message).toBe(original)
            expect(chatMountProbe.unmounts).not.toContain(instance)
        },
    )

    test('retains rows for regex name typing and refreshes pattern typing in place', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        const character = makeCharacter(messages)
        character.customscript = [{ type: 'editdisplay', in: 'before', out: 'after', comment: 'Regex', flag: '', ableFlag: false }]
        mounted = mount(ChatsRegexHarness, { target, props: { initialMessages: messages, initialCharacter: character } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        await vi.waitFor(() => expect(target.querySelector('[data-regex-editor] button')).not.toBeNull())
        target.querySelector<HTMLButtonElement>('[data-regex-editor] button')!.click()
        await domSettled()
        const inputs = target.querySelectorAll<HTMLInputElement>('[data-regex-editor] input')
        const initial = probeIdForMessage(target, 'message-0')
        inputs[0].value = 'Regex renamed'
        inputs[0].dispatchEvent(new Event('input', { bubbles: true }))
        await domSettled()
        expect(probeIdForMessage(target, 'message-0')).toBe(initial)
        expect(chatMountProbe.displayUpdates).toHaveLength(0)
        const renamed = probeIdForMessage(target, 'message-0')
        inputs[1].value = 'new pattern'
        inputs[1].dispatchEvent(new Event('input', { bubbles: true }))
        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.some((update) => update.instanceId === renamed)).toBe(true))
        expect(probeIdForMessage(target, 'message-0')).toBe(renamed)
    })

    test('refreshes image and parser dependency changes in retained rows', async () => {
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
        expect(imageInstance).toBe(initialInstance)

        ;(mounted as HarnessInstance).replaceParserDependencies()
        await domSettled()
        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.filter((update) => update.instanceId === imageInstance).length).toBeGreaterThan(1))
        expect(probeIdForMessage(target, 'message-0')).toBe(imageInstance)
    })

    test('refreshes retained rows after parser-relevant assets and scripts mutate', async () => {
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
        await domSettled()
        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.some((update) => update.instanceId === initialInstance)).toBe(true))
        const assetEditInstance = probeIdForMessage(target, 'message-0')

        ;(mounted as HarnessInstance).mutateScriptOutput('changed')
        await domSettled()
        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.filter((update) => update.instanceId === assetEditInstance)).toHaveLength(2))
        expect(assetEditInstance).toBe(initialInstance)
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
        await domSettled()
        await vi.waitFor(() => expect(imageMocks.staleReject).toBeTypeOf('function'))
        ;(mounted as HarnessInstance).setImage('latest.png')
        await domSettled()
        await vi.waitFor(() => expect(probeElements(target)[0]?.dataset.image).toBe('normal:latest.png'))

        imageMocks.staleReject?.(new Error('stale image rejected'))
        await domSettled()
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
        await domSettled()
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
        await domSettled()
        await vi.waitFor(() => expect(chatMountProbe.streamingUpdates.some((update) => (
            update.rawStreamingText === 'streamed without parser changes'
        ))).toBe(true))
        expect(scan.reads).toBe(initialReads)

        const initialInstance = probeIdForMessage(target, 'message-0')
        ;(mounted as HarnessInstance).mutateAssetTuple('changed.png')
        await domSettled()
        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.some((update) => update.instanceId === initialInstance)).toBe(true))
        expect(probeIdForMessage(target, 'message-0')).toBe(initialInstance)
        expect(scan.reads).toBeGreaterThanOrEqual(initialReads + 10_000)
    })

    test('retries a latest-pair lookup invalidated before a queued source update runs', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => makeMessage(index))
        messages[98].role = 'user'
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        let reject!: (error: unknown) => void
        let lookup = new Promise<number[]>((_resolve, fail) => { reject = fail })
        const find = vi.spyOn(source, 'findLatestMessagePair').mockImplementation(() => lookup)
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: currentCharacter, initialViewportSource: source } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const replacement = Promise.resolve([98, 99])
        lookup = replacement
        reject(new DOMException('Synthetic conversation changed', 'AbortError'))
        session.edit(session.locate(99), { ...messages[99], data: 'synthetic changed row' })
        await vi.waitFor(() => expect(find.mock.results.some(result => result.value === replacement)).toBe(true))
        expect(target.querySelector('[data-chat-history-load-error]')).toBeNull()
    })

    test('prepares by viewport height and preserves latest-pair plugin DOM across an old-history jump', async () => {
        vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockImplementation(function (this: HTMLElement) {
            return this.classList.contains('scroll-parent') ? 600 : 0
        })
        const messages = Array.from({ length: 1_000 }, (_, index) => makeMessage(index))
        const source = makePersistentViewportSource(messages)
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: makeMetadataOnlyCharacter(), initialViewportSource: source } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(5))
        const tail = target.querySelector<HTMLElement>('[data-chat-index="999"]')!
        TestResizeObserver.instances[0].emit(tail, 10_000)
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const pair = [998, 999].map(index => target.querySelector<HTMLElement>(`[data-chat-index="${index}"]`)!)
        const button = document.createElement('button')
        const clicked = vi.fn()
        button.addEventListener('click', clicked)
        pair[0].append(button)
        await expect((mounted as HarnessInstance).jumpTo(500)).resolves.toBe(true)
        expect(probeElements(target).length).toBeLessThan(16)
        expect(target.querySelector('[data-chat-index="998"]')).toBe(pair[0])
        expect(target.querySelector('[data-chat-index="999"]')).toBe(pair[1])
        button.click()
        expect(clicked).toHaveBeenCalledOnce()
        expect(source.snapshot().rowAt(998)).toBeDefined()
        expect(source.snapshot().rowAt(999)).toBeDefined()
        expect(target.querySelector('[data-chat-index="500"]')).not.toBeNull()
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

    test('prepares at most two unfinished bodies and reserves queued row height', async () => {
        chatMountProbe.holdDisplay = true
        const messages = Array.from({ length: 64 }, (_, index) =>
            makeMessage(index),
        )
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
            },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(chatMountProbe.pendingDisplays.size).toBe(2)
        const queuedRows = [
            ...target.querySelectorAll<HTMLElement>('.chat-message-container'),
        ].filter((element) => !element.querySelector('[data-chat-probe]'))
        expect(queuedRows).toHaveLength(62)
        expect(
            queuedRows.every(
                (element) =>
                    element.style.minHeight === '256px' &&
                    element.style.flexBasis === '256px',
            ),
        ).toBe(true)

        chatMountProbe.pendingDisplays.values().next().value!()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(3))
        expect(chatMountProbe.pendingDisplays.size).toBe(2)
        chatMountProbe.holdDisplay = false
        for (const settle of [...chatMountProbe.pendingDisplays.values()]) settle()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
    })

    test.each([false, true])('preserves the reader anchor (ready: %s) when a pending row becomes very tall', async ready => {
        chatMountProbe.holdDisplay = true
        const messages = Array.from({ length: 3 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(chatMountProbe.pendingDisplays.size).toBe(2))
        const readyMount = chatMountProbe.mounts.find(row => row.index === 1)!
        const anchor = target.querySelector<HTMLElement>('[data-chat-index="1"]')!
        if (ready) {
            chatMountProbe.pendingDisplays.get(readyMount.instanceId)!()
            await vi.waitFor(() => expect(anchor.dataset.chatMountPending).toBeUndefined())
        }
        const growing = target.querySelector<HTMLElement>('[data-chat-index="2"]')!
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({ top: 0, bottom: 500, height: 500 }) as DOMRect
        scrollParent.scrollBy = vi.fn().mockImplementation(({ top = 0 }: ScrollToOptions) => { scrollParent.scrollTop += top })
        for (const row of target.querySelectorAll<HTMLElement>('[data-chat-index]')) {
            row.getBoundingClientRect = () => {
                const growth = growing.dataset.chatHeightPending ? 0 : 24_000
                const top = row === anchor ? 100 - growth - scrollParent.scrollTop : -1_000 - growth
                return { top, bottom: top + 400, height: 400 } as DOMRect
            }
        }
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.scrollTop = -100
        scrollParent.dispatchEvent(new Event('scroll'))
        const offset = anchor.getBoundingClientRect().top
        const pendingMount = chatMountProbe.mounts.find(row => row.index === 2)!
        chatMountProbe.pendingDisplays.get(pendingMount.instanceId)!()
        await vi.waitFor(() => expect(growing.dataset.chatHeightPending).toBeUndefined())
        expect(anchor.getBoundingClientRect().top).toBe(offset)
        expect(scrollParent.scrollBy).toHaveBeenCalledWith({ top: -24_000, behavior: 'instant' })
        // The resulting native scroll notification must not be treated as another history step.
        scrollParent.dispatchEvent(new Event('scroll'))
        await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
        expect(anchor.isConnected).toBe(true)
        expect(anchor.getBoundingClientRect().top).toBe(offset)
    })

    test('bounds unfinished projections before message bodies mount', async () => {
        const ready = deferred<void>()
        const messages = Array.from({ length: 80 }, (_, index) =>
            makeMessage(index),
        )
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) => {
                await ready.promise
                return boundedProjection(currentCharacter, row.absoluteIndex)
            }),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })

        await vi.waitFor(() =>
            expect(resolver.resolve).toHaveBeenCalledTimes(2),
        )
        await Promise.resolve()
        expect(probeElements(target)).toHaveLength(0)

        ready.resolve()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
    })

    test('includes the greeting in the two unfinished body preparations', async () => {
        chatMountProbe.holdDisplay = true
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: currentCharacter, initialViewportSource: source } })
        await vi.waitFor(() => expect(chatMountProbe.pendingDisplays.size).toBe(2))
        expect(target.querySelectorAll('[data-chat-probe]')).toHaveLength(2)
        expect(conversationStartProbe(target)).not.toBeNull()
        await domSettled()
        expect(chatMountProbe.pendingDisplays.size).toBe(2)
        chatMountProbe.holdDisplay = false
        for (const settle of [...chatMountProbe.pendingDisplays.values()]) settle()
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
    })

    test('waits for a queued jump target to mount before resolving navigation', async () => {
        schedulingMocks.state.controlled = true
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))

        let settled = false
        const jumping = (mounted as HarnessInstance)
            .jumpTo(0)
            .then((result) => {
                settled = true
                return result
            })
        await domSettled()
        await Promise.resolve()
        expect(settled).toBe(false)

        schedulingMocks.releaseNext()
        await expect(jumping).resolves.toBe(true)
        expect(
            probeElements(target).some(
                (element) => element.dataset.message === 'message-0',
            ),
        ).toBe(true)
    })

    test('cancels queued rows when switching conversations', async () => {
        schedulingMocks.state.controlled = true
        const oldMessages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index, {
                data: `old-message-${index}`,
            }),
        )
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: oldMessages,
                initialCharacter: makeCharacter(oldMessages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const oldMountCount = chatMountProbe.mounts.length

        const nextMessages = Array.from({ length: 20 }, (_, index) =>
            makeMessage(index, {
                data: `new-message-${index}`,
            }),
        )
        const nextCharacter = makeCharacter(nextMessages)
        nextCharacter.chaId = 'next-character-id'
        nextCharacter.chats[0].id = 'next-chat-id'
        ;(mounted as HarnessInstance).switchCharacter(
            nextCharacter,
            nextMessages,
        )
        await domSettled()

        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        await vi.waitFor(() =>
            expect(
                probeElements(target).every((element) =>
                    element.dataset.message?.startsWith('new-message-'),
                ),
            ).toBe(true),
        )
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(20))
        expect(
            chatMountProbe.mounts.filter((entry) =>
                entry.message.startsWith('old-message-'),
            ),
        ).toHaveLength(oldMountCount)
    })

    test('keeps an existing row mounted until its queued replacement runs', async () => {
        const messages = Array.from({ length: 8 }, (_, index) =>
            makeMessage(index),
        )
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const oldInstance = probeIdForMessage(target, 'message-0')

        schedulingMocks.state.controlled = true
        ;(mounted as HarnessInstance).setMessages(messages.map((message) => ({ ...message, role: message.role === 'user' ? 'char' : 'user' })))
        await domSettled()
        await vi.waitFor(() =>
            expect(
                target.querySelector('[data-chat-mount-pending]'),
            ).not.toBeNull(),
        )
        expect(probeIdForMessage(target, 'message-0')).toBe(oldInstance)

        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        await vi.waitFor(() =>
            expect(probeIdForMessage(target, 'message-0')).not.toBe(
                oldInstance,
            ),
        )
    })

    test('renders absolute session rows and mirrors UI pin lifetimes through the viewport source', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        messages.at(-1)!.role = 'char'
        const currentCharacter = makeCharacter(messages, true)
        const { session, source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
            },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        expect(source.snapshot().rowAt(199)?.message.data).toBe('message-199')
        expect(source.snapshot().rowAt(0)).toBeUndefined()
        expect(session.pinCount('viewport')).toBeGreaterThan(0)
        expect(session.pinCount('streaming')).toBe(1)
        await expect((mounted as HarnessInstance).jumpTo(0)).resolves.toBe(true)
        expect(source.snapshot().rowAt(0)?.message.data).toBe('message-0')

        const oldest = probeElements(target).find(
            (element) => element.dataset.message === 'message-0',
        )!
        const editor = document.createElement('textarea')
        oldest.append(editor)
        editor.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
        expect(session.pinCount('editor')).toBe(1)

        const media = document.createElement('audio')
        oldest.append(media)
        media.dispatchEvent(new Event('play'))
        expect(session.pinCount('playing-media')).toBe(1)

        editor.dispatchEvent(new FocusEvent('focusout', { bubbles: true }))
        media.dispatchEvent(new Event('pause'))
        await vi.waitFor(() => expect(session.pinCount('editor')).toBe(0))
        expect(session.pinCount('playing-media')).toBe(0)

        await unmount(mounted)
        mounted = undefined
        expect(session.pinCount('viewport')).toBe(0)
        expect(session.pinCount('streaming')).toBe(0)
    })

    test.each(['button', 'checkbox', 'link'])(
        'applies a bot message change while its %s still has focus',
        async (kind) => {
            const messages = Array.from({ length: 8 }, (_, index) =>
                makeMessage(index),
            )
            const currentCharacter = makeCharacter(messages)
            const { session, source } = makeViewportSource(currentCharacter)
            const resolver: LiveChatParserProjectionResolver = {
                resolve: vi.fn(async ({ row }) =>
                    boundedProjection(currentCharacter, row.absoluteIndex),
                ),
            }
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialMessages: messages,
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                    parserProjectionResolver: resolver,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
            const row = probeElements(target).find(
                (element) => element.dataset.message === 'message-7',
            )!
            const control = document.createElement(
                kind === 'checkbox' ? 'input' : kind === 'link' ? 'a' : 'button',
            )
            if (control instanceof HTMLInputElement) control.type = 'checkbox'
            if (control instanceof HTMLAnchorElement) control.href = '#synthetic'
            row.append(control)
            control.focus()
            expect(document.activeElement).toBe(control)
            // Keep the focused row resident, but do not freeze it as an unsaved editor.
            expect(session.pinCount('editor')).toBe(1)
            session.edit(session.locate(7), {
                ...messages[7],
                data: 'updated bot UI',
            })
            await vi.waitFor(() =>
                expect(
                    probeElements(target).some(
                        (element) => element.dataset.message === 'updated bot UI',
                    ),
                ).toBe(true),
            )
            expect(
                probeElements(target).some(
                    (element) => element.dataset.message === 'message-7',
                ),
            ).toBe(false)
            expect(document.activeElement).toBe(control)
            control.blur()
            await vi.waitFor(() => expect(session.pinCount('editor')).toBe(0))
        },
    )

    test('preserves focused editors and playing media while source parser projections refresh', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) => boundedProjection(
                currentCharacter,
                row.absoluteIndex,
            )),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const editorRow = probeElements(target).find(
            (element) => element.dataset.message === 'message-0',
        )!
        const mediaRow = probeElements(target).find(
            (element) => element.dataset.message === 'message-1',
        )!
        const editorInstance = Number(editorRow.dataset.chatProbe)
        const mediaInstance = Number(mediaRow.dataset.chatProbe)
        const editor = document.createElement('textarea')
        editor.value = 'unsaved editor draft'
        editorRow.append(editor)
        editor.focus()
        editor.dispatchEvent(new FocusEvent('focusin', { bubbles: true }))
        const media = document.createElement('audio')
        mediaRow.append(media)
        media.dispatchEvent(new Event('play'))
        expect(session.pinCount('editor')).toBe(1)
        expect(session.pinCount('playing-media')).toBe(1)

        session.append(makeMessage(8))

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(9))
        expect(probeIdForMessage(target, 'message-0')).toBe(editorInstance)
        expect(probeIdForMessage(target, 'message-1')).toBe(mediaInstance)
        expect(editor.isConnected).toBe(true)
        expect(editor.value).toBe('unsaved editor draft')
        expect(media.isConnected).toBe(true)
        expect(session.pinCount('editor')).toBe(1)
        expect(session.pinCount('playing-media')).toBe(1)

        editor.blur()
        media.dispatchEvent(new Event('pause'))
        await vi.waitFor(() => expect(session.pinCount('editor')).toBe(0))
        expect(session.pinCount('playing-media')).toBe(0)
        await vi.waitFor(() => {
            expect(chatMountProbe.displayUpdates.some(update => update.instanceId === editorInstance)).toBe(true)
            expect(chatMountProbe.displayUpdates.some(update => update.instanceId === mediaInstance)).toBe(true)
        })
        expect(probeIdForMessage(target, 'message-0')).toBe(editorInstance)
        expect(probeIdForMessage(target, 'message-1')).toBe(mediaInstance)
    })

    test('preserves pinned row runtime across a transient parser projection retry', async () => {
        const messages = [makeMessage(0)]
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        let rejectedRefresh = false
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) => {
                if (row.absoluteIndex === 0 && row.sourceVersion > 0 && !rejectedRefresh) {
                    rejectedRefresh = true
                    throw new Error('transient projection failure')
                }
                return boundedProjection(currentCharacter, row.absoluteIndex)
            }),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
        const row = probeElements(target)[0]
        const instance = Number(row.dataset.chatProbe)
        const editor = document.createElement('textarea')
        editor.value = 'retry-safe draft'
        row.append(editor)
        editor.focus()
        const media = document.createElement('audio')
        row.append(media)
        media.dispatchEvent(new Event('play'))

        session.append(makeMessage(1))

        await vi.waitFor(() => expect(rejectedRefresh).toBe(true))
        await vi.waitFor(() => expect(
            (resolver.resolve as ReturnType<typeof vi.fn>).mock.calls.filter(
                ([input]) => input.row.absoluteIndex === 0 && input.row.sourceVersion > 0,
            ),
        ).toHaveLength(2))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(probeIdForMessage(target, 'message-0')).toBe(instance)
        expect(editor.isConnected).toBe(true)
        expect(editor.value).toBe('retry-safe draft')
        expect(media.isConnected).toBe(true)
        expect(session.pinCount('editor')).toBe(1)
        expect(session.pinCount('playing-media')).toBe(1)
    })

    test('releases a complete projection when reconciliation throws and retries', async () => {
        const messages = [makeMessage(0)]
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        const release = vi.fn()
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn()
                .mockResolvedValueOnce({
                    kind: 'complete',
                    characterId: 'character-id',
                    conversationId: 'chat-room-id',
                    revision: 1,
                    totalMessages: 1,
                    chatID: 0,
                    projectedChatID: 0,
                    historyOffset: 0,
                    reasons: ['projection-budget'],
                    release,
                })
                .mockResolvedValueOnce(boundedProjection(currentCharacter, 0)),
        }
        chatMountProbe.throwNextMount = true
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })

        await vi.waitFor(() => expect(release).toHaveBeenCalledOnce())
        await vi.waitFor(() => expect(resolver.resolve).toHaveBeenCalledTimes(2))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
    })

    test('preserves pinned row runtime across a same-conversation source replacement', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        messages[1].role = 'char'
        const firstCharacter = makeMetadataOnlyCharacter()
        firstCharacter.chats[0].isStreaming = true
        firstCharacter.chats[0].activeStreamingDisplayOptimizationMode = 'balanced'
        const firstSource = makePersistentViewportSource(messages)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) => boundedProjection(
                firstCharacter,
                row.absoluteIndex,
            )),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: firstCharacter,
                initialViewportSource: firstSource,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const editorRow = probeElements(target).find(
            (element) => element.dataset.message === 'message-0',
        )!
        const mediaRow = probeElements(target).find(
            (element) => element.dataset.message === 'message-1',
        )!
        const editorInstance = Number(editorRow.dataset.chatProbe)
        const mediaInstance = Number(mediaRow.dataset.chatProbe)
        const editor = document.createElement('textarea')
        editor.value = 'handoff draft'
        editorRow.append(editor)
        editor.focus()
        const media = document.createElement('audio')
        mediaRow.append(media)
        media.dispatchEvent(new Event('play'))

        const replacementCharacter = makeCharacter(structuredClone(messages), true)
        const { session, source: replacementSource } = makeViewportSource(replacementCharacter)
        firstSource.dispose()
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(probeIdForMessage(target, 'message-0')).toBe(editorInstance)
        expect(probeIdForMessage(target, 'message-1')).toBe(mediaInstance)
        expect(editor.isConnected).toBe(true)
        expect(editor.value).toBe('handoff draft')
        expect(media.isConnected).toBe(true)
        expect(session.pinCount('editor')).toBe(1)
        expect(session.pinCount('playing-media')).toBe(1)
        expect(session.pinCount('streaming')).toBe(1)
    })

    test('keeps an open editor pinned after it loses focus and across a source replacement', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        const firstCharacter = makeMetadataOnlyCharacter()
        const firstSource = makePersistentViewportSource(messages)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: firstCharacter,
                initialViewportSource: firstSource,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const editorRow = probeElements(target).find(
            (element) => element.dataset.message === 'message-0',
        )!
        const editorInstance = Number(editorRow.dataset.chatProbe)
        const editor = document.createElement('textarea')
        editor.value = 'blurred draft'
        editorRow.append(editor)
        editor.focus()
        chatMountProbe.activeEditors.add(editorInstance)
        // An inert page moves focus to the body without a related target.
        editor.blur()
        await Promise.resolve()
        await Promise.resolve()

        const replacementCharacter = makeCharacter(structuredClone(messages), true)
        const { session, source: replacementSource } = makeViewportSource(replacementCharacter)
        firstSource.dispose()
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(probeIdForMessage(target, 'message-0')).toBe(editorInstance)
        expect(editor.isConnected).toBe(true)
        expect(editor.value).toBe('blurred draft')
        expect(session.pinCount('editor')).toBe(1)

        chatMountProbe.activeEditors.delete(editorInstance)
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )
        await vi.waitFor(() => expect(session.pinCount('editor')).toBe(0))
    })

    test('brings an open editor back with its draft across a new navigation generation', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        const firstCharacter = makeCharacter(messages)
        const { source: firstSource } = makeViewportSource(firstCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: firstCharacter,
                initialViewportSource: firstSource,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const row = probeElements(target).find(
            (element) => element.dataset.message === 'message-0',
        )!
        const instance = Number(row.dataset.chatProbe)
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'typed draft', evidence: structuredClone(messages[0]) })
        const editor = document.createElement('textarea')
        editor.value = 'typed draft'
        row.append(editor)
        editor.focus()
        editor.setSelectionRange(2, 5)
        const media = document.createElement('audio')
        row.append(media)
        media.dispatchEvent(new Event('play'))

        const replacementCharacter = makeCharacter(structuredClone(messages))
        const { session: replacementSession, source: replacementSource } = makeViewportSource(replacementCharacter)
        ;(mounted as HarnessInstance).setViewportNavigationGeneration(1)
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )

        await vi.waitFor(() => expect(
            probeIdForMessage(target, 'message-0'),
        ).not.toBe(instance))
        expect(editor.isConnected).toBe(false)
        expect(media.isConnected).toBe(false)
        const restored = probeElements(target).find((element) => element.dataset.message === 'message-0')!
        expect(restored.dataset.restoredDraft).toBe('typed draft')
        expect(chatMountProbe.restored).toEqual([{
            instanceId: Number(restored.dataset.chatProbe),
            draft: expect.objectContaining({ kind: 'original', draft: 'typed draft', index: 0, caret: [2, 5] }),
        }])
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([])
        await vi.waitFor(() => expect(replacementSession.pinCount('editor')).toBe(1))
    })

    test('renders source rows and conversation count without reading a metadata-only shell body', async () => {
        const messages = [makeMessage(0), makeMessage(1), makeMessage(2)]
        const source = makePersistentViewportSource(messages)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: makeMetadataOnlyCharacter(),
                initialViewportSource: source,
            },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(3))
        expect(probeElements(target).map((element) => element.dataset.message)).toEqual(
            [...messages].reverse().map((entry) => entry.data),
        )
        expect(probeElements(target).every((element) => element.dataset.index !== undefined)).toBe(true)
    })

    test('shows one pane loader until the first persistent window mounts', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        const windowRead = deferred<{
            revision: number
            value: {
                characterId: string
                conversationId: string
                startIndex: number
                endIndex: number
                totalMessages: number
                messages: Message[]
                hasMoreBefore: boolean
                hasMoreAfter: boolean
            }
        }>()
        const source = new PersistentConversationViewportSource({
            reader: { readConversationWindow: vi.fn(() => windowRead.promise) },
            characterId: 'character-id',
            conversationId: 'chat-room-id',
            revision: 1,
            totalMessages: messages.length,
            rowBudget: 64,
        })
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: makeMetadataOnlyCharacter(),
                initialViewportSource: source,
            },
        })

        await vi.waitFor(() =>
            expect(
                target.querySelectorAll('[data-chat-initial-loading]'),
            ).toHaveLength(1),
        )
        windowRead.resolve({
            revision: 1,
            value: {
                characterId: 'character-id',
                conversationId: 'chat-room-id',
                startIndex: 0,
                endIndex: messages.length,
                totalMessages: messages.length,
                messages,
                hasMoreBefore: false,
                hasMoreAfter: false,
            },
        })

        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()
    })

    test('does not show an initial row loader for an empty conversation', async () => {
        const source = makePersistentViewportSource([])
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: makeMetadataOnlyCharacter(),
                initialViewportSource: source,
            },
        })

        await domSettled()
        await Promise.resolve()
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()
    })

    test('shows a recoverable initial load error and retries the current window', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        const reader = vi
            .fn()
            .mockRejectedValueOnce(new Error('synthetic read failure'))
            .mockResolvedValueOnce({
                revision: 1,
                value: {
                    characterId: 'character-id',
                    conversationId: 'chat-room-id',
                    startIndex: 0,
                    endIndex: messages.length,
                    totalMessages: messages.length,
                    messages,
                    hasMoreBefore: false,
                    hasMoreAfter: false,
                },
            })
        const source = new PersistentConversationViewportSource({
            reader: { readConversationWindow: reader },
            characterId: 'character-id',
            conversationId: 'chat-room-id',
            revision: 1,
            totalMessages: messages.length,
            rowBudget: 64,
        })
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: makeMetadataOnlyCharacter(),
                initialViewportSource: source,
            },
        })

        await vi.waitFor(() =>
            expect(
                target.querySelector('[data-chat-load-error]'),
            ).not.toBeNull(),
        )
        expect(target.querySelector('[role="status"]')).toBeNull()
        expect(reader).toHaveBeenCalledOnce()

        target
            .querySelector<HTMLButtonElement>('[data-chat-load-retry]')!
            .click()
        await vi.waitFor(() =>
            expect(probeElements(target)).toHaveLength(messages.length),
        )
        expect(reader).toHaveBeenCalledTimes(2)
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()
        expect(target.querySelector('[data-chat-load-error]')).toBeNull()
    })

    test('keeps mounted rows visible while a later persistent window is loading', async () => {
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        const earlierRead = deferred<{
            revision: number
            value: {
                characterId: string
                conversationId: string
                startIndex: number
                endIndex: number
                totalMessages: number
                messages: Message[]
                hasMoreBefore: boolean
                hasMoreAfter: boolean
            }
        }>()
        let delayedRange: { startIndex: number; limit: number } | undefined
        const source = new PersistentConversationViewportSource({
            reader: {
                readConversationWindow: vi.fn(async ({ startIndex, limit }) => {
                    if (startIndex >= 136) {
                        const endIndex = Math.min(
                            messages.length,
                            startIndex + limit,
                        )
                        return {
                            revision: 1,
                            value: {
                                characterId: 'character-id',
                                conversationId: 'chat-room-id',
                                startIndex,
                                endIndex,
                                totalMessages: messages.length,
                                messages: messages.slice(startIndex, endIndex),
                                hasMoreBefore: startIndex > 0,
                                hasMoreAfter: endIndex < messages.length,
                            },
                        }
                    }
                    delayedRange = { startIndex, limit }
                    return earlierRead.promise
                }),
            },
            characterId: 'character-id',
            conversationId: 'chat-room-id',
            revision: 1,
            totalMessages: messages.length,
            rowBudget: 64,
        })
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: makeMetadataOnlyCharacter(),
                initialViewportSource: source,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const visibleMessages = probeElements(target).map(
            (element) => element.dataset.message,
        )
        const scrollParent =
            target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () =>
            ({ top: 0, bottom: 500, height: 500 }) as DOMRect
        const gap = target.querySelector<HTMLElement>('[data-chat-gap]')!
        gap.getBoundingClientRect = () =>
            ({ top: 0, bottom: 100, height: 100 }) as DOMRect

        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.scrollTop = -100
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() => expect(delayedRange).toBeDefined())
        expect(
            probeElements(target).map((element) => element.dataset.message),
        ).toEqual(visibleMessages)
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()

        const { startIndex, limit } = delayedRange!
        const endIndex = Math.min(messages.length, startIndex + limit)
        earlierRead.resolve({
            revision: 1,
            value: {
                characterId: 'character-id',
                conversationId: 'chat-room-id',
                startIndex,
                endIndex,
                totalMessages: messages.length,
                messages: messages.slice(startIndex, endIndex),
                hasMoreBefore: startIndex > 0,
                hasMoreAfter: endIndex < messages.length,
            },
        })
        await vi.waitFor(() =>
            expect(
                probeElements(target).some(
                    (element) => element.dataset.message === 'message-135',
                ),
            ).toBe(true),
        )
    })

    test('does not mount a live parser before a source row projection is ready', async () => {
        const messages = [makeMessage(0)]
        const source = makePersistentViewportSource(messages)
        const pending = deferred<LiveChatParserProjection>()
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(() => pending.promise),
        }
        const currentCharacter = makeMetadataOnlyCharacter()
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })

        await vi.waitFor(() => expect(resolver.resolve).toHaveBeenCalledOnce())
        expect(probeElements(target)).toHaveLength(0)

        pending.resolve(boundedProjection(currentCharacter, 0))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
        expect(chatMountProbe.mounts.find((entry) => entry.index === 0)).toMatchObject({
            index: 0,
            parserProjectionKind: 'bounded',
            projectedChatID: 0,
        })
    })

    test.each([3, 67])('prepares and displays only the edited row across %s stream chunks', async (chunks) => {
        const messages = Array.from({ length: 6 }, (_, index) => makeMessage(index))
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        const resolve = vi.fn(async (input: Parameters<LiveChatParserProjectionResolver['resolve']>[0]) => {
            const seed = boundedProjection(currentCharacter, input.row.absoluteIndex)
            if (seed.kind !== 'bounded') throw new Error('Expected bounded seed')
            seed.context.parserContext.character.chats[0].message = []
            const projection = await createChatParserHistoryProjection({
                characterId: currentCharacter.chaId, conversationId: currentCharacter.chats[0].id!,
                revision: 1, totalMessages: input.totalMessages, currentAbsoluteIndex: input.row.absoluteIndex,
                maxProjectionMessages: 4, parserSource: '{{#when::1::is::1}}HP{{/when}}',
                contextSeed: seed.context, dependencies: input.dependencies, signal: input.signal, isCurrent: input.isCurrent,
                reader: { revision: 1, readResidentMessage: (index) => source.snapshot().rowAt(index)?.message,
                    readConversationWindow: async () => { throw new Error('All test rows are resident') } },
            })
            if (projection.kind !== 'bounded') throw new Error('Unexpected complete promotion')
            return projection
        })
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: currentCharacter,
            initialViewportSource: source, parserProjectionResolver: { resolve } } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(6))
        const baseline = resolve.mock.calls.length
        const unchanged = probeElements(target).filter((row) => row.dataset.index !== '5')
        const counts = unchanged.map((row) => row.dataset.refreshCount)
        for (let chunk = 1; chunk <= chunks; chunk++) {
            source.applyOptimisticRange(5, 1, [{ ...messages[5], data: `chunk ${chunk}` }])
            await vi.waitFor(() => expect(probeElements(target).find((row) => row.dataset.index === '5')?.dataset.message).toBe(`chunk ${chunk}`))
        }
        expect(resolve.mock.calls.length - baseline).toBe(chunks)
        expect(unchanged.map((row) => row.dataset.refreshCount)).toEqual(counts)
    })

    test('renews an in-flight row resolve after a silent revision advance without waiting for failure retry', async () => {
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        const messages = [makeMessage(0)]
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        let advanced = false
        const { resolve } = residentRowParserResolver(messages, source, currentCharacter, () => {
            if (advanced) return
            advanced = true
            // The current row has been captured, but its asynchronous projection has not settled.
            queueMicrotask(() => source.advanceUnchangedRevision(2))
        })
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: currentCharacter,
            initialViewportSource: source, parserProjectionResolver: { resolve } } })
        // Drain preparation without advancing the 250 ms failure retry delay.
        await settleParserProjection()
        expect(resolve).toHaveBeenCalledTimes(2)
        expect(probeElements(target).map((row) => row.dataset.message)).toEqual(['message-0'])
        expect(target.querySelector('[data-chat-load-error]')).toBeNull()
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()
        expect(chatMountProbe.mounts.filter((entry) => entry.index === 0)).toHaveLength(1)
        expect(chatMountProbe.unmounts).toEqual([])
    })

    test.each(['history edit', 'revision advance'])('prepares a row again when a later pass follows a %s after its projection', async (change) => {
        const messages = Array.from({ length: 6 }, (_, index) => makeMessage(index))
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        const { resolve, projections } = residentRowParserResolver(messages, source, currentCharacter)
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: currentCharacter,
            initialViewportSource: source, parserProjectionResolver: { resolve } } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(6))
        const stale = projections.get(2)
        if (stale?.kind !== 'bounded') throw new Error('Expected bounded projection')
        const staleSignal = chatMountProbe.mounts.filter((entry) => entry.index === 2).at(-1)?.parserAbortSignal
        const mediaRow = probeElements(target).find((row) => row.dataset.index === '2')!
        const mediaInstance = Number(mediaRow.dataset.chatProbe)
        const media = document.createElement('audio')
        mediaRow.append(media)
        media.dispatchEvent(new Event('play'))
        const bindings = () => chatMountProbe.viewportBindings.filter((entry) => entry.instanceId === mediaInstance).length
        const displays = () => chatMountProbe.displayUpdates.filter((entry) => entry.instanceId === mediaInstance)
        const boundBefore = bindings()
        const beforeResolves = resolve.mock.calls.length
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        const beforeScroll = scrollParent.scrollTop
        if (change === 'history edit') {
            // Row 2 read only itself, so editing the tail does not refresh it.
            source.applyOptimisticRange(5, 1, [{ ...messages[5], data: 'edited tail' }])
            await vi.waitFor(() => expect(probeElements(target).find((row) => row.dataset.index === '5')?.dataset.message).toBe('edited tail'))
        } else {
            source.advanceUnchangedRevision(2)
            await domSettled()
            expect(resolve).toHaveBeenCalledTimes(beforeResolves)
            expect(scrollParent.scrollTop).toBe(beforeScroll)
        }
        expect(projections.get(2)).toBe(stale)
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        await expect(stale.context.parserContext.prepareHistory!('{{lastmessage}}', 2, false))
            .rejects.toThrow('Chat parser history projection became stale')
        expect(staleSignal?.aborted).toBe(true)
        await settleParserProjection()
        expect(projections.get(2)).not.toBe(stale)
        expect(target.querySelector('[data-chat-load-error]')).toBeNull()
        vi.useRealTimers()
        const fresh = projections.get(2)
        if (fresh?.kind !== 'bounded') throw new Error('Expected bounded projection')
        expect((await fresh.context.parserContext.prepareHistory!('{{lastmessage}}', 2, false)).row(5)?.data).toBe(change === 'history edit' ? 'edited tail' : 'message-5')
        // Like a covered history change, a playing row keeps its display until the media pauses.
        await vi.waitFor(() => expect(bindings()).toBeGreaterThan(boundBefore))
        expect(displays()).toEqual([])
        expect(media.isConnected).toBe(true)
        media.dispatchEvent(new Event('pause'))
        await vi.waitFor(() => expect(displays().some((entry) => entry.signal?.aborted === false)).toBe(true))
        expect(probeElements(target).find((row) => row.dataset.index === '2')?.dataset.chatProbe).toBe(String(mediaInstance))
        expect(chatMountProbe.unmounts).toEqual([])
        if (change === 'revision advance') expect(resolve).toHaveBeenCalledTimes(beforeResolves + 1)
    })

    test('prepares a row again when a later pass reads history edited while the row was preparing', async () => {
        chatMountProbe.holdDisplay = true
        const messages = Array.from({ length: 6 }, (_, index) => makeMessage(index))
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        const { resolve, projections } = residentRowParserResolver(messages, source, currentCharacter)
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: currentCharacter,
            initialViewportSource: source, parserProjectionResolver: { resolve } } })
        await vi.waitFor(() => expect(chatMountProbe.pendingDisplays.size).toBe(2))
        const stale = projections.get(5)
        if (stale?.kind !== 'bounded') throw new Error('Expected bounded projection')
        const staleSignal = chatMountProbe.mounts.find((entry) => entry.index === 5)?.parserAbortSignal
        // Row 5 read only itself and is still preparing, so the reconcile for this edit
        // passes it by before the later pass reads the edited row.
        source.applyOptimisticRange(3, 1, [{ ...messages[3], data: 'edited row' }])
        await domSettled()
        await domSettled()
        expect(staleSignal?.aborted).toBe(false)
        await expect(stale.context.parserContext.prepareHistory!('{{previouschatlog::3}}', 5, false))
            .rejects.toThrow('Chat parser history projection became stale')
        expect(staleSignal?.aborted).toBe(true)
        chatMountProbe.holdDisplay = false
        for (const settle of [...chatMountProbe.pendingDisplays.values()]) settle()
        await vi.waitFor(() => expect(projections.get(5)).not.toBe(stale))
        const fresh = projections.get(5)
        if (fresh?.kind !== 'bounded') throw new Error('Expected bounded projection')
        expect((await fresh.context.parserContext.prepareHistory!('{{previouschatlog::3}}', 5, false)).row(3)?.data).toBe('edited row')
        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.some((entry) => entry.index === 5 && entry.signal?.aborted === false)).toBe(true))
        expect(chatMountProbe.unmounts).toEqual([])
    })

    test('keeps an unresolved retained row projection usable across a direct jump', async () => {
        const messages = Array.from({ length: 100 }, (_, index) =>
            makeMessage(index),
        )
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        const projections = new Map<
            number,
            ReturnType<typeof deferred<LiveChatParserProjection>>
        >()
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(({ row }) => {
                const pending = deferred<LiveChatParserProjection>()
                projections.set(row.absoluteIndex, pending)
                return pending.promise
            }),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(projections.has(98)).toBe(true))

        const jumping = (mounted as HarnessInstance).jumpTo(35)
        projections.get(99)!.resolve(boundedProjection(currentCharacter, 99))
        await vi.waitFor(() => expect(projections.has(35)).toBe(true))
        projections.get(35)!.resolve(boundedProjection(currentCharacter, 35))
        await expect(jumping).resolves.toBe(true)

        projections.get(98)!.resolve(boundedProjection(currentCharacter, 98))
        await vi.waitFor(() =>
            expect(
                probeElements(target).some(
                    (element) => element.dataset.message === 'message-98',
                ),
            ).toBe(true),
        )
    })

    test('releases stale and mounted complete projections exactly once', async () => {
        const messages = [makeMessage(0)]
        const firstSource = makePersistentViewportSource(messages)
        const secondSource = makePersistentViewportSource(messages)
        const first = deferred<LiveChatParserProjection>()
        const firstRelease = vi.fn()
        const secondRelease = vi.fn()
        let calls = 0
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async (): Promise<LiveChatParserProjection> => {
                calls += 1
                if (calls === 1) return first.promise
                return {
                    kind: 'complete',
                    characterId: 'character-id',
                    conversationId: 'chat-room-id',
                    revision: 1,
                    totalMessages: 1,
                    chatID: 0,
                    projectedChatID: 0,
                    historyOffset: 0,
                    reasons: [],
                    release: secondRelease,
                }
            }),
        }
        const currentCharacter = makeMetadataOnlyCharacter()
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: firstSource,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(resolver.resolve).toHaveBeenCalledOnce())

        ;(mounted as HarnessInstance).setViewportSource(secondSource)
        await domSettled()
        first.resolve({
            kind: 'complete',
            characterId: 'character-id',
            conversationId: 'chat-room-id',
            revision: 1,
            totalMessages: 1,
            chatID: 0,
            projectedChatID: 0,
            historyOffset: 0,
            reasons: ['projection-budget'],
            release: firstRelease,
        })

        await vi.waitFor(() => expect(resolver.resolve).toHaveBeenCalledTimes(2))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
        expect(firstRelease).toHaveBeenCalledOnce()
        expect(secondRelease).not.toHaveBeenCalled()

        await unmount(mounted)
        mounted = undefined
        expect(firstRelease).toHaveBeenCalledOnce()
        expect(secondRelease).toHaveBeenCalledOnce()
    })

    test('keeps the row unparsed and retries a transient projection failure', async () => {
        const messages = [makeMessage(0)]
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn()
                .mockRejectedValueOnce(new Error('transient read failure'))
                .mockResolvedValueOnce(boundedProjection(currentCharacter, 0)),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })

        await vi.waitFor(() => expect(resolver.resolve).toHaveBeenCalledOnce())
        expect(probeElements(target)).toHaveLength(0)
        await vi.waitFor(() => expect(resolver.resolve).toHaveBeenCalledTimes(2))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
        expect(chatMountProbe.mounts.find((entry) => entry.index === 0)).toMatchObject({
            parserProjectionKind: 'bounded',
        })
    })

    test('stops retrying a permanent initial parser projection failure and lets the user retry', async () => {
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        const messages = [makeMessage(0)]
        const source = makePersistentViewportSource(messages)
        const currentCharacter = makeMetadataOnlyCharacter()
        const resolve = vi
            .fn()
            .mockRejectedValue(new Error('persistent projection failure'))
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: { resolve },
            },
        })

        await settleParserProjection()
        expect(resolve).toHaveBeenCalledTimes(1)
        expect(probeElements(target)).toHaveLength(0)
        await vi.advanceTimersByTimeAsync(250)
        await settleParserProjection()
        expect(resolve).toHaveBeenCalledTimes(2)
        await vi.advanceTimersByTimeAsync(250)
        await settleParserProjection()
        expect(resolve).toHaveBeenCalledTimes(3)
        expect(target.querySelector('[data-chat-load-error]')).not.toBeNull()
        expect(probeElements(target)).toHaveLength(0)
        await expect((mounted as HarnessInstance).jumpTo(0)).resolves.toBe(false)
        await vi.advanceTimersByTimeAsync(1_000)
        await settleParserProjection()
        expect(resolve).toHaveBeenCalledTimes(3)

        resolve.mockResolvedValue(boundedProjection(currentCharacter, 0))
        target.querySelector<HTMLButtonElement>('[data-chat-load-retry]')!.click()
        await settleParserProjection()
        expect(probeElements(target)).toHaveLength(1)
        expect(resolve).toHaveBeenCalledTimes(4)
        expect(target.querySelector('[data-chat-load-error]')).toBeNull()
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()
    })

    test('keeps rendered history visible while a later parser failure waits for manual retry', async () => {
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        const messages = [makeMessage(0)]
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        let failLatest = true
        const resolve = vi.fn(async ({ row }) => {
            if (row.absoluteIndex === 1 && failLatest)
                throw new Error('persistent later projection failure')
            return boundedProjection(currentCharacter, row.absoluteIndex)
        })
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: { resolve },
            },
        })
        await settleParserProjection()
        expect(probeElements(target)).toHaveLength(1)
        session.append(makeMessage(1))

        const latestAttempts = () =>
            resolve.mock.calls.filter(([input]) => input.row.absoluteIndex === 1)
                .length
        await settleParserProjection()
        expect(latestAttempts()).toBe(1)
        await vi.advanceTimersByTimeAsync(250)
        await settleParserProjection()
        expect(latestAttempts()).toBe(2)
        await vi.advanceTimersByTimeAsync(250)
        await settleParserProjection()
        expect(latestAttempts()).toBe(3)
        expect(target.querySelector('[data-chat-load-error]')).not.toBeNull()
        expect(probeElements(target).map((row) => row.dataset.message)).toEqual([
            'message-0',
        ])
        expect(target.querySelector('[data-chat-initial-loading]')).toBeNull()
        await vi.advanceTimersByTimeAsync(1_000)
        await settleParserProjection()
        expect(latestAttempts()).toBe(3)

        failLatest = false
        target.querySelector<HTMLButtonElement>('[data-chat-load-retry]')!.click()
        await settleParserProjection()
        expect(probeElements(target)).toHaveLength(2)
        expect(latestAttempts()).toBe(4)
        expect(target.querySelector('[data-chat-load-error]')).toBeNull()
    })

    test('refreshes mounted bookmark presentation after a source metadata mutation', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const beforeSource = probeIdForMessage(target, 'message-7')
        const currentCharacter = (mounted as HarnessInstance).getCurrentCharacter()
        const { session, source } = makeViewportSource(currentCharacter)
        ;(mounted as HarnessInstance).setViewportSource(source)
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-7')).not.toBe(beforeSource))
        const retained = probeIdForMessage(target, 'message-7')
        await vi.waitFor(() => expect(
            probeElements(target).find(
                (element) => element.dataset.message === 'message-7',
            )?.dataset.bookmarked,
        ).toBe('false'))

        session.setBookmark(session.locate(7), {
            bookmarked: true,
            messageId: 'message-id-7',
            name: 'Newest',
        })

        await vi.waitFor(() => expect(
            probeElements(target).find((entry) => entry.dataset.message === 'message-7')?.dataset.bookmarked,
        ).toBe('true'))
        expect(probeElements(target).find(
            (element) => element.dataset.message === 'message-7',
        )?.dataset.bookmarked).toBe('true')
        expect(probeIdForMessage(target, 'message-7')).toBe(retained)
        expect(chatMountProbe.unmounts).not.toContain(retained)
    })

    test('updates character images without replacing an open source editor', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) => boundedProjection(currentCharacter, row.absoluteIndex)),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const editorRow = probeElements(target).find(
            (element) => element.dataset.message === 'message-6',
        )!
        const editorInstance = Number(editorRow.dataset.chatProbe)
        const editor = document.createElement('textarea')
        editor.value = 'image draft'
        editorRow.append(editor)
        chatMountProbe.activeEditors.add(editorInstance)

        ;(mounted as HarnessInstance).setImage('replacement.png')

        await vi.waitFor(() => expect(editorRow.dataset.image).toBe('normal:replacement.png'))
        expect(probeIdForMessage(target, 'message-6')).toBe(editorInstance)
        expect(chatMountProbe.unmounts).not.toContain(editorInstance)
        expect(editor.isConnected).toBe(true)
        expect(editor.value).toBe('image draft')

        chatMountProbe.activeEditors.delete(editorInstance)
        session.edit(session.locate(6), { ...messages[6], data: 'image draft' })

        await vi.waitFor(() => expect(probeIdForMessage(target, 'image draft')).toBe(editorInstance))
        expect(chatMountProbe.unmounts).not.toContain(editorInstance)
        expect(probeElements(target).find(
            (element) => element.dataset.message === 'image draft',
        )?.dataset.image).toBe('normal:replacement.png')
    })

    test('refreshes parser dependencies without replacing open editors', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index, { role: 'char' }))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const editorInstance = probeIdForMessage(target, 'message-0')
        const neighborInstance = probeIdForMessage(target, 'message-1')
        chatMountProbe.activeEditors.add(editorInstance)

        ;(mounted as HarnessInstance).replaceParserDependencies()
        await domSettled()

        await vi.waitFor(() => expect(chatMountProbe.displayUpdates.some((update) => update.instanceId === neighborInstance)).toBe(true))
        expect(probeIdForMessage(target, 'message-1')).toBe(neighborInstance)
        expect(probeIdForMessage(target, 'message-0')).toBe(editorInstance)
        expect(chatMountProbe.unmounts).not.toContain(editorInstance)

        chatMountProbe.activeEditors.delete(editorInstance)
        ;(mounted as HarnessInstance).updateMessage(0, 'saved draft')
        await domSettled()

        await vi.waitFor(() => expect(probeIdForMessage(target, 'saved draft')).toBe(editorInstance))
        expect(chatMountProbe.unmounts).not.toContain(editorInstance)
    })

    test('reconciles a deferred role change immediately when the editor closes', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        const instance = probeIdForMessage(target, 'message-0')
        chatMountProbe.activeEditors.add(instance)
        ;(mounted as HarnessInstance).setMessages(messages.map((message, index) => index === 0 ? { ...message, role: 'user' } : message))
        await domSettled()
        expect(probeIdForMessage(target, 'message-0')).toBe(instance)
        chatMountProbe.closeEditors.get(instance)!()
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(instance))
    })

    test('restores a saved history anchor after chat unmount and follows its unique message ID', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        const character = makeCharacter(messages)
        const owner = { characterId: character.chaId, conversationId: character.chats[0].id! }
        chatScreenState.writeAnchor(owner, { index: 35, messageId: messages[35].chatId, relativeOffset: -18, latest: false })
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: character } })
        await vi.waitFor(() => expect(probeElements(target).some((row) => row.dataset.message === 'message-35')).toBe(true))
        await unmount(mounted)
        mounted = null
        chatScreenState.writeAnchor(owner, { index: 35, messageId: messages[35].chatId, relativeOffset: -18, latest: false })
        const shifted = [makeMessage(-1), ...messages]
        mounted = mount(ChatsHarness, { target, props: { initialMessages: shifted, initialCharacter: makeCharacter(shifted) } })
        await vi.waitFor(() => expect(probeElements(target).find((row) => row.dataset.message === 'message-35')?.dataset.index).toBe('36'))
    })

    test.each(['editor', 'media'] as const)(
        'moves a retained %s row to its new index when an earlier message is deleted',
        async (retention) => {
            const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
            const currentCharacter = makeCharacter(messages)
            const { session, source } = makeViewportSource(currentCharacter)
            const resolver: LiveChatParserProjectionResolver = {
                resolve: vi.fn(async ({ row }) => boundedProjection(currentCharacter, row.absoluteIndex)),
            }
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialMessages: messages,
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                    parserProjectionResolver: resolver,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
            const retainedRow = probeElements(target).find(
                (element) => element.dataset.message === 'message-6',
            )!
            const retainedInstance = Number(retainedRow.dataset.chatProbe)
            const rowKey = retainedRow.closest<HTMLElement>('[data-chat-render-key]')!.dataset.chatRenderKey
            expect(retainedRow.dataset.index).toBe('6')
            if (retention === 'editor') {
                chatMountProbe.activeEditors.add(retainedInstance)
            } else {
                const media = document.createElement('audio')
                retainedRow.append(media)
                media.dispatchEvent(new Event('play'))
            }

            session.replaceRange(session.positionAt(0), 1, [])

            await vi.waitFor(() => expect(retainedRow.dataset.index).toBe('5'))
            expect(probeIdForMessage(target, 'message-6')).toBe(retainedInstance)
            expect(chatMountProbe.unmounts).not.toContain(retainedInstance)
            expect(chatMountProbe.viewportBindings).toContainEqual({
                instanceId: retainedInstance,
                index: 5,
                rowKey,
            })
            if (retention === 'media') return

            // The open editor also receives the live parser lease for its new position.
            await vi.waitFor(() => expect(chatMountProbe.displayUpdates.some((update) =>
                update.instanceId === retainedInstance &&
                update.index === 5 &&
                update.signal?.aborted === false,
            )).toBe(true))

            chatMountProbe.activeEditors.delete(retainedInstance)
            session.edit(session.locate(5), { ...messages[6], data: 'saved draft' })

            await vi.waitFor(() => expect(probeElements(target).find(
                (element) => element.dataset.message === 'saved draft',
            )?.dataset.index).toBe('5'))
        },
    )

    test('keeps the source-key wrapper when an insertion shifts its absolute index', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
            },
        })

        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-90'),
        ).toBe(true))
        const before = probeElements(target)
            .find((element) => element.dataset.message === 'message-90')!
            .closest<HTMLElement>('[data-chat-render-key]')
        expect(before).not.toBeNull()

        const retained = probeIdForMessage(target, 'message-90')
        session.replaceRange(session.positionAt(0), 0, [makeMessage(-1)])

        await vi.waitFor(() => {
            const shifted = probeElements(target)
                .find((element) => element.dataset.message === 'message-90')
            expect(shifted?.dataset.index).toBe('91')
            expect(Number(shifted?.dataset.chatProbe)).toBe(retained)
            expect(shifted?.closest('[data-chat-render-key]')).toBe(before)
        })
        session.replaceRange(session.positionAt(0), 1, [])
        await vi.waitFor(() => {
            const shifted = probeElements(target).find((element) => element.dataset.message === 'message-90')
            expect(shifted?.dataset.index).toBe('90')
            expect(Number(shifted?.dataset.chatProbe)).toBe(retained)
            expect(shifted?.closest('[data-chat-render-key]')).toBe(before)
        })
    })

    test('preserves the prior tail bottom state while a source append row is loading', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        messages.at(-1)!.role = 'char'
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        let blockLoads = false
        let releaseLoads!: () => void
        const loadBarrier = new Promise<void>((resolve) => {
            releaseLoads = resolve
        })
        const delayedSource: ConversationViewportSource = {
            snapshot: () => source.snapshot(),
            ensureRange: async (request) => {
                if (blockLoads) await loadBarrier
                if (!request.signal?.aborted) await source.ensureRange(request)
            },
            acquireRangePin: (...args) => source.acquireRangePin(...args),
            subscribe: (listener) => source.subscribe(listener),
            captureMessageTarget: (key) => source.captureMessageTarget(key),
            dispose: () => source.dispose(),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: delayedSource,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({
            top: 0,
            bottom: 500,
            height: 500,
        } as DOMRect)
        const oldTail = probeElements(target)
            .find((element) => element.dataset.message === 'message-7')!
            .closest<HTMLElement>('[data-chat-render-key]')!
        oldTail.getBoundingClientRect = () => ({
            top: 450,
            bottom: 500,
            height: 50,
        } as DOMRect)
        DBState.db.autoScrollToNewMessage = true
        blockLoads = true

        session.append(makeMessage(8, { role: 'char' }))
        await domSettled()
        await new Promise((resolve) => setTimeout(resolve, 0))

        expect((mounted as HarnessInstance).hasUnreadMessage()).toBe(false)
        releaseLoads()
    })

    test('aborts pending source loads and renders only the replacement source', async () => {
        const firstMessages = Array.from({ length: 100 }, (_, index) => makeMessage(index))
        const firstCharacter = makeCharacter(firstMessages)
        const { source: firstSource } = makeViewportSource(firstCharacter)
        const pendingSignals: AbortSignal[] = []
        let releaseLoads!: () => void
        const loadBarrier = new Promise<void>((resolve) => {
            releaseLoads = resolve
        })
        const delayedSource: ConversationViewportSource = {
            snapshot: () => firstSource.snapshot(),
            ensureRange: async (request) => {
                if (request.signal) pendingSignals.push(request.signal)
                await loadBarrier
                if (!request.signal?.aborted) await firstSource.ensureRange(request)
            },
            acquireRangePin: (...args) => firstSource.acquireRangePin(...args),
            subscribe: (listener) => firstSource.subscribe(listener),
            captureMessageTarget: (key) => firstSource.captureMessageTarget(key),
            dispose: () => firstSource.dispose(),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: firstMessages,
                initialCharacter: firstCharacter,
                initialViewportSource: delayedSource,
            },
        })
        await vi.waitFor(() => expect(pendingSignals.length).toBeGreaterThan(0))

        const replacementMessages = Array.from(
            { length: 8 },
            (_, index) => makeMessage(index, { data: `replacement-${index}` }),
        )
        const replacementCharacter = makeCharacter(replacementMessages)
        replacementCharacter.chaId = 'replacement-character'
        replacementCharacter.chats[0].id = 'replacement-chat'
        const { source: replacementSource } = makeViewportSource(replacementCharacter)
        ;(mounted as HarnessInstance).switchCharacter(replacementCharacter, replacementMessages)
        ;(mounted as HarnessInstance).setViewportSource(replacementSource)
        await domSettled()

        expect(pendingSignals.every((signal) => signal.aborted)).toBe(true)
        releaseLoads()
        await vi.waitFor(() => expect(
            probeElements(target).map((element) => element.dataset.message),
        ).toContain('replacement-7'))
        expect(target.textContent).not.toContain('message-99')
    })

    test('remaps an absolute DOM anchor across a source replacement for the same conversation', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await expect((mounted as HarnessInstance).jumpTo(100)).resolves.toBe(true)

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({
            top: 0,
            bottom: 500,
            height: 500,
        } as DOMRect)
        scrollParent.scrollBy = vi.fn()
        for (const wrapper of target.querySelectorAll<HTMLElement>('[data-chat-render-key]')) {
            const index = Number(wrapper.dataset.chatViewportIndex)
            wrapper.getBoundingClientRect = () => ({
                top: index === 101 ? 120 : 1_000,
                bottom: index === 101 ? 220 : 1_100,
                height: 100,
            } as DOMRect)
        }
        const anchorWrapper = target.querySelector<HTMLElement>(
            '[data-chat-viewport-index="101"]',
        )

        const replacementMessages = messages.map((entry, index) => ({
            ...entry,
            data: `replacement-${index}`,
        }))
        const replacementCharacter = makeCharacter(replacementMessages)
        const { source: replacementSource } = makeViewportSource(replacementCharacter)
        source.dispose()
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )

        await vi.waitFor(() => expect(
            probeElements(target).some((element) => (
                element.dataset.message === 'replacement-100' && element.dataset.index === '100'
            )),
        ).toBe(true))
        expect(target.querySelector('[data-chat-viewport-index="101"]')).toBe(anchorWrapper)
        expect(scrollParent.scrollBy).not.toHaveBeenCalled()
    })

    test('does not remap a source anchor across different conversation owners', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await expect((mounted as HarnessInstance).jumpTo(100)).resolves.toBe(true)

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({
            top: 0,
            bottom: 500,
            height: 500,
        } as DOMRect)
        scrollParent.scrollBy = vi.fn()
        const anchor = probeElements(target)
            .find((element) => element.dataset.message === 'message-100')!
            .closest<HTMLElement>('[data-chat-render-key]')!
        anchor.getBoundingClientRect = () => ({
            top: 120,
            bottom: 220,
            height: 100,
        } as DOMRect)

        const replacementMessages = messages.map((entry, index) => ({
            ...entry,
            data: `other-${index}`,
        }))
        const replacementCharacter = makeCharacter(replacementMessages)
        replacementCharacter.chaId = 'other-character'
        replacementCharacter.chats[0].id = 'other-conversation'
        const { source: replacementSource } = makeViewportSource(replacementCharacter)
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )

        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'other-199'),
        ).toBe(true))
        expect(scrollParent.scrollBy).not.toHaveBeenCalled()
    })

    test('preserves bottom auto-scroll when a replacement source appends a delayed tail row', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        messages.at(-1)!.role = 'char'
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({
            top: 0,
            bottom: 500,
            height: 500,
        } as DOMRect)
        const oldTail = probeElements(target)
            .find((element) => element.dataset.message === 'message-7')!
            .closest<HTMLElement>('[data-chat-render-key]')!
        oldTail.getBoundingClientRect = () => ({
            top: 450,
            bottom: 500,
            height: 50,
        } as DOMRect)
        const scrollIntoView = vi.spyOn(HTMLElement.prototype, 'scrollIntoView')
        DBState.db.autoScrollToNewMessage = true

        const replacementMessages = [...messages, makeMessage(8, { role: 'char' })]
        const replacementCharacter = makeCharacter(replacementMessages)
        const { source: replacementSource } = makeViewportSource(replacementCharacter)
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            replacementSource,
        )

        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-8'),
        ).toBe(true))
        await vi.waitFor(() => expect(scrollIntoView).toHaveBeenCalled(), { timeout: 1_500 })
        expect((mounted as HarnessInstance).hasUnreadMessage()).toBe(false)
    })

    test.each(['wheel', 'jump'] as const)(
        'cancels a scheduled response auto-scroll after later %s navigation',
        async (navigation) => {
            const messages = Array.from({ length: 8 }, (_, index) =>
                makeMessage(index),
            )
            const currentCharacter = makeCharacter(messages)
            const { source } = makeViewportSource(currentCharacter)
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
            const scrollParent =
                target.querySelector<HTMLElement>('.scroll-parent')!
            scrollParent.getBoundingClientRect = () =>
                ({ top: 0, bottom: 500, height: 500 }) as DOMRect
            target.querySelector<HTMLElement>(
                '[data-chat-index="7"]',
            )!.getBoundingClientRect = () =>
                ({ top: 450, bottom: 500, height: 50 }) as DOMRect
            const scrollIntoView = vi.spyOn(HTMLElement.prototype, 'scrollIntoView')
            DBState.db.autoScrollToNewMessage = true
            vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
            try {
                const schedule = vi.spyOn(globalThis, 'setTimeout')
                const replacement = makeCharacter([
                    ...messages,
                    makeMessage(8, { role: 'char' }),
                ])
                ;(mounted as HarnessInstance).switchCharacterAndSource(
                    replacement,
                    makeViewportSource(replacement).source,
                )
                await vi.waitFor(() =>
                    expect(probeElements(target)).toHaveLength(9),
                )
                expect(schedule).toHaveBeenCalledWith(expect.any(Function), 700)
                if (navigation === 'wheel') {
                    scrollParent.dispatchEvent(
                        new WheelEvent('wheel', { deltaY: -200 }),
                    )
                    scrollParent.scrollTop = -200
                    scrollParent.dispatchEvent(new Event('scroll'))
                } else {
                    await expect(
                        (mounted as HarnessInstance).jumpTo(0),
                    ).resolves.toBe(true)
                }
                scrollIntoView.mockClear()
                await vi.advanceTimersByTimeAsync(750)
                expect(scrollIntoView).not.toHaveBeenCalled()
                if (navigation === 'wheel')
                    expect(scrollParent.scrollTop).toBe(-200)
            } finally {
                vi.useRealTimers()
                scrollIntoView.mockRestore()
            }
        },
    )

    test.each([true, false])('drops an open editor draft and its popup without a dialog when the conversation is removed (%s) or left', async (removed) => {
        const messages = [makeMessage(0), makeMessage(1)]
        const character = makeCharacter(messages)
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: character } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const instance = probeIdForMessage(target, 'message-0')
        const request = { value: 'typed in popup', save: vi.fn(async () => true) }
        openTextEditorPopup(request)
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, {
            kind: 'original', draft: 'typed in popup', evidence: structuredClone(messages[0]),
            popup: { request, owner: { save: vi.fn(async () => true), input: vi.fn(), cancel: vi.fn() } },
        })
        const replacement = makeCharacter([makeMessage(2)])
        replacement.chats[0].id = 'replacement-chat'
        if (!removed) replacement.chats.push(character.chats[0])
        ;(mounted as HarnessInstance).switchCharacter(replacement, replacement.chats[0].message)
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-2')).toBe(true))
        expect(alertNormal).not.toHaveBeenCalled()
        expect(textEditorPopup.request).toBeNull()
        expect(request.save).not.toHaveBeenCalled()
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([])
        expect(chatMountProbe.restored).toEqual([])
    })

    test('brings an open editor back after a compatibility scope reset', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const instance = probeIdForMessage(target, 'message-1')
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'kept', evidence: structuredClone(messages[1]) })
        const replacement = makeCharacter(structuredClone(messages))
        ;(mounted as HarnessInstance).switchCharacter(replacement, replacement.chats[0].message)
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-1')).not.toBe(instance))
        await vi.waitFor(() => expect(probeElements(target).find((node) => node.dataset.message === 'message-1')?.dataset.restoredDraft).toBe('kept'))
        expect(chatMountProbe.restored).toHaveLength(1)
    })

    test('follows a moved message only when exactly one message carries its id', async () => {
        const messages = [makeMessage(0), makeMessage(1)]
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const instance = probeIdForMessage(target, 'message-1')
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'moved draft', evidence: structuredClone(messages[1]) })
        const shifted = [makeMessage(7), makeMessage(8), ...structuredClone(messages)]
        const replacement = makeCharacter(shifted)
        ;(mounted as HarnessInstance).switchCharacter(replacement, shifted)
        await vi.waitFor(() => expect(probeElements(target).find((node) => node.dataset.message === 'message-1')?.dataset.restoredDraft).toBe('moved draft'))
        expect(chatMountProbe.restored.map(({ draft }) => draft.index)).toEqual([3])
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([])
    })

    test.each([
        ['a repeated message id', (moved: Message) => [makeMessage(7), makeMessage(8), moved, structuredClone(moved)]],
        ['no message id', (moved: Message) => [makeMessage(7), makeMessage(8), moved]],
    ] as const)('discards a moved draft without a dialog when its target is ambiguous: %s', async (name, arrange) => {
        const edited = name === 'no message id' ? makeMessage(1, { chatId: undefined }) : makeMessage(1)
        const messages = [makeMessage(0), edited]
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(2))
        const instance = probeIdForMessage(target, 'message-1')
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'ambiguous draft', evidence: structuredClone(edited) })
        const shifted = arrange(structuredClone(edited))
        ;(mounted as HarnessInstance).switchCharacter(makeCharacter(shifted), shifted)
        await vi.waitFor(() => expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([]))
        await domSettled()
        expect(chatMountProbe.restored).toEqual([])
        expect(probeElements(target).every((node) => node.dataset.restoredDraft === undefined)).toBe(true)
        expect(alertNormal).not.toHaveBeenCalled()
    })

    test('brings an older edited message back into view when the chat is opened again', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        const character = makeCharacter(messages)
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: character } })
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true))
        await expect((mounted as HarnessInstance).jumpTo(5)).resolves.toBe(true)
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-5')).toBe(true))
        const instance = probeIdForMessage(target, 'message-5')
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'older draft', evidence: structuredClone(messages[5]) })
        await unmount(mounted)
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toHaveLength(1)
        expect(target.querySelector('[data-chat-probe]')).toBeNull()

        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: character } })
        await vi.waitFor(() => expect(probeElements(target).find((node) => node.dataset.message === 'message-5')?.dataset.restoredDraft).toBe('older draft'))
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([])
        // The view moved to the draft, not only its pinned row.
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-6')).toBe(true))
        expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true)
        expect(probeElements(target).some((node) => node.dataset.message === 'message-197')).toBe(false)
    })

    test('keeps an older windowed message with a draft loaded through a new navigation generation', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: makeMetadataOnlyCharacter(), initialViewportSource: makePersistentViewportSource(messages) } })
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true))
        await expect((mounted as HarnessInstance).jumpTo(5)).resolves.toBe(true)
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-5')).toBe(true))
        const instance = probeIdForMessage(target, 'message-5')
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'windowed draft', evidence: structuredClone(messages[5]) })
        ;(mounted as HarnessInstance).setViewportNavigationGeneration(1)
        ;(mounted as HarnessInstance).switchCharacterAndSource(makeMetadataOnlyCharacter(), makePersistentViewportSource(messages))
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-5')).not.toBe(instance))
        await vi.waitFor(() => expect(probeElements(target).find((node) => node.dataset.message === 'message-5')?.dataset.restoredDraft).toBe('windowed draft'))
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([])
    })

    test('brings an older windowed message with a draft back into view when the chat is opened again', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: makeMetadataOnlyCharacter(), initialViewportSource: makePersistentViewportSource(messages) } })
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true))
        await expect((mounted as HarnessInstance).jumpTo(5)).resolves.toBe(true)
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-5')).toBe(true))
        const instance = probeIdForMessage(target, 'message-5')
        chatMountProbe.activeEditors.add(instance)
        chatMountProbe.editorDrafts.set(instance, { kind: 'original', draft: 'windowed draft', evidence: structuredClone(messages[5]) })
        await unmount(mounted)
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toHaveLength(1)

        const reopened = makePersistentViewportSource(messages)
        const ensureRange = vi.spyOn(reopened, 'ensureRange')
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: makeMetadataOnlyCharacter(), initialViewportSource: reopened } })
        await vi.waitFor(() => expect(probeElements(target).find((node) => node.dataset.message === 'message-5')?.dataset.restoredDraft).toBe('windowed draft'))
        expect(pendingEditorDrafts('character-id', 'chat-room-id')).toEqual([])
        // The view jumped to the draft row instead of staying at the latest messages.
        await vi.waitFor(() => expect(ensureRange.mock.calls.some(([input]) => input.reason === 'jump')).toBe(true))
        const jump = ensureRange.mock.calls.find(([input]) => input.reason === 'jump')![0]
        expect(jump.startIndex).toBeLessThanOrEqual(5)
        expect(jump.startIndex + jump.limit).toBeGreaterThan(5)
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-6')).toBe(true))
        expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true)
        expect(probeElements(target).some((node) => node.dataset.message === 'message-197')).toBe(false)
    })

    test.each(['middle', 'home'] as const)('seeks directly into a distant virtual gap: %s', async (position) => {
        const messages = Array.from({ length: 10_000 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({ top: 0, bottom: 500, height: 500 } as DOMRect)
        const gap = target.querySelector<HTMLElement>('[data-chat-gap]')!
        const end = Number(gap.dataset.chatGapEnd)
        const distance = position === 'home' ? end * 256 : (end - 5_001) * 256 + 128
        const corrections = vi.fn()
        scrollParent.scrollBy = corrections
        gap.getBoundingClientRect = () => ({ top: distance - end * 256, bottom: distance, height: end * 256 } as DOMRect)
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        if (position === 'home') scrollParent.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home' }))
        scrollParent.scrollTop = -distance
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === (position === 'home' ? 'message-0' : 'message-4999'))).toBe(true))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(position === 'home' ? 63 : 64))
        if (position === 'home') expect(conversationStartProbe(target)).not.toBeNull()
        expect(target.querySelectorAll('[data-chat-probe]')).toHaveLength(64)
        for (const [options] of corrections.mock.calls) {
            expect(Math.abs((options as ScrollToOptions).top ?? 0)).toBeLessThanOrEqual(256)
        }
    })

    test('steps to message edges from the rows crossing the visible edges', async () => {
        const messages = Array.from({ length: 20 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages) } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(20))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({ top: 0, bottom: 500, height: 500 } as DOMRect)
        const scrollBy = vi.fn()
        scrollParent.scrollBy = scrollBy
        // Viewport row 10 (message 9) crosses the top edge and row 12 (message 11) the bottom edge.
        for (const row of target.querySelectorAll<HTMLElement>('[data-chat-render-key]')) {
            const top = Number(row.dataset.chatViewportIndex) * 200 - 2_050
            row.getBoundingClientRect = () => ({ top, bottom: top + 200, height: 200 } as DOMRect)
        }
        const align = vi.spyOn(HTMLElement.prototype, 'scrollIntoView')
        try {
            await expect((mounted as HarnessInstance).navigateMessage('previous')).resolves.toBe(true)
            expect(align).toHaveBeenLastCalledWith({ behavior: 'instant', block: 'start' })
            expect(align.mock.contexts.at(-1)).toBe(target.querySelector('[data-chat-viewport-index="10"]'))

            await expect((mounted as HarnessInstance).navigateMessage('next')).resolves.toBe(true)
            expect(scrollBy).toHaveBeenLastCalledWith({ top: 50, behavior: 'instant' })
            await expect((mounted as HarnessInstance).navigateMessage('next', 100)).resolves.toBe(true)
            expect(scrollBy).toHaveBeenLastCalledWith({ top: 150, behavior: 'instant' })
        } finally {
            align.mockRestore()
        }
    })

    test('jumps to the conversation start and back to the bottom of a windowed history', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: makeMetadataOnlyCharacter(), initialViewportSource: makePersistentViewportSource(messages) } })
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true))
        expect(conversationStartProbe(target)).toBeNull()
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        const align = vi.spyOn(HTMLElement.prototype, 'scrollIntoView')
        try {
            await expect((mounted as HarnessInstance).jumpToTop()).resolves.toBe(true)
            expect(conversationStartProbe(target)).not.toBeNull()
            expect(align.mock.contexts.at(-1)).toBe(target.querySelector('[data-chat-viewport-index="0"]'))
            expect(align).toHaveBeenLastCalledWith({ behavior: 'instant', block: 'start' })
        } finally {
            align.mockRestore()
        }
        expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true)
        expect(probeElements(target).some((node) => node.dataset.message === 'message-197')).toBe(false)

        scrollParent.scrollTop = -4_000
        await expect((mounted as HarnessInstance).jumpToBottom()).resolves.toBe(true)
        expect(probeElements(target).some((node) => node.dataset.message === 'message-199')).toBe(true)
        expect(scrollParent.scrollTop).toBe(0)
    })

    test('reports scrolling by the user but not its own position changes', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        const onScrollMove = vi.fn()
        mounted = mount(ChatsHarness, { target, props: { initialMessages: messages, initialCharacter: makeCharacter(messages), onScrollMove } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        await expect((mounted as HarnessInstance).jumpTo(150)).resolves.toBe(true)
        scrollParent.dispatchEvent(new Event('scroll'))
        expect(onScrollMove).not.toHaveBeenCalled()

        // Scroll anchoring moves the position without input while rows grow.
        scrollParent.scrollTop -= 100
        scrollParent.dispatchEvent(new Event('scroll'))
        expect(onScrollMove).not.toHaveBeenCalled()

        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.scrollTop -= 100
        scrollParent.dispatchEvent(new Event('scroll'))
        expect(onScrollMove).toHaveBeenCalledTimes(1)

        const now = performance.now()
        const clock = vi.spyOn(performance, 'now').mockReturnValue(now + 2_000)
        try {
            scrollParent.scrollTop -= 100
            scrollParent.dispatchEvent(new Event('scroll'))
            expect(onScrollMove).toHaveBeenCalledTimes(1)

            scrollParent.dispatchEvent(new PointerEvent('pointerdown'))
            clock.mockReturnValue(now + 10_000)
            scrollParent.scrollTop -= 100
            scrollParent.dispatchEvent(new Event('scroll'))
            expect(onScrollMove).toHaveBeenCalledTimes(2)

            window.dispatchEvent(new PointerEvent('pointerup'))
            scrollParent.scrollTop -= 100
            scrollParent.dispatchEvent(new Event('scroll'))
            expect(onScrollMove).toHaveBeenCalledTimes(2)
        } finally {
            clock.mockRestore()
        }
    })

    test('shows one recoverable later history failure while retaining readable rows', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        let failEarlier = true
        const reader = vi.fn(async ({ startIndex, limit }) => {
            if (startIndex < 136 && failEarlier) throw new Error('synthetic history failure')
            const endIndex = Math.min(startIndex + limit, messages.length)
            return { revision: 1, value: { characterId: 'character-id', conversationId: 'chat-room-id', startIndex, endIndex, totalMessages: messages.length,
                messages: messages.slice(startIndex, endIndex), hasMoreBefore: startIndex > 0, hasMoreAfter: endIndex < messages.length } }
        })
        const source = new PersistentConversationViewportSource({ reader: { readConversationWindow: reader }, characterId: 'character-id', conversationId: 'chat-room-id', revision: 1, totalMessages: messages.length, rowBudget: 64 })
        mounted = mount(ChatsHarness, { target, props: { initialCharacter: makeMetadataOnlyCharacter(), initialViewportSource: source } })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const rows = probeElements(target)
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () => ({ top: 0, bottom: 500, height: 500 } as DOMRect)
        const gap = target.querySelector<HTMLElement>('[data-chat-gap]')!
        gap.getBoundingClientRect = () => ({ top: 0, bottom: 100, height: 100 } as DOMRect)
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.scrollTop = -100
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() => expect(target.querySelector('[data-chat-load-error]')).not.toBeNull())
        const attempts = reader.mock.calls.length
        expect(attempts).toBe(2)
        for (let i = 0; i < 3; i++) {
            scrollParent.scrollTop -= 10
            scrollParent.dispatchEvent(new Event('scroll'))
            await domSettled()
        }
        expect(reader).toHaveBeenCalledTimes(attempts)
        expect(probeElements(target)).toEqual(rows)
        failEarlier = false
        target.querySelector<HTMLButtonElement>('[data-chat-load-retry]')!.click()
        await vi.waitFor(() => expect(probeElements(target).some((node) => node.dataset.message === 'message-135')).toBe(true))
        expect(target.querySelector('[data-chat-load-error]')).toBeNull()
    })

    test('bounds retained height corrections while visiting a long conversation', async () => {
        const messages = Array.from({ length: 2_000 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))

        const observer = TestResizeObserver.instances.at(-1)!
        for (const index of [0, 300, 600, 900, 1_200, 1_500, 1_800]) {
            await (mounted as HarnessInstance).jumpTo(index)
            for (const row of target.querySelectorAll<HTMLElement>('[data-chat-render-key]')) {
                observer.emit(row, 200 + (Number(row.dataset.chatViewportIndex) % 7))
            }
            await domSettled()
        }

        const chatBody = target.querySelector<HTMLElement>('[data-chat-measured-height-count]')!
        expect(Number(chatBody.dataset.chatMeasuredHeightCount)).toBeLessThanOrEqual(256)
        expect(Number(chatBody.dataset.chatKeyLookupScans)).toBe(0)
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

        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.scrollTop = -100
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-135'),
        ).toBe(true))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))

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
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
    })

    test.each([
        'layout',
        'handoff',
        'wheel',
        'keyboard',
        'touch',
        'scrollbar',
        'jump',
        'text-tab',
        'text-arrow',
    ])(
        'keeps the initial latest message visible through delayed layout until %s navigation',
        async (intent) => {
            const frames = new Map<number, FrameRequestCallback>()
            let nextFrame = 1
            vi.stubGlobal(
                'requestAnimationFrame',
                (callback: FrameRequestCallback) => {
                    const id = nextFrame++
                    frames.set(id, callback)
                    return id
                },
            )
            vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
            const messages = Array.from({ length: 128 }, (_, index) =>
                makeMessage(index),
            )
            const currentCharacter = makeCharacter(messages)
            const { source } = makeViewportSource(currentCharacter)
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialMessages: messages,
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
            const scrollParent =
                target.querySelector<HTMLElement>('.scroll-parent')!
            scrollParent.getBoundingClientRect = () =>
                ({ top: 0, bottom: 500, height: 500 }) as DOMRect
            scrollParent.scrollBy = vi
                .fn()
                .mockImplementation(({ top = 0 }: ScrollToOptions) => {
                    scrollParent.scrollTop += top
                })
            let latestHeight = 300
            for (const row of target.querySelectorAll<HTMLElement>(
                '[data-chat-index]',
            )) {
                const index = Number(row.dataset.chatIndex)
                row.getBoundingClientRect = () =>
                    ({
                        top:
                            500 -
                            latestHeight -
                            (127 - index) * 200 -
                            scrollParent.scrollTop,
                        bottom: 500 - (127 - index) * 200 - scrollParent.scrollTop,
                        height: index === 127 ? latestHeight : 300,
                    }) as DOMRect
            }
            for (const gap of target.querySelectorAll<HTMLElement>(
                '[data-chat-gap]',
            )) {
                gap.getBoundingClientRect = () =>
                    ({ top: -10_000, bottom: -9_000, height: 1_000 }) as DOMRect
            }
            const latest = target.querySelector<HTMLElement>(
                '[data-chat-index="127"]',
            )!
            const observer = TestResizeObserver.instances[0]
            observer.emit(latest, 300)
            if (intent === 'wheel')
                scrollParent.dispatchEvent(
                    new WheelEvent('wheel', { deltaY: -100 }),
                )
            if (intent === 'keyboard')
                scrollParent.dispatchEvent(
                    new KeyboardEvent('keydown', { key: 'PageUp' }),
                )
            if (intent === 'touch')
                scrollParent.dispatchEvent(new Event('touchmove'))
            if (intent === 'scrollbar')
                scrollParent.dispatchEvent(new PointerEvent('pointerdown'))
            if (intent === 'text-tab' || intent === 'text-arrow') {
                const editor = document.createElement('textarea')
                latest.appendChild(editor)
                editor.dispatchEvent(
                    new KeyboardEvent('keydown', {
                        key: intent === 'text-tab' ? 'Tab' : 'ArrowUp',
                        bubbles: true,
                    }),
                )
            }
            if (intent === 'handoff') {
                const replacementCharacter = makeCharacter(
                    structuredClone(messages),
                )
                const { source: replacementSource } =
                    makeViewportSource(replacementCharacter)
                source.dispose()
                ;(mounted as HarnessInstance).switchCharacterAndSource(
                    replacementCharacter,
                    replacementSource,
                )
                await domSettled()
            }
            const jump =
                intent === 'jump'
                    ? (mounted as HarnessInstance).jumpTo(100)
                    : undefined
            // WebView scroll anchoring and genuine input both dispatch scroll events.
            // Only the explicit input above transfers ownership away from initial layout.
            scrollParent.scrollTop = -1_981
            scrollParent.dispatchEvent(new Event('scroll'))
            for (let step = 0; step < 4; step++) {
                await domSettled()
                for (const [id, callback] of [...frames]) {
                    frames.delete(id)
                    callback(performance.now())
                }
            }
            await domSettled()
            if (jump) {
                let completed: boolean | undefined
                void jump.then((result) => {
                    completed = result
                })
                await vi.waitFor(async () => {
                    await domSettled()
                    for (const [id, callback] of [...frames]) {
                        frames.delete(id)
                        callback(performance.now())
                    }
                    expect(completed).toBe(true)
                })
            }
            if (
                intent === 'layout' ||
                intent === 'handoff' ||
                intent === 'text-arrow'
            ) {
                expect(scrollParent.scrollTop).toBe(0)
                expect(latest.getBoundingClientRect().top).toBeLessThan(500)
                scrollParent.scrollTop = -159
                latestHeight = 600
                observer.emit(latest, 600)
                for (const [id, callback] of [...frames]) {
                    frames.delete(id)
                    callback(performance.now())
                }
                expect(scrollParent.scrollTop).toBe(0)
            } else {
                expect(scrollParent.scrollTop).toBe(-1_981)
            }
        },
    )

    test('updates the greeting avatar without releasing its parser admission', async () => {
        const messages = [makeMessage(0)]
        const release = vi.fn()
        const acquireGreeting = vi.fn(async () => ({ release }))
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
                acquireConversationStartParserLease: acquireGreeting,
            },
        })
        await vi.waitFor(() =>
            expect(conversationStartProbe(target)?.dataset.image).toBe(
                'normal:character.png',
            ),
        )
        const greeting = conversationStartProbe(target)
        ;(mounted as HarnessInstance).setImage('new-avatar.png')
        await vi.waitFor(() =>
            expect(conversationStartProbe(target)?.dataset.image).toBe(
                'normal:new-avatar.png',
            ),
        )
        expect(conversationStartProbe(target)).toBe(greeting)
        expect(acquireGreeting).toHaveBeenCalledOnce()
        expect(release).not.toHaveBeenCalled()
    })

    test('refreshes every row after a message edit without removing the displayed components', async () => {
        const messages = Array.from({ length: 12 }, (_, index) =>
            makeMessage(index),
        )
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) =>
                boundedProjection(currentCharacter, row.absoluteIndex),
            ),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(12))
        const nodes = [...probeElements(target)]
        const priorUnmounts = chatMountProbe.unmounts.length
        session.edit(session.locate(11), {
            ...messages[11],
            data: 'new last message',
        })
        await vi.waitFor(() =>
            expect(chatMountProbe.displayUpdates).toHaveLength(12),
        )
        expect(chatMountProbe.displayUpdates[0].index).toBe(11)
        expect(
            new Set(chatMountProbe.displayUpdates.map((update) => update.index))
                .size,
        ).toBe(12)
        expect(probeElements(target)).toEqual(nodes)
        expect(chatMountProbe.unmounts).toHaveLength(priorUnmounts)
        expect(
            probeElements(target).some(
                (node) => node.dataset.message === 'new last message',
            ),
        ).toBe(true)

        chatMountProbe.displayUpdates = []
        session.edit(session.locate(2), {
            ...messages[2],
            data: 'edited older message',
        })
        await vi.waitFor(() =>
            expect(chatMountProbe.displayUpdates).toHaveLength(12),
        )
        expect(probeElements(target)).toEqual(nodes)
        expect(
            probeElements(target).some(
                (node) => node.dataset.message === 'edited older message',
            ),
        ).toBe(true)
        expect(chatMountProbe.unmounts).toHaveLength(priorUnmounts)
    })

    test('updates the response candidate position of a retained source row refreshed in place', async () => {
        const messages = Array.from({ length: 5 }, (_, index) => makeMessage(index))
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) =>
                boundedProjection(currentCharacter, row.absoluteIndex),
            ),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(5))
        const tail = probeElements(target).find((node) => node.dataset.message === 'message-4')!
        const tailInstance = Number(tail.dataset.chatProbe)
        expect(tail.dataset.candidatePage).toBe('1')
        expect(tail.dataset.candidateTotal).toBe('1')

        // A new candidate is stored with the same text, so only its position changes.
        const { chatId: _generatedId, ...snapshot } = messages[4]
        session.edit(session.locate(4), {
            ...messages[4],
            chatId: 'response-group',
            responseVariants: {
                groupId: 'response-group',
                selectedId: 'candidate-b',
                candidates: [
                    { id: 'candidate-a', messages: [{ ...snapshot, data: 'earlier candidate' }] },
                    { id: 'candidate-b', messages: [snapshot] },
                ],
            },
        })
        await vi.waitFor(() => expect(
            chatMountProbe.displayUpdates.some((update) => update.instanceId === tailInstance),
        ).toBe(true))
        await domSettled()

        expect(probeElements(target).find((node) => node.dataset.message === 'message-4')).toBe(tail)
        expect(chatMountProbe.unmounts).not.toContain(tailInstance)
        expect(tail.dataset.candidatePage).toBe('2')
        expect(tail.dataset.candidateTotal).toBe('2')
    })

    test.each(['balanced', 'strong'] as const)(
        'refreshes the retained %s streaming row with a live parser signal on every session update',
        async (mode) => {
            const messages = [makeMessage(0, { data: '' })]
            const currentCharacter = makeCharacter(messages, true)
            currentCharacter.chats[0].activeStreamingDisplayOptimizationMode = mode
            const { session, source } = makeViewportSource(currentCharacter)
            const resolver: LiveChatParserProjectionResolver = {
                resolve: vi.fn(async ({ row }) =>
                    boundedProjection(currentCharacter, row.absoluteIndex),
                ),
            }
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                    parserProjectionResolver: resolver,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(1))
            const node = probeElements(target)[0]
            let previousSignal = chatMountProbe.mounts.find(
                (entry) => entry.index === 0,
            )!.parserAbortSignal!
            for (const data of ['first chunk', 'first chunk second chunk']) {
                session.edit(session.locate(0), { ...messages[0], data })
                await vi.waitFor(() => expect(node.dataset.streamingText).toBe(data))
                const update = chatMountProbe.displayUpdates.at(-1)
                expect(previousSignal.aborted).toBe(true)
                expect(update?.message).toBe(data)
                expect(update?.signal).toBeDefined()
                expect(update?.signal?.aborted).toBe(false)
                previousSignal = update!.signal!
                expect(probeElements(target)[0]).toBe(node)
            }
            ;(mounted as HarnessInstance).setStreaming(false)
            await vi.waitFor(() => expect(probeElements(target)[0]).not.toBe(node))
            expect(probeElements(target)[0].dataset.message).toBe('first chunk second chunk')
        },
    )

    test('restarts queued source refreshes with the newest tail and cancels older parser jobs', async () => {
        const messages = Array.from({ length: 12 }, (_, index) =>
            makeMessage(index),
        )
        const currentCharacter = makeCharacter(messages)
        const { session, source } = makeViewportSource(currentCharacter)
        const resolver: LiveChatParserProjectionResolver = {
            resolve: vi.fn(async ({ row }) =>
                boundedProjection(currentCharacter, row.absoluteIndex),
            ),
        }
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(12))
        const nodes = [...probeElements(target)]
        schedulingMocks.state.controlled = true
        session.edit(session.locate(11), {
            ...messages[11],
            data: 'superseded tail',
        })
        await vi.waitFor(() =>
            expect(schedulingMocks.pending.length).toBeGreaterThan(0),
        )
        schedulingMocks.releaseNext()
        await vi.waitFor(() =>
            expect(chatMountProbe.displayUpdates).toHaveLength(2),
        )
        expect(chatMountProbe.displayUpdates.map((update) => update.index)).toEqual(
            [11, 10],
        )
        const previousSignals = chatMountProbe.displayUpdates.map(
            (update) => update.signal,
        )
        session.edit(session.locate(11), { ...messages[11], data: 'latest tail' })
        await vi.waitFor(() =>
            expect(previousSignals.every((signal) => signal?.aborted)).toBe(true),
        )
        schedulingMocks.state.controlled = false
        schedulingMocks.releaseAll()
        await vi.waitFor(() =>
            expect(chatMountProbe.displayUpdates).toHaveLength(14),
        )
        const next = chatMountProbe.displayUpdates.slice(2)
        expect(next.map((update) => update.index)).toEqual([
            11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0,
        ])
        expect(next[0].message).toBe('latest tail')
        expect(next.every((update) => !update.signal?.aborted)).toBe(true)
        expect(probeElements(target)).toEqual(nodes)
    })

    test('refreshes unchanged messages and greeting without unmounting on a global display reload', async () => {
        const messages = Array.from({ length: 8 }, (_, index) => makeMessage(index))
        const acquireGreeting = vi.fn(async () => null)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
                acquireConversationStartParserLease: acquireGreeting,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(8))
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledOnce())
        const previousRows = [...probeElements(target)]
        const previousGreeting = conversationStartProbe(target)
        const unmounts = chatMountProbe.unmounts.length
        ReloadGUIPointer.update((value) => value + 1)
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledTimes(2))
        await vi.waitFor(() =>
            expect(
                probeElements(target).every(
                    (row) => Number(row.dataset.refreshCount) > 0,
                ),
            ).toBe(true),
        )
        expect(probeElements(target)).toEqual(previousRows)
        expect(conversationStartProbe(target)).toBe(previousGreeting)
        expect(chatMountProbe.unmounts).toHaveLength(unmounts)
    })

    test.each(['append', 'tail'])('keeps the greeting mounted and refreshes its history after %s changes', async (change) => {
        const messages = [makeMessage(0)]
        const source = change === 'tail' ? makePersistentViewportSource(messages) : undefined
        const acquireGreeting = vi.fn(async (request: import('src/ts/selectedConversationLiveParserProjection').LiveChatParserConversationStartRequest) => {
            captureResidentCbsHistory(messages, planChatParserHistory({ source: '{{lastmessage}}' }), -1, request.dependencies)
            return null
        })
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
                initialViewportSource: source,
                acquireConversationStartParserLease: acquireGreeting,
            },
        })
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledOnce())
        await vi.waitFor(() => expect(conversationStartProbe(target)).not.toBeNull())
        const greeting = conversationStartProbe(target)
        const unmounts = chatMountProbe.unmounts.length
        if (source) source.applyOptimisticRange(0, 1, [{ ...messages[0], data: 'streamed tail' }])
        else (mounted as HarnessInstance).setMessages([...messages, makeMessage(1)])
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledTimes(2))
        expect(acquireGreeting.mock.calls[1][0].totalMessages).toBe(source ? 1 : 2)
        expect(conversationStartProbe(target)).toBe(greeting)
        expect(chatMountProbe.unmounts).toHaveLength(unmounts)
    })

    test('rechecks greeting admission when in-place parser scripts or global modules reload', async () => {
        const messages = [makeMessage(0)]
        const currentCharacter = makeCharacter(messages)
        currentCharacter.customscript = [
            { type: 'editdisplay', in: 'synthetic', out: 'plain' } as any,
        ]
        const acquireGreeting = vi.fn(async () => null)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: currentCharacter,
                acquireConversationStartParserLease: acquireGreeting,
            },
        })
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledOnce())
        ;(mounted as HarnessInstance).mutateScriptOutput('{{history}}')
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledTimes(2))
        ReloadGUIPointer.update((revision) => revision + 1)
        await vi.waitFor(() => expect(acquireGreeting).toHaveBeenCalledTimes(3))
    })

    test('starts a different conversation at the latest message instead of retaining reverse-flex scroll offset', async () => {
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        const initialCharacter = makeCharacter(messages)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter,
                initialViewportSource: makePersistentViewportSource(messages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.scrollTop = -332
        const replacementCharacter = makeCharacter(messages)
        replacementCharacter.chaId = 'replacement-owner'
        ;(mounted as HarnessInstance).switchCharacterAndSource(
            replacementCharacter,
            makePersistentViewportSource(messages),
        )

        await vi.waitFor(() => expect(scrollParent.scrollTop).toBe(0))
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        expect(
            probeElements(target).some(
                (row) => row.dataset.message === 'message-199',
            ),
        ).toBe(true)
        expect(conversationStartProbe(target)).toBeNull()
    })

    test('resumes scrolling after a conversation switch cancels an anchor correction', async () => {
        const pendingFrames = new Map<number, FrameRequestCallback>()
        let nextFrame = 1
        vi.stubGlobal(
            'requestAnimationFrame',
            vi.fn((callback: FrameRequestCallback) => {
                const frame = nextFrame++
                pendingFrames.set(frame, callback)
                return frame
            }),
        )
        vi.stubGlobal(
            'cancelAnimationFrame',
            vi.fn((frame: number) => pendingFrames.delete(frame)),
        )
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () =>
            ({ top: 0, bottom: 500, height: 500 }) as DOMRect
        scrollParent.scrollBy = vi.fn()
        const wrappers = [
            ...target.querySelectorAll<HTMLElement>('[data-chat-render-key]'),
        ]
        const anchor = wrappers.find(
            (element) => element.dataset.chatIndex === '190',
        )!
        let anchorTop = 100
        for (const wrapper of wrappers) {
            wrapper.getBoundingClientRect = () =>
                ({
                    top: wrapper === anchor ? anchorTop : 1_000,
                    bottom: wrapper === anchor ? anchorTop + 100 : 1_100,
                    height: 100,
                }) as DOMRect
        }
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.dispatchEvent(new Event('scroll'))
        anchorTop = 150
        ;(mounted as HarnessInstance).replaceParserDependencies()
        await domSettled()
        expect(scrollParent.scrollBy).toHaveBeenCalledWith({
            top: 50,
            behavior: 'instant',
        })
        expect(pendingFrames.size).toBeGreaterThan(0)

        const replacementMessages = messages.map((message, index) => ({
            ...message,
            data: `replacement-${index}`,
        }))
        const replacementCharacter = makeCharacter(replacementMessages)
        replacementCharacter.chaId = 'replacement-owner'
        ;(mounted as HarnessInstance).switchCharacter(
            replacementCharacter,
            replacementMessages,
        )
        await vi.waitFor(() =>
            expect(
                probeElements(target).some(
                    (element) => element.dataset.message === 'replacement-199',
                ),
            ).toBe(true),
        )
        expect(pendingFrames.size).toBe(0)

        const gap = target.querySelector<HTMLElement>('[data-chat-gap]')!
        gap.getBoundingClientRect = () =>
            ({ top: 0, bottom: 100, height: 100 }) as DOMRect
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
        scrollParent.scrollTop = -100
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() =>
            expect(
                probeElements(target).some(
                    (element) => element.dataset.message === 'replacement-135',
                ),
            ).toBe(true),
        )
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

        ;(mounted as HarnessInstance).setMessages(messages.map((message, index) => index === 0 ? { ...message, role: 'user' } : message))
        await vi.waitFor(() => expect(probeIdForMessage(target, 'message-0')).not.toBe(originalInstance))
        expect(media.isConnected).toBe(false)
        expect(chatMountProbe.unmounts).toContain(originalInstance)

        await (mounted as HarnessInstance).jumpToLatestMessage()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-0'),
        ).toBe(false))
    })

    test('keeps the latest pair after streaming settles during an old-history jump', async () => {
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
        await domSettled()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-199'),
        ).toBe(true))
    })

    test('does not add an app scroll correction for a measured content size change', async () => {
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        await (mounted as HarnessInstance).jumpTo(100)

        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        const wrappers = [
            ...target.querySelectorAll<HTMLElement>('[data-chat-render-key]'),
        ]
        const anchor = wrappers.find(
            (element) => element.dataset.chatIndex === '100',
        )!
        let anchorTop = 120
        scrollParent.getBoundingClientRect = () =>
            ({
                top: 0,
                bottom: 500,
                height: 500,
            }) as DOMRect
        scrollParent.scrollBy = vi.fn()
        for (const wrapper of wrappers) {
            wrapper.getBoundingClientRect = () =>
                ({
                    top: wrapper === anchor ? anchorTop : 1_000,
                    bottom: wrapper === anchor ? anchorTop + 100 : 1_100,
                    height: 100,
                }) as DOMRect
        }

        // Record the reading position after installing this synthetic layout.
        for (const gap of target.querySelectorAll<HTMLElement>('[data-chat-gap]')) {
            gap.getBoundingClientRect = () =>
                ({ top: -1_000, bottom: -900, height: 100 }) as DOMRect
        }
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -1 }))
        scrollParent.scrollTop = -1
        scrollParent.dispatchEvent(new Event('scroll'))
        TestResizeObserver.instances[0].emit(anchor, 100)
        anchorTop = 170

        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
        await domSettled()
        expect(scrollParent.scrollBy).not.toHaveBeenCalled()
        expect(anchor.getBoundingClientRect().top).toBe(170)
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
        await domSettled()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message === 'message-0'),
        ).toBe(false))

        const replacement = makeMessage(0, { data: 'replacement-message-0' })
        const withReusedId = [replacement, ...withoutOldMessage]
        ;(mounted as HarnessInstance).setMessages(withReusedId)
        await domSettled()
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
        await vi.waitFor(() => expect(pendingFrames.size).toBeGreaterThan(0))
        await unmount(mounted)
        mounted = undefined

        await expect(jumping).resolves.toBe(false)
        expect(pendingFrames.size).toBe(0)
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
        await domSettled()
        const newMessages = Array.from({ length: 200 }, (_, index) => makeMessage(index, {
            data: `new-owner-message-${index}`,
        }))
        const newCharacter = makeCharacter(newMessages)
        newCharacter.chaId = 'new-owner-id'
        ;(mounted as HarnessInstance).switchCharacter(newCharacter, newMessages)
        await domSettled()
        await vi.waitFor(() => expect(
            probeElements(target).some((element) => element.dataset.message?.startsWith('new-owner-message-')),
        ).toBe(true))

        for (const [frame, callback] of [...pendingFrames]) {
            pendingFrames.delete(frame)
            callback(0)
        }
        await expect(jumping).resolves.toBe(false)
    })

    test.each([
        ['edit', 'parser'],
        ['append', 'parser'],
        ['edit', 'layout'],
        ['append', 'layout'],
    ] as const)(
        'rejects a jump when %s changes the source while its target %s is pending',
        async (change, stage) => {
            const messages = Array.from({ length: 200 }, (_, index) =>
                makeMessage(index),
            )
            const currentCharacter = makeCharacter(messages)
            const { session, source } = makeViewportSource(currentCharacter)
            const pending = deferred<LiveChatParserProjection>()
            const resolver: LiveChatParserProjectionResolver = {
                resolve: vi.fn(({ row }) =>
                    row.absoluteIndex === 100 && row.sourceVersion === 0
                        ? pending.promise
                        : Promise.resolve(
                              boundedProjection(
                                  currentCharacter,
                                  row.absoluteIndex,
                              ),
                          ),
                ),
            }
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                    parserProjectionResolver: resolver,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
            const frames = new Map<number, FrameRequestCallback>()
            let nextFrame = 0
            if (stage === 'layout') {
                vi.stubGlobal(
                    'requestAnimationFrame',
                    (callback: FrameRequestCallback) => {
                        frames.set(++nextFrame, callback)
                        return nextFrame
                    },
                )
                vi.stubGlobal('cancelAnimationFrame', (id: number) =>
                    frames.delete(id),
                )
            }
            const align = vi.spyOn(HTMLElement.prototype, 'scrollIntoView')
            try {
                const jumping = (mounted as HarnessInstance).jumpTo(100)
                await vi.waitFor(() =>
                    expect(resolver.resolve).toHaveBeenCalledWith(
                        expect.objectContaining({
                            row: expect.objectContaining({
                                absoluteIndex: 100,
                                sourceVersion: 0,
                            }),
                        }),
                    ),
                )
                if (stage === 'layout') {
                    pending.resolve(boundedProjection(currentCharacter, 100))
                    await vi.waitFor(() => {
                        expect(
                            target.querySelector(
                                '[data-chat-probe][data-index="100"]',
                            ),
                        ).not.toBeNull()
                        expect(frames.size).toBeGreaterThan(0)
                    })
                }
                if (change === 'edit')
                    session.edit(
                        session.locate(100),
                        makeMessage(100, { data: 'changed while mounting' }),
                    )
                else session.append(makeMessage(200))
                pending.resolve(boundedProjection(currentCharacter, 100))
                for (const [id, callback] of [...frames]) {
                    frames.delete(id)
                    callback(performance.now())
                }
                await expect(jumping).resolves.toBe(false)
                expect(align).not.toHaveBeenCalled()
            } finally {
                pending.resolve(boundedProjection(currentCharacter, 100))
                align.mockRestore()
            }
        },
    )

    test('allows continued scrolling when a failed jump supersedes a pending anchor correction', async () => {
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        const currentCharacter = makeCharacter(messages)
        const { source } = makeViewportSource(currentCharacter)
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: currentCharacter,
                initialViewportSource: source,
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () =>
            ({ top: 0, bottom: 500, height: 500 }) as DOMRect
        scrollParent.scrollBy = vi
            .fn()
            .mockImplementation(({ top = 0 }: ScrollToOptions) => {
                scrollParent.scrollTop += top
            })
        const anchor = target.querySelector<HTMLElement>('[data-chat-index="190"]')!
        let anchorTop = 120
        for (const row of target.querySelectorAll<HTMLElement>(
            '[data-chat-index]',
        )) {
            row.getBoundingClientRect = () =>
                ({
                    top: row === anchor ? anchorTop : 1_000,
                    bottom: row === anchor ? anchorTop + 100 : 1_100,
                    height: 100,
                }) as DOMRect
        }
        for (const gap of target.querySelectorAll<HTMLElement>('[data-chat-gap]')) {
            gap.getBoundingClientRect = () =>
                ({ top: -1_000, bottom: -900, height: 100 }) as DOMRect
        }
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -1 }))
        scrollParent.scrollTop = -1
        scrollParent.dispatchEvent(new Event('scroll'))
        const pendingFrames = new Map<number, FrameRequestCallback>()
        let nextFrame = 0
        vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
            pendingFrames.set(++nextFrame, callback)
            return nextFrame
        })
        vi.stubGlobal('cancelAnimationFrame', (frame: number) =>
            pendingFrames.delete(frame),
        )
        anchorTop = 170
        ;(mounted as HarnessInstance).replaceParserDependencies()
        await domSettled()
        expect(scrollParent.scrollBy).toHaveBeenCalledWith({
            top: 50,
            behavior: 'instant',
        })
        expect(pendingFrames.size).toBeGreaterThan(0)
        vi.spyOn(source, 'ensureRange').mockRejectedValueOnce(
            new Error('synthetic jump load failure'),
        )
        await expect((mounted as HarnessInstance).jumpTo(0)).resolves.toBe(false)
        for (const [frame, callback] of [...pendingFrames]) {
            pendingFrames.delete(frame)
            callback(0)
        }

        const gap = target.querySelector<HTMLElement>('[data-chat-gap]')!
        gap.getBoundingClientRect = () =>
            ({ top: 0, bottom: 100, height: 100 }) as DOMRect
        scrollParent.scrollTop = -200
        scrollParent.dispatchEvent(new Event('scroll'))
        await vi.waitFor(() =>
            expect(
                probeElements(target).some(
                    (element) => element.dataset.message === 'message-135',
                ),
            ).toBe(true),
        )
    })

    test.each([
        [0, 'complete'],
        [100, 'complete'],
        [0, 'jump'],
        [100, 'jump'],
        [0, 'wheel'],
        [100, 'wheel'],
    ] as const)(
        'retains delayed jump target %i through competing layout until %s navigation',
        async (index, completion) => {
            const messages = Array.from({ length: 240 }, (_, messageIndex) =>
                makeMessage(messageIndex),
            )
            const currentCharacter = makeCharacter(messages)
            const { source } = makeViewportSource(currentCharacter)
            const pending = deferred<LiveChatParserProjection>()
            const resolver: LiveChatParserProjectionResolver = {
                resolve: vi.fn(({ row }) =>
                    row.absoluteIndex === index
                        ? pending.promise
                        : Promise.resolve(
                              boundedProjection(
                                  currentCharacter,
                                  row.absoluteIndex,
                              ),
                          ),
                ),
            }
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialCharacter: currentCharacter,
                    initialViewportSource: source,
                    parserProjectionResolver: resolver,
                },
            })
            await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
            const scrollParent =
                target.querySelector<HTMLElement>('.scroll-parent')!
            scrollParent.getBoundingClientRect = () =>
                ({ top: 0, bottom: 500, height: 500 }) as DOMRect
            const originalRect = HTMLElement.prototype.getBoundingClientRect
            const rectSpy = vi
                .spyOn(HTMLElement.prototype, 'getBoundingClientRect')
                .mockImplementation(function (this: HTMLElement) {
                    if (this.dataset.chatViewportIndex === undefined)
                        return originalRect.call(this)
                    const top =
                        this.dataset.chatIndex !== undefined &&
                        Number(this.dataset.chatIndex) === index + 30
                            ? 120
                            : 1_000
                    return { top, bottom: top + 100, height: 100 } as DOMRect
                })
            try {
                let result: boolean | undefined
                const jumping = (mounted as HarnessInstance)
                    .jumpTo(index)
                    .then((value) => {
                        result = value
                        return value
                    })
                await vi.waitFor(() =>
                    expect(resolver.resolve).toHaveBeenCalledWith(
                        expect.objectContaining({
                            row: expect.objectContaining({ absoluteIndex: index }),
                        }),
                    ),
                )
                await vi.waitFor(() =>
                    expect(
                        probeElements(target).some(
                            (element) =>
                                element.dataset.message === `message-${index + 30}`,
                        ),
                    ).toBe(true),
                )
                const competitor = target.querySelector<HTMLElement>(
                    `[data-chat-index="${index + 30}"]`,
                )!
                TestResizeObserver.instances[0].emit(competitor, 100)
                await new Promise<void>((resolve) =>
                    requestAnimationFrame(() => resolve()),
                )
                await domSettled()
                expect(result).toBeUndefined()
                expect(
                    target.querySelector(`[data-chat-index="${index}"]`),
                ).not.toBeNull()

                if (completion === 'jump') {
                    await expect(
                        (mounted as HarnessInstance).jumpTo(239),
                    ).resolves.toBe(true)
                } else if (completion === 'wheel') {
                    scrollParent.dispatchEvent(
                        new WheelEvent('wheel', { deltaY: -200 }),
                    )
                    scrollParent.scrollTop = -200
                    scrollParent.dispatchEvent(new Event('scroll'))
                }
                pending.resolve(boundedProjection(currentCharacter, index))
                await vi.waitFor(() =>
                    expect(result).toBe(completion === 'complete'),
                )
                await expect(jumping).resolves.toBe(completion === 'complete')
                if (completion === 'complete') {
                    expect(
                        probeElements(target).some(
                            (element) =>
                                element.dataset.message === `message-${index}`,
                        ),
                    ).toBe(true)
                }
            } finally {
                pending.resolve(boundedProjection(currentCharacter, index))
                rectSpy.mockRestore()
            }
        },
    )

    test.each([false, true])('keeps row DOM and native anchoring through a publication and delayed content growth (streaming: %s)', async streaming => {
        const messages = Array.from({ length: 200 }, (_, index) =>
            makeMessage(index),
        )
        messages.at(-1)!.role = 'char'
        mounted = mount(ChatsHarness, {
            target,
            props: {
                initialMessages: messages,
                initialCharacter: makeCharacter(messages, streaming),
            },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const scrollParent = target.querySelector<HTMLElement>('.scroll-parent')!
        scrollParent.getBoundingClientRect = () =>
            ({ top: 0, bottom: 500, height: 500 }) as DOMRect
        scrollParent.scrollBy = vi
            .fn()
            .mockImplementation(({ top = 0 }: ScrollToOptions) => {
                scrollParent.scrollTop += top
            })
        const anchor = target.querySelector<HTMLElement>('[data-chat-index="190"]')!
        const anchorKey = anchor.dataset.chatRenderKey
        const growingRow = target.querySelector<HTMLElement>(
            '[data-chat-index="199"]',
        )!
        let growth = 0
        for (const row of target.querySelectorAll<HTMLElement>(
            '[data-chat-index]',
        )) {
            row.getBoundingClientRect = () => {
                const top =
                    -100 +
                    (Number(row.dataset.chatIndex) - 190) * 400 -
                    growth -
                    scrollParent.scrollTop
                const height = row === growingRow ? 400 + growth : 400
                return { top, bottom: top + height, height } as DOMRect
            }
        }
        for (const gap of target.querySelectorAll<HTMLElement>('[data-chat-gap]')) {
            gap.getBoundingClientRect = () =>
                ({ top: -10_000, bottom: -9_000, height: 1_000 }) as DOMRect
        }
        scrollParent.dispatchEvent(new WheelEvent('wheel', { deltaY: -200 }))
        scrollParent.scrollTop = -200
        scrollParent.dispatchEvent(new Event('scroll'))
        const readerOffset = anchor.getBoundingClientRect().top
        expect(readerOffset).toBe(100)

        // A publication must leave native anchoring available for the later body layout.
        scrollParent.style.overflowAnchor = 'auto'
        ;(mounted as HarnessInstance).setMessages(messages.map((message, index) =>
            index === 199 ? { ...message, data: 'growing response' } : message,
        ))
        await domSettled()
        expect(scrollParent.style.overflowAnchor).toBe('auto')
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))

        // ResizeObserver runs after layout has already moved the content.
        growth = 80
        expect(anchor.getBoundingClientRect().top).toBe(20)
        TestResizeObserver.instances[0].emit(growingRow, 480)
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
        await domSettled()
        expect(target.querySelector(`[data-chat-render-key="${anchorKey}"]`)).toBe(
            anchor,
        )
        expect(anchor.getBoundingClientRect().top).toBe(readerOffset - growth)
        expect(scrollParent.scrollBy).not.toHaveBeenCalled()
    })

    test('keeps gaps and plugin children without geometry reads on size-only updates', async () => {
        const messages = Array.from({ length: 200 }, (_, index) => makeMessage(index))
        mounted = mount(ChatsHarness, {
            target,
            props: { initialMessages: messages, initialCharacter: makeCharacter(messages) },
        })
        await vi.waitFor(() => expect(probeElements(target)).toHaveLength(64))
        const gap = target.querySelector<HTMLElement>('[data-chat-gap]')!
        const row = target.querySelector<HTMLElement>('[data-chat-index="199"]')!
        const pluginControl = document.createElement('button')
        row.append(pluginControl)
        const read = vi.spyOn(row, 'getBoundingClientRect')
        const mounts = chatMountProbe.mounts.length
        TestResizeObserver.instances[0].emit(row, 10_000)
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
        await domSettled()
        expect(target.querySelector('[data-chat-gap]')).toBe(gap)
        expect(row.contains(pluginControl)).toBe(true)
        expect(chatMountProbe.mounts).toHaveLength(mounts)
        expect(read).not.toHaveBeenCalled()
    })
})
