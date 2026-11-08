// @vitest-environment happy-dom

import { fromStore, get, writable } from 'svelte/store'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { ReloadChatPointer, ReloadGUIPointer } from 'src/ts/stores.svelte'
import { ActiveConversationSession } from 'src/ts/storage/activeConversationSession'
import type { character, Chat as ChatRecord, Message } from 'src/ts/storage/database.svelte'
import type { ConversationViewportKey } from 'src/ts/conversationViewportSource'
import type { SelectedConversationOperations } from 'src/ts/selectedConversationOperations'
import { createSelectedConversationOperations } from 'src/ts/selectedConversationOperations'
import { SynchronousSessionConversationViewportSource } from 'src/ts/conversationViewportSource'
import type { SelectedConversationTarget } from 'src/ts/storage/activeWorkingSet.svelte'
import { cancelTextEditorPopup, textEditorPopup } from 'src/ts/gui/textEditorPopup.svelte'
import { discardEditorDraftsExcept, keepEditorDraft, pendingEditorDrafts, type ChatEditorDraft } from 'src/ts/chatEditorDrafts'
import { chatScreenState } from 'src/ts/ui/chatScreenState.svelte'
import { language } from 'src/lang'

const live = vi.hoisted(() => ({
    db: {} as Record<string, any>,
    reactive: false,
}))
const runtime = vi.hoisted(() => ({
    activeSession: null as unknown,
    persistent: {} as Record<string, unknown>,
}))
const actionMocks = vi.hoisted(() => {
    const hold = { held: false }
    return {
        hold,
        runTrigger: vi.fn(),
        runLuaInteractionTrigger: vi.fn(),
        holdScriptRun: vi.fn(async <T>(operation: () => Promise<T>): Promise<T> => {
            hold.held = true
            try { return await operation() } finally { hold.held = false }
        }),
    }
})
const parserCalls = vi.hoisted(() => [] as Array<{
    chara?: unknown
    role?: string
    chatID?: number
    projectedChatID?: number
    historyOffset?: number
}>)

class TestIntersectionObserver {
    static instance: TestIntersectionObserver | undefined

    constructor(private readonly callback: IntersectionObserverCallback) {
        TestIntersectionObserver.instance = this
    }

    observe = vi.fn()
    unobserve = vi.fn()
    disconnect = vi.fn()
    takeRecords = () => []
    readonly root = null
    readonly rootMargin = ''
    readonly thresholds = [0]

    setVisible(element: Element) {
        this.callback([{
            target: element,
            isIntersecting: true,
            intersectionRatio: 1,
        } as IntersectionObserverEntry], this as unknown as IntersectionObserver)
    }
}

vi.mock('../SideBars/Scripts/RegexData.svelte', () => ({ default: () => {} }))
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('./ChatBody.svelte', async () => ({
    default: (await import('./ChatBodyCaptureProbe.test.svelte')).default,
}))
vi.mock('src/ts/stores.svelte', () => {
    let plainDatabase = live.db
    const database = fromStore(writable(plainDatabase))
    Object.defineProperty(live, 'db', {
        get: () => plainDatabase,
        set: value => {
            plainDatabase = value
            if (live.reactive) database.current = value
        },
    })
    return {
    DBState: { get db() { return live.reactive ? database.current : live.db } },
    ReloadChatPointer: writable([]),
    CurrentTriggerIdStore: writable(null),
    popupStore: writable(null),
    alertStore: writable({ type: 'none', msg: '' }),
    selectedCharID: writable(0),
    HideIconStore: writable(false),
    ReloadGUIPointer: writable(0),
    ScriptDisplayRefreshPointer: writable(0),
    selIdState: { selId: 0 },
    createSimpleCharacter: (char: character) => ({ ...char, type: 'simple' }),
    }
})
vi.mock('src/ts/characters', () => ({ getCharImage: async () => '' }))
vi.mock('src/ts/gui/colorscheme', () => ({ ColorSchemeTypeStore: writable('light') }))
vi.mock('src/ts/globalApi.svelte', () => ({
    aiLawApplies: () => false,
    changeChatTo: vi.fn(),
    foldChatToMessage: vi.fn(),
    getFileSrc: vi.fn(async (source: string) => source),
    createChatCopyName: vi.fn(),
}))
vi.mock('src/ts/process/scripts', async () => {
    const { getChatVar } = await import('src/ts/parser/chatVar.svelte')
    return {
    risuChatParser: (value: string, arg: {
        chara?: any
        role?: string
        chatID?: number
        projectedChatID?: number
    } = {}) => {
        parserCalls.push({
            chara: arg.chara,
            role: arg.role,
            chatID: arg.chatID,
            projectedChatID: arg.projectedChatID,
            historyOffset: (arg as any).historyOffset,
        })
        value = value.replace(/\{\{getvar::([^}]+)\}\}/g, (_, key: string) => getChatVar(key))
        if (!value.includes('{{char}}')) return value
        if (typeof arg.chara === 'string') return value.replaceAll('{{char}}', arg.chara)
        return value.replaceAll('{{char}}', arg.chara?.name ?? '')
    },
    }
})
vi.mock('src/ts/model/modellist', () => ({
    getModelInfo: (model: string) => ({ shortName: model || 'model' }),
}))
vi.mock('src/ts/process/scriptings', () => ({
    runLuaInteractionTrigger: actionMocks.runLuaInteractionTrigger,
}) satisfies Partial<typeof import('src/ts/process/scriptings')>)
vi.mock('src/ts/process/triggers', () => ({ runTrigger: actionMocks.runTrigger }))
vi.mock('src/ts/process/generationBackground', async importOriginal => ({
    ...await importOriginal<typeof import('src/ts/process/generationBackground')>(),
    holdScriptRun: actionMocks.holdScriptRun,
}))
vi.mock('src/ts/process/tts', () => ({ sayTTS: vi.fn() }))
vi.mock('src/ts/util', () => ({
    capitalize: (value: string) => value,
    getUserIcon: () => '',
    getUserName: () => 'Live User',
    sleep: () => Promise.resolve(),
}))
vi.mock('../../ts/alert', () => ({
    alertClear: vi.fn(), alertConfirm: vi.fn(), alertError: vi.fn(), alertInput: vi.fn(), alertNormal: vi.fn(),
    alertRequestData: vi.fn(), alertWait: vi.fn(), alertToast: vi.fn(),
}))
vi.mock('../../ts/translator/translator', () => ({ getLLMCache: vi.fn(), setLLMCache: vi.fn() }))
vi.mock('src/ts/process/files/inlayRenderSource', () => ({
    DeferredInlayMarkerRegistry: class {}, withResolvedDeferredInlaySources: vi.fn(),
}))
vi.mock('src/ts/process/files/chatCopyInlays', () => ({ copyImageSourceToDataUrl: vi.fn() }))
vi.mock('../../ts/storage/persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: vi.fn(),
    capturePersistentMutationToken: vi.fn(),
    getActiveConversationSession: () => runtime.activeSession,
    getPersistentDataRuntime: () => runtime.persistent,
}))

import type { ProcessScriptCaptureContext } from 'src/ts/process/scripts'
import Chat from './Chat.svelte'
import Chats from './Chats.svelte'
import { alertToast } from 'src/ts/alert'
import ChatCaptureBatchHarness from './ChatCaptureBatchHarness.test.svelte'
import ChatsHarness from './ChatsHarness.test.svelte'
import { probedDisplayRows } from './ChatBodyCaptureProbe.test.svelte'
import { chatViewEvents } from 'src/ts/plugins/chatViewHost.svelte'
import { requestScriptDisplayRefresh } from 'src/ts/process/scriptDisplayRefresh'
import type { ChatViewEvent } from 'src/ts/plugins/chatViewEvents'

function context(overrides: Record<string, unknown> = {}) {
    const character = {
        type: 'character' as const,
        name: 'Frozen Character',
        chaId: 'frozen',
        chatPage: 0,
        chats: [{ message: [], note: '', name: '', localLore: [], bookmarks: [] }],
        customscript: [],
    }
    return {
        character: null,
        characterName: 'Frozen Character',
        characterImageSource: '',
        characterLargePortrait: false,
        userName: 'Frozen User',
        userImageSource: '',
        userLargePortrait: false,
        moduleAssets: [],
        presetRegex: [],
        moduleRegexScripts: [],
        assetStyle: '',
        parserContext: {
            database: { characters: [character] } as any,
            character: character as any,
            userName: 'Frozen User',
            personaPrompt: 'Frozen Persona',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
        totalTurns: 4,
        selectionStart: 1,
        firstParserMessageIndex: 0,
        settings: {
            autoTranslate: false,
            autoTranslateCachedOnly: false,
            translatorType: 'google',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
            assetWidth: -1,
            hideAllImages: false,
            iconSize: 100,
            zoomSize: 100,
            lineHeight: 1.25,
            dynamicAssets: false,
            dynamicAssetsEditDisplay: false,
            legacyMediaFindings: false,
            assetMaxDifference: 0.5,
            theme: '',
            guiHTML: '',
            roundIcons: false,
            hideIcons: false,
            proseInvert: false,
            requestInfoInsideChat: false,
            aiLawApplies: false,
            translator: '',
            showFirstMessagePages: true,
            memoryLimitThickness: 1,
            customQuotes: false,
            customQuotesData: ['“', '”', '‘', '’'] as [string, string, string, string],
            unformatQuotes: false,
            blockquoteStyling: false,
            ...overrides,
        },
    }
}

function makeWindowedEditHarness() {
    const message: Message = {
        role: 'char',
        data: 'Original viewport message',
        chatId: 'message-1',
    }
    const completeConversation = {
        id: 'conversation-a',
        name: 'Conversation',
        note: '',
        localLore: [],
        message: [{ role: 'user' as const, data: 'zero' }, message],
        bookmarks: [],
    } as ChatRecord
    const completeCharacter = {
        type: 'character',
        name: 'Live Character',
        chaId: 'character-a',
        chatPage: 0,
        chats: [completeConversation],
        ttsMode: 'none',
    } as unknown as character
    const metadataConversation = {
        id: completeConversation.id,
        bookmarks: [],
    } as unknown as character['chats'][number]
    Object.defineProperty(metadataConversation, 'message', {
        get() {
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    const metadataCharacter = {
        ...completeCharacter,
        chats: [metadataConversation],
    }
    const session = new ActiveConversationSession({
        characterId: completeCharacter.chaId,
        conversationId: completeConversation.id,
        conversation: completeConversation,
        storeRevision: 7,
    })
    const release = vi.fn()
    const intent = {
        selection: {
            characterId: completeCharacter.chaId,
            conversationId: completeConversation.id,
            navigationGeneration: 1,
            storeRevision: 7,
        },
        absoluteIndex: 1,
        sourceToken: 'source-a',
        sourceVersion: 3,
        rowKey: 'row-1' as ConversationViewportKey,
        messageEvidence: message,
    }
    const captureMessageEditIntent = vi.fn(() => intent)
    // Like the real operation, a row whose message no longer matches the evidence keeps the intent.
    const rebindMessageEditIntent = vi.fn((
        retained: typeof intent,
        input: Omit<typeof intent, 'selection' | 'messageEvidence'> & { message: Message },
    ) => input.message.data === retained.messageEvidence.data
        ? {
            ...retained,
            absoluteIndex: input.absoluteIndex,
            sourceToken: input.sourceToken,
            sourceVersion: input.sourceVersion,
            rowKey: input.rowKey,
        }
        : retained)
    const acquireTarget = vi.fn(async () => {
        live.db.characters = [completeCharacter]
        runtime.activeSession = session
        const locator = session.locate(1)
        return {
            target: {
                kind: 'session' as const,
                absoluteIndex: 1,
                character: completeCharacter,
                conversation: completeConversation,
                message: session.readMessage(locator),
                session,
                locator,
            },
            release,
        }
    })
    const acquireCompleteMessageTargetForIntent = acquireTarget
    const acquireCompleteMessageTarget = acquireTarget
    const withCompleteSelectedConversation = vi.fn()
    const operations = {
        captureMessageEditIntent,
        rebindMessageEditIntent,
        acquireMessageMutation: acquireTarget,
        acquireCompleteMessageTargetForIntent,
        acquireCompleteMessageTarget,
        withCompleteSelectedConversation,
    } as unknown as SelectedConversationOperations
    return {
        message,
        intent,
        metadataCharacter,
        completeConversation,
        operations,
        captureMessageEditIntent,
        rebindMessageEditIntent,
        acquireCompleteMessageTargetForIntent,
        acquireCompleteMessageTarget,
        completeCharacter,
        session,
        withCompleteSelectedConversation,
        release,
    }
}

describe('Chat frozen capture presentation', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        chatScreenState.clear()
        live.reactive = false
        parserCalls.length = 0
        actionMocks.runTrigger.mockReset()
        actionMocks.runLuaInteractionTrigger.mockReset()
        live.db = {
            theme: 'cardboard',
            iconsize: 25,
            zoomsize: 25,
            lineHeight: 3,
            roundIcons: true,
            memoryLimitThickness: 9,
            characters: [],
        }
        runtime.activeSession = null
        runtime.persistent = {}
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        try {
            if (mounted) await unmount(mounted)
        } finally {
            mounted = undefined
            textEditorPopup.request = null
            discardEditorDraftsExcept(null, null)
            vi.useRealTimers()
            document.body.replaceChildren()
            TestIntersectionObserver.instance = undefined
            vi.unstubAllGlobals()
        }
    })

    test.each(['off', 'balanced', 'strong'] as const)(
        'bypasses the initial CBS parser for the %s thought preview and resumes it on completion',
        async (mode) => {
            live.db.streamingDeferDisplayProcessing = true
            const original = '<Thoughts>FULL ORIGINAL THOUGHT</Thoughts>Answer'
            mounted = mount(Chat, {
                target,
                props: {
                    message: original,
                    rawStreamingText: original,
                    role: 'char',
                    idx: -1,
                    name: 'Synthetic',
                    isLastMemory: false,
                    isOptimizedStreamingMessage: true,
                    streamingOptimizationMode: mode,
                },
            })
            await tick()
            expect(
                target.querySelector('[data-chat-body-probe]')?.textContent,
            ).toBe('FULL ORIGINAL THOUGHT')
            expect(parserCalls).toHaveLength(0)
            ;(mounted as ReturnType<typeof Chat>).updateStreamingDisplay({
                isOptimizedStreamingMessage: false,
                streamingOptimizationMode: mode,
                rawStreamingText: original,
            })
            await tick()
            expect(
                target.querySelector('[data-chat-body-probe]')?.textContent,
            ).toBe(original)
            expect(parserCalls.length).toBeGreaterThan(0)
        },
    )

    test.each(['off', 'balanced', 'strong'] as const)(
        'keeps full capture rendering even when %s streaming preview props are supplied',
        async (mode) => {
            live.db.streamingDeferDisplayProcessing = true
            const original = '<Thoughts>Full capture reasoning</Thoughts>Answer'
            mounted = mount(Chat, {
                target,
                props: {
                    message: original,
                    rawStreamingText: original,
                    role: 'char',
                    idx: -1,
                    name: 'Synthetic',
                    isLastMemory: false,
                    isOptimizedStreamingMessage: true,
                    streamingOptimizationMode: mode,
                    captureContext: context() as any,
                },
            })
            await tick()
            expect(
                target.querySelector('[data-chat-body-probe]')?.textContent,
            ).toBe(original)
            expect(
                target
                    .querySelector('[data-chat-body-probe]')
                    ?.getAttribute('data-thought-preview'),
            ).toBe('false')
        },
    )

    test.each(['recent', 'collapsed', 'off'] as const)(
        'keeps CBS enabled independently of the %s thought view',
        async (mode) => {
            live.db.streamingThoughtMode = mode
            const original = '<Thoughts>Reasoning</Thoughts>Answer'
            mounted = mount(Chat, {
                target,
                props: {
                    message: original,
                    rawStreamingText: original,
                    role: 'char',
                    idx: -1,
                    name: 'Synthetic',
                    isLastMemory: false,
                    isOptimizedStreamingMessage: true,
                    streamingOptimizationMode: 'strong',
                },
            })
            await tick()
            const probe = target.querySelector('[data-chat-body-probe]')
            expect(probe?.textContent).toBe(original)
            expect(probe?.getAttribute('data-thought-mode')).toBe(mode)
            expect(probe?.getAttribute('data-raw-preview')).toBe('false')
            expect(parserCalls.length).toBeGreaterThan(0)
        },
    )

    test('can defer display effects without any dedicated thought handling', async () => {
        live.db.streamingThoughtMode = 'off'
        live.db.streamingDeferDisplayProcessing = true
        mounted = mount(Chat, {
            target,
            props: {
                message: '**Plain answer**',
                rawStreamingText: '**Plain answer**',
                role: 'char',
                idx: -1,
                name: 'Synthetic',
                isLastMemory: false,
                isOptimizedStreamingMessage: true,
                streamingOptimizationMode: 'off',
            },
        })
        await tick()
        const probe = target.querySelector('[data-chat-body-probe]')
        expect(probe?.getAttribute('data-thought-mode')).toBe('off')
        expect(probe?.getAttribute('data-raw-preview')).toBe('true')
        expect(parserCalls).toHaveLength(0)
    })

    test('keeps the existing live rendering when compact thoughts are disabled', async () => {
        live.db.streamingThoughtMode = 'off'
        const original = '<Thoughts>Reasoning</Thoughts>Answer'
        mounted = mount(Chat, {
            target,
            props: {
                message: original,
                rawStreamingText: original,
                role: 'char',
                idx: -1,
                name: 'Synthetic',
                isLastMemory: false,
                isOptimizedStreamingMessage: true,
                streamingOptimizationMode: 'balanced',
            },
        })
        await tick()
        expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe(
            original,
        )
        expect(parserCalls.length).toBeGreaterThan(0)
    })

    test.each(['cardboard', 'mobilechat', 'customHTML'])(
        'renders a greeting without reading metadata-only history (%s)',
        async (theme) => {
            const harness = makeWindowedEditHarness()
            live.db = {
                ...live.db,
                theme,
                guiHTML: '<div><RISUTEXTBOX></RISUTEXTBOX></div>',
                characters: [harness.metadataCharacter],
                translator: '',
                useChatCopy: false,
                enableBookmark: false,
            }
            mounted = mount(Chat, {
                target,
                props: {
                    message: 'Synthetic greeting',
                    name: 'Live Character',
                    role: 'char',
                    idx: -1,
                    firstMessage: true,
                    totalLength: 2,
                    isLastMemory: false,
                },
            })
            await vi.waitFor(() =>
                expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe(
                    'Synthetic greeting',
                ),
            )
            expect(target.querySelector('[data-chat-id]')?.getAttribute('data-chat-id')).toBe('')
            expect(harness.withCompleteSelectedConversation).not.toHaveBeenCalled()
        },
    )

    test('shows a greeting variable again only when a script changes that variable', async () => {
        const scriptstate = { $hp: '10', $mood: 'calm' }
        live.db = {
            ...live.db,
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live',
                chatPage: 0,
                chats: [{ id: 'greeting', message: [], scriptstate }],
            }],
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: 'HP {{getvar::hp}}',
                name: 'Live Character',
                role: 'char',
                idx: -1,
                firstMessage: true,
                totalLength: 0,
                isLastMemory: false,
            },
        })
        const shown = () => target.querySelector('[data-chat-body-probe]')?.textContent
        await vi.waitFor(() => expect(shown()).toBe('HP 10'))

        scriptstate.$hp = '4'
        scriptstate.$mood = 'tense'
        const parsesBefore = parserCalls.length
        requestScriptDisplayRefresh({ keys: ['mood'] })
        await Promise.resolve()
        await tick()
        expect(parserCalls).toHaveLength(parsesBefore)
        expect(shown()).toBe('HP 10')

        requestScriptDisplayRefresh({ keys: ['hp'] })
        await vi.waitFor(() => expect(shown()).toBe('HP 4'))
    })

    test('renders normal branch comment presentation from the frozen message', async () => {
        const message = {
            role: 'char' as const,
            data: '{{specialcomment::branchedfrom::chat-id::Frozen Branch::message-id::}}',
            isComment: true,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen Character',
                role: 'char',
                idx: 1,
                totalLength: 4,
                isLastMemory: false,
                isComment: true,
                captureMessage: message,
                captureContext: context(),
            },
        })

        await vi.waitFor(() => expect(target.querySelector('button')?.textContent).toContain('Frozen Branch'))
        expect(target.querySelector('[data-chat-body-probe]')).toBeNull()
    })

    test('keeps the frozen mobile theme and timestamp after live presentation state changes', async () => {
        const timestamp = Date.UTC(2024, 0, 2, 3, 4, 5)
        const frozen = context({ theme: 'mobilechat' })
        live.db.theme = 'customHTML'
        live.db.guiHTML = '<div>Live custom</div>'
        const message = { role: 'user' as const, data: 'Frozen body', time: timestamp }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen User',
                role: 'user',
                idx: 2,
                totalLength: 4,
                isLastMemory: false,
                captureMessage: message,
                captureContext: frozen,
            },
        })

        await vi.waitFor(() => expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe('Frozen body'))
        expect(target.querySelector('.bg-gray-100')).not.toBeNull()
        expect(target.textContent).not.toContain('Live custom')
        expect(target.querySelector('.text-xs')?.textContent?.trim()).not.toBe('')
    })

    test('renders frozen custom HTML with the normal text box slot', async () => {
        const frozen = context({
            theme: 'customHTML',
            guiHTML: '<div class="capture-custom"><span>Frozen layout</span><a href="#top">Top</a><a href="https://synthetic.invalid/">Web</a><RISUTEXTBOX></RISUTEXTBOX></div>',
        })
        live.db.theme = 'mobilechat'
        const message = { role: 'char' as const, data: 'Frozen custom body' }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen Character',
                role: 'char',
                idx: 2,
                totalLength: 4,
                isLastMemory: false,
                captureMessage: message,
                captureContext: frozen,
            },
        })

        await vi.waitFor(() => expect(target.querySelector('.capture-custom')).not.toBeNull())
        expect(target.textContent).toContain('Frozen layout')
        const links = [...target.querySelectorAll('.capture-custom a')]
        expect(links.map((link) => [link.textContent, link.getAttribute('href')])).toEqual([['Top', null], ['Web', 'https://synthetic.invalid/']])
        expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe('Frozen custom body')
    })

    test('keeps generation information and all-before boundary presentation', async () => {
        const frozen = context({ requestInfoInsideChat: true })
        const message = {
            role: 'char' as const,
            data: 'Generated',
            disabled: 'allBefore' as const,
            generationInfo: { model: 'Frozen Model' },
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Frozen Character',
                role: 'char',
                idx: 3,
                totalLength: 4,
                isLastMemory: false,
                disabled: 'allBefore',
                messageGenerationInfo: message.generationInfo,
                captureMessage: message,
                captureContext: frozen,
            },
        })

        await vi.waitFor(() => expect(target.textContent).toContain('Frozen Model'))
        expect(target.querySelector('.border-amber-500')).not.toBeNull()
    })

    test('uses the mounted viewport row for live presentation without recapturing its array index', async () => {
        const timestamp = Date.UTC(2024, 5, 6, 7, 8, 9)
        const message = {
            role: 'char' as const,
            data: 'Viewport body',
            chatId: 'viewport-message',
            time: timestamp,
        }
        const indexReads = vi.fn(() => {
            throw new Error('live message index was recaptured')
        })
        const messages = new Proxy([] as typeof message[], {
            get(target, property, receiver) {
                if (property === '3') return indexReads()
                return Reflect.get(target, property, receiver)
            },
        })
        live.db = {
            ...live.db,
            theme: 'mobilechat',
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live-character',
                chatPage: 0,
                chats: [{ id: 'live-chat', message: messages, bookmarks: [] }],
                ttsMode: 'none',
            }],
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Live Character',
                role: 'char',
                idx: 3,
                totalLength: 10,
                isLastMemory: false,
                viewportRow: {
                    key: 'viewport-key' as any,
                    absoluteIndex: 3,
                    message,
                    sourceVersion: 1,
                },
                captureViewportTarget: () => null,
                bookmarked: false,
            },
        })

        await vi.waitFor(() => expect(target.querySelector('[data-chat-body-probe]')?.textContent).toBe('Viewport body'))
        const body = target.querySelector('[data-chat-body-probe]')
        ReloadGUIPointer.update((value) => value + 1)
        await tick()
        expect(target.querySelector('[data-chat-body-probe]')).toBe(body)
        ReloadChatPointer.update((value) => ({ ...value, 3: (value[3] ?? 0) + 1 }))
        await tick()
        expect(target.querySelector('[data-chat-body-probe]')).toBe(body)
        const refresh = (
            mounted as {
                refreshMessageDisplay(
                    state: import('src/ts/chatDisplayRefresh').ChatDisplayRefresh,
                ): void
            }
        ).refreshMessageDisplay
        const update = {
            message: 'Updated viewport body',
            totalMessages: 10,
            parserAbortSignal: new AbortController().signal,
            viewportBinding: {
                viewportRow: {
                    key: 'viewport-key' as any,
                    absoluteIndex: 3,
                    message: { ...message, data: 'Updated viewport body' },
                    sourceVersion: 2,
                },
                viewportSourceToken: 'updated-source',
                captureViewportTarget: () => null,
            },
        }
        parserCalls.length = 0
        refresh(update)
        await vi.waitFor(() => expect(body?.textContent).toBe('Updated viewport body'))
        // Each explicit refresh parses the message once, also when nothing it reads changed.
        expect(parserCalls).toHaveLength(1)
        refresh(update)
        await tick()
        expect(parserCalls).toHaveLength(2)
        expect(target.querySelector('[data-chat-body-probe]')).toBe(body)

        expect(target.querySelector('[data-chat-id="viewport-message"]')).not.toBeNull()
        expect(target.querySelector('.text-xs')?.textContent?.trim()).not.toBe('')
        expect(indexReads).not.toHaveBeenCalled()
    })

    test('does not recapture optional presentation fields missing from a viewport row', async () => {
        const message = {
            role: 'char' as const,
            data: 'Viewport body without optional fields',
        }
        const indexReads = vi.fn(() => {
            throw new Error('live message index was recaptured')
        })
        const messages = new Proxy([] as typeof message[], {
            get(target, property, receiver) {
                if (property === '3') return indexReads()
                return Reflect.get(target, property, receiver)
            },
        })
        live.db = {
            ...live.db,
            theme: 'mobilechat',
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live-character',
                chatPage: 0,
                chats: [{ id: 'live-chat', message: messages, bookmarks: [] }],
                ttsMode: 'none',
            }],
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: message.data,
                name: 'Live Character',
                role: 'char',
                idx: 3,
                totalLength: 10,
                isLastMemory: false,
                viewportRow: {
                    key: 'viewport-key' as any,
                    absoluteIndex: 3,
                    message,
                    sourceVersion: 1,
                },
                captureViewportTarget: () => null,
            },
        })

        await vi.waitFor(() => expect(
            target.querySelector('[data-chat-body-probe]')?.textContent,
        ).toBe(message.data))
        expect(target.querySelector('[data-chat-id=""]')).not.toBeNull()
        expect(indexReads).not.toHaveBeenCalled()
    })

    test('uses a bounded live parser projection without capture-only UI semantics', async () => {
        const projected = context()
        const parserContext = projected.parserContext as ProcessScriptCaptureContext['parserContext']
        parserContext.historyOffset = 4
        parserContext.character.chats[0].message = [
            { role: 'char', data: 'previous' },
            { role: 'user', data: 'nearby' },
            { role: 'char', data: '{{char}}' },
        ] as any
        parserContext.database.characters[0] = parserContext.character
        const parserProjection = {
            kind: 'bounded' as const,
            characterId: 'frozen',
            conversationId: parserContext.character.chats[0].id ?? 'live-chat',
            revision: 1,
            totalMessages: 7,
            chatID: 6,
            projectedChatID: 2,
            historyOffset: 4,
            messages: parserContext.character.chats[0].message,
            context: {
                presetRegex: projected.presetRegex,
                moduleRegexScripts: projected.moduleRegexScripts,
                moduleAssets: projected.moduleAssets,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext,
            },
        }
        live.db = {
            ...live.db,
            theme: '',
            clickToEdit: false,
            characters: [{
                type: 'character',
                name: 'Live Character',
                chaId: 'live-character',
                chatPage: 0,
                chats: [{ id: 'live-chat', message: [] }],
                ttsMode: 'none',
            }],
        }

        mounted = mount(Chat, {
            target,
            props: {
                message: '{{char}}',
                name: 'Live Character',
                role: 'char',
                idx: 6,
                totalLength: 7,
                isLastMemory: false,
                parserProjection: parserProjection as any,
            },
        })

        await vi.waitFor(() => expect(
            target.querySelector('[data-chat-body-probe]')?.textContent,
        ).toBe('Frozen Character'))
        expect(parserCalls).toContainEqual(expect.objectContaining({
            chatID: 6,
            projectedChatID: 2,
            historyOffset: 4,
        }))
        expect(target.querySelector('[data-chat-body-probe]')?.getAttribute(
            'data-parser-projection',
        )).toBe('bounded')
        expect(target.querySelector('.button-icon-edit')).not.toBeNull()
    })

    test.each(['save-unchanged', 'save-conflict', 'delete'] as const)(
        'keeps the actual retained editor addressed after an earlier insertion (%s)',
        async (action) => {
            const messages: Message[] = [
                { role: 'user', data: 'Earlier row', chatId: 'earlier-row' },
                { role: 'char', data: 'Original edited row', chatId: 'edited-row' },
                { role: 'user', data: 'Later row', chatId: 'later-row' },
            ]
            const conversation = { id: 'retained-chat', message: messages, note: '', localLore: [], bookmarks: [] } as ChatRecord
            const owner = {
                type: 'character', chaId: 'retained-owner', name: 'Synthetic retained owner', chatPage: 0,
                firstMessage: '', firstMsgIndex: -1, image: '', customscript: [], virtualscript: '',
                additionalAssets: [], emotionImages: [], triggerscript: [], chats: [conversation], ttsMode: 'none',
            } as unknown as character
            const session = new ActiveConversationSession({
                characterId: owner.chaId, conversationId: conversation.id!, conversation, storeRevision: 7,
            })
            const current = () => ({ character: owner, conversation })
            const source = new SynchronousSessionConversationViewportSource({ session, captureCurrent: current })
            const selection = {
                characterId: owner.chaId, conversationId: conversation.id!, navigationGeneration: 1, storeRevision: 7,
            } as SelectedConversationTarget
            const release = vi.fn()
            const operations = createSelectedConversationOperations({
                captureCurrent: current,
                captureSelectedConversationTarget: () => selection,
                getCurrentSession: () => session,
                getCurrentViewportSource: () => source,
                acquireCompleteConversation: async (reason) => ({ reason, target: selection, session, release }),
            })
            const acquireEdit = vi.spyOn(operations, 'acquireCompleteMessageTargetForIntent')
            const acquireAction = vi.spyOn(operations, 'acquireCompleteMessageTarget')
            vi.mocked(alertToast).mockClear()
            runtime.activeSession = session
            live.db = { ...live.db, theme: 'cardboard', characters: [owner], translator: '', clickToEdit: false,
                useChatCopy: false, enableBookmark: false, askRemoval: false, instantRemove: false,
                risunestChatEditPopup: false }
            vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
            try {
                mounted = mount(Chats, { target, props: {
                    currentCharacter: owner, viewportSource: source, selectedConversationOperations: operations,
                    onReroll: () => {}, unReroll: () => {}, currentUsername: 'User', userIcon: '',
                } })
                const row = await vi.waitFor(() => {
                    const element = [...target.querySelectorAll<HTMLElement>('[data-chat-render-key]')]
                        .find((node) => node.textContent?.includes('Original edited row'))
                    expect(element?.querySelector('.button-icon-edit')).not.toBeNull()
                    expect(element).toBeDefined()
                    return element!
                })
                row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
                const editor = await vi.waitFor(() => {
                    const element = row.querySelector<HTMLTextAreaElement>('.message-edit-area')
                    expect(element).not.toBeNull()
                    return element!
                })
                editor.value = 'Retained draft'
                editor.dispatchEvent(new Event('input', { bubbles: true }))
                editor.focus()
                session.replaceRange(session.positionAt(0), 0, [{ role: 'user', data: 'Inserted above', chatId: 'inserted-row' }])
                if (action === 'save-conflict') {
                    session.edit(session.locate(2), { ...session.readMessage(session.locate(2)), data: 'Concurrent edited row' })
                }
                await vi.waitFor(() => expect(row.dataset.chatViewportIndex).toBe('3'))
                expect(row.querySelector('.message-edit-area')).toBe(editor)
                expect(editor.value).toBe('Retained draft')
                expect(document.activeElement).toBe(editor)

                if (action === 'delete') {
                    row.querySelector<HTMLButtonElement>('.button-icon-remove')!.click()
                    await vi.waitFor(() => expect(conversation.message.map((message) => message.chatId))
                        .toEqual(['inserted-row', 'earlier-row', 'later-row']))
                    expect(acquireAction).toHaveBeenCalledWith(2, 'remove-message')
                } else {
                    row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
                    if (action === 'save-unchanged') {
                        await vi.waitFor(() => expect(conversation.message[2].data).toBe('Retained draft'))
                        expect(acquireEdit).toHaveBeenCalledWith(expect.objectContaining({ absoluteIndex: 2,
                            messageEvidence: expect.objectContaining({ data: 'Original edited row' }) }), 'edit-message')
                        expect(row.querySelector('.message-edit-area')).toBeNull()
                    } else {
                        await vi.waitFor(() => expect(alertToast).toHaveBeenCalledWith(language.chatMessageActionFailed))
                        expect(conversation.message[2].data).toBe('Concurrent edited row')
                        expect(row.querySelector('.message-edit-area')).toBe(editor)
                        expect(editor.value).toBe('Retained draft')
                    }
                    expect(conversation.message[1].data).toBe('Earlier row')
                    expect(conversation.message[3].data).toBe('Later row')
                }
            } finally {
                if (mounted) await unmount(mounted)
                mounted = undefined
                source.dispose()
            }
        },
    )

    test.each(['save-unchanged', 'save-conflict'] as const)(
        'saves the popup draft to the retained row after an earlier insertion (%s)',
        async (action) => {
            const messages: Message[] = [
                { role: 'user', data: 'Earlier row', chatId: 'earlier-row' },
                { role: 'char', data: 'Original edited row', chatId: 'edited-row' },
                { role: 'user', data: 'Later row', chatId: 'later-row' },
            ]
            const conversation = { id: 'popup-chat', message: messages, note: '', localLore: [], bookmarks: [] } as ChatRecord
            const owner = {
                type: 'character', chaId: 'popup-owner', name: 'Synthetic popup owner', chatPage: 0,
                firstMessage: '', firstMsgIndex: -1, image: '', customscript: [], virtualscript: '',
                additionalAssets: [], emotionImages: [], triggerscript: [], chats: [conversation], ttsMode: 'none',
            } as unknown as character
            const session = new ActiveConversationSession({
                characterId: owner.chaId, conversationId: conversation.id!, conversation, storeRevision: 7,
            })
            const current = () => ({ character: owner, conversation })
            const source = new SynchronousSessionConversationViewportSource({ session, captureCurrent: current })
            const selection = {
                characterId: owner.chaId, conversationId: conversation.id!, navigationGeneration: 1, storeRevision: 7,
            } as SelectedConversationTarget
            const operations = createSelectedConversationOperations({
                captureCurrent: current,
                captureSelectedConversationTarget: () => selection,
                getCurrentSession: () => session,
                getCurrentViewportSource: () => source,
                acquireCompleteConversation: async (reason) => ({ reason, target: selection, session, release: vi.fn() }),
            })
            const acquireEdit = vi.spyOn(operations, 'acquireCompleteMessageTargetForIntent')
            vi.mocked(alertToast).mockClear()
            runtime.activeSession = session
            live.db = { ...live.db, theme: '', characters: [owner], translator: '', clickToEdit: false,
                useChatCopy: false, enableBookmark: false, askRemoval: false, instantRemove: false }
            vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
            try {
                mounted = mount(Chats, { target, props: {
                    currentCharacter: owner, viewportSource: source, selectedConversationOperations: operations,
                    onReroll: () => {}, unReroll: () => {}, currentUsername: 'User', userIcon: '',
                } })
                const row = await vi.waitFor(() => {
                    const element = [...target.querySelectorAll<HTMLElement>('[data-chat-render-key]')]
                        .find((node) => node.textContent?.includes('Original edited row'))
                    expect(element).toBeDefined()
                    expect(element!.querySelector('.button-icon-edit')).not.toBeNull()
                    return element!
                })
                row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
                const request = textEditorPopup.request!
                expect(request.value).toBe('Original edited row')
                request.input?.('Popup draft')
                session.replaceRange(session.positionAt(0), 0, [{ role: 'user', data: 'Inserted above', chatId: 'inserted-row' }])
                if (action === 'save-conflict') {
                    session.edit(session.locate(2), { ...session.readMessage(session.locate(2)), data: 'Concurrent edited row' })
                }
                await vi.waitFor(() => expect(row.dataset.chatViewportIndex).toBe('3'))
                expect(row.querySelector('.message-edit-area')).toBeNull()

                if (action === 'save-unchanged') {
                    await expect(request.save('Popup draft')).resolves.toBe(true)
                    expect(conversation.message[2].data).toBe('Popup draft')
                    expect(acquireEdit).toHaveBeenCalledWith(expect.objectContaining({ absoluteIndex: 2 }), 'edit-message')
                } else {
                    await expect(request.save('Popup draft')).resolves.toBe(false)
                    expect(alertToast).toHaveBeenCalledWith(language.chatMessageActionFailed)
                    expect(conversation.message[2].data).toBe('Concurrent edited row')
                }
                expect(conversation.message[1].data).toBe('Earlier row')
                expect(conversation.message[3].data).toBe('Later row')
            } finally {
                if (mounted) await unmount(mounted)
                mounted = undefined
                source.dispose()
            }
        },
    )

    function mountRetainedEditChats(db: Record<string, unknown>, options: { reactiveProps?: boolean } = {}) {
        const messages: Message[] = [
            { role: 'user', data: 'Earlier row', chatId: 'earlier-row' },
            { role: 'char', data: 'Original edited row', chatId: 'edited-row' },
            { role: 'user', data: 'Later row', chatId: 'later-row' },
        ]
        const conversation = { id: 'retained-chat', message: messages, note: '', localLore: [], bookmarks: [] } as ChatRecord
        const owner = {
            type: 'character', chaId: 'retained-owner', name: 'Synthetic retained owner', chatPage: 0,
            firstMessage: '', firstMsgIndex: -1, image: '', customscript: [], virtualscript: '',
            additionalAssets: [], emotionImages: [], triggerscript: [], chats: [conversation], ttsMode: 'none',
        } as unknown as character
        const session = new ActiveConversationSession({
            characterId: owner.chaId, conversationId: conversation.id!, conversation, storeRevision: 7,
        })
        const current = () => ({ character: owner, conversation })
        const source = new SynchronousSessionConversationViewportSource({ session, captureCurrent: current })
        const state = {
            selection: {
                characterId: owner.chaId, conversationId: conversation.id!, navigationGeneration: 1, storeRevision: 7,
            } as SelectedConversationTarget,
        }
        const operations = createSelectedConversationOperations({
            captureCurrent: current,
            captureSelectedConversationTarget: () => state.selection,
            getCurrentSession: () => session,
            getCurrentViewportSource: () => source,
            acquireCompleteConversation: async (reason) => ({ reason, target: state.selection, session, release: vi.fn() }),
        })
        vi.mocked(alertToast).mockClear()
        runtime.activeSession = session
        live.db = { ...live.db, theme: '', characters: [owner], translator: '', clickToEdit: false,
            useChatCopy: false, enableBookmark: false, askRemoval: false, instantRemove: false, ...db }
        vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
        const mountList = () => {
            mounted = options.reactiveProps
                ? mount(ChatsHarness, { target, props: {
                    initialCharacter: owner, initialViewportSource: source, selectedConversationOperations: operations,
                } })
                : mount(Chats, { target, props: {
                    currentCharacter: owner, viewportSource: source, selectedConversationOperations: operations,
                    onReroll: () => {}, unReroll: () => {}, currentUsername: 'User', userIcon: '',
                } })
        }
        let editedIndex: string | undefined
        const editedRow = () => vi.waitFor(() => {
            const element = editedIndex === undefined
                ? [...target.querySelectorAll<HTMLElement>('[data-chat-viewport-index]')]
                    .find((node) => node.textContent?.includes('Original edited row'))
                : target.querySelector<HTMLElement>(`[data-chat-viewport-index="${editedIndex}"]`)
            expect(element?.querySelector('.button-icon-edit')).toBeTruthy()
            editedIndex = element!.dataset.chatViewportIndex
            return element!
        })
        mountList()
        return { owner, conversation, session, source, current, state, mountList, editedRow }
    }

    test.each(['saved', 'refused'] as const)(
        'brings an inline editor back after the chat list is rebuilt and saves it only against an unchanged message (%s)',
        async (outcome) => {
            const harness = mountRetainedEditChats({ risunestChatEditPopup: false })
            try {
                ;(await harness.editedRow()).querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
                const editor = await vi.waitFor(() => {
                    const element = target.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')
                    expect(element).not.toBeNull()
                    return element!
                })
                editor.value = 'Inline draft'
                editor.dispatchEvent(new Event('input', { bubbles: true }))
                await tick()
                await unmount(mounted!)
                mounted = undefined
                expect(pendingEditorDrafts(harness.owner.chaId, 'retained-chat')).toHaveLength(1)

                harness.state.selection = { ...harness.state.selection, navigationGeneration: 2 }
                harness.mountList()
                const row = await harness.editedRow()
                await vi.waitFor(() => expect(row.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')?.value).toBe('Inline draft'))
                expect(pendingEditorDrafts(harness.owner.chaId, 'retained-chat')).toEqual([])
                if (outcome === 'refused') {
                    harness.session.edit(harness.session.locate(1), { ...harness.session.readMessage(harness.session.locate(1)), data: 'Concurrent edited row' })
                    await tick()
                }
                row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()

                if (outcome === 'saved') {
                    await vi.waitFor(() => expect(harness.conversation.message[1].data).toBe('Inline draft'))
                    await vi.waitFor(() => expect(target.querySelector('textarea.message-edit-area')).toBeNull())
                    expect(alertToast).not.toHaveBeenCalled()
                } else {
                    await vi.waitFor(() => expect(alertToast).toHaveBeenCalledWith(language.chatMessageActionFailed))
                    expect(harness.conversation.message[1].data).toBe('Concurrent edited row')
                    expect(target.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')?.value).toBe('Inline draft')
                }
            } finally {
                if (mounted) await unmount(mounted)
                mounted = undefined
                harness.source.dispose()
            }
        },
    )

    test('carries a typed inline draft and its caret into the row a new navigation generation mounts', async () => {
        const harness = mountRetainedEditChats({ risunestChatEditPopup: false }, { reactiveProps: true })
        const list = mounted as unknown as {
            setViewportNavigationGeneration(generation: number): void
            setViewportSource(source: SynchronousSessionConversationViewportSource): void
        }
        const nextSource = new SynchronousSessionConversationViewportSource({ session: harness.session, captureCurrent: harness.current })
        try {
            ;(await harness.editedRow()).querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
            const editor = await vi.waitFor(() => {
                const element = target.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')
                expect(element).not.toBeNull()
                return element!
            })
            editor.value = 'Inline draft'
            editor.dispatchEvent(new Event('input', { bubbles: true }))
            editor.focus()
            editor.setSelectionRange(2, 6)
            await tick()

            // The working set publishes the same conversation under a new navigation generation.
            harness.state.selection = { ...harness.state.selection, navigationGeneration: 2 }
            list.setViewportNavigationGeneration(1)
            list.setViewportSource(nextSource)

            await vi.waitFor(() => expect(editor.isConnected).toBe(false))
            const row = await harness.editedRow()
            const restored = await vi.waitFor(() => {
                const element = row.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')
                expect(element?.value).toBe('Inline draft')
                return element!
            })
            await vi.waitFor(() => expect([restored.selectionStart, restored.selectionEnd]).toEqual([2, 6]))
            expect(document.activeElement).toBe(restored)
            expect(pendingEditorDrafts(harness.owner.chaId, 'retained-chat')).toEqual([])

            row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
            await vi.waitFor(() => expect(harness.conversation.message[1].data).toBe('Inline draft'))
            expect(alertToast).not.toHaveBeenCalled()
        } finally {
            if (mounted) await unmount(mounted)
            mounted = undefined
            nextSource.dispose()
            harness.source.dispose()
        }
    })

    test('reports the edited row to chat view listeners again when its inline editor closes', async () => {
        const harness = mountRetainedEditChats({ risunestChatEditPopup: false })
        const events: ChatViewEvent[] = []
        chatViewEvents.forOwner('editor-view-test').register((event) => { events.push(event) })
        const editedRow = { index: 1, messageId: 'edited-row', role: 'char' }
        try {
            const row = await harness.editedRow()
            await vi.waitFor(() => expect(events.some((event) => event.type === 'rows' && event.mounted.some((entry) => entry.messageId === 'edited-row'))).toBe(true))
            events.length = 0

            row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
            await vi.waitFor(() => expect(row.querySelector('textarea.message-edit-area')).not.toBeNull())
            row.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
            await vi.waitFor(() => expect(row.querySelector('textarea.message-edit-area')).toBeNull())
            await vi.waitFor(() => expect(events).toContainEqual({
                type: 'rows', characterId: 'retained-owner', conversationId: 'retained-chat',
                mounted: [], unmounted: [], rerendered: [editedRow],
            }))
            expect(harness.conversation.message[1].data).toBe('Original edited row')
        } finally {
            chatViewEvents.forOwner('editor-view-test').dispose()
            if (mounted) await unmount(mounted)
            mounted = undefined
            harness.source.dispose()
        }
    })

    test('saves an open popup after a committed refresh started a new navigation generation', async () => {
        const harness = mountRetainedEditChats({})
        try {
            ;(await harness.editedRow()).querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
            const request = textEditorPopup.request!
            request.input?.('Popup draft')
            harness.state.selection = { ...harness.state.selection, navigationGeneration: 2 }

            await expect(request.save('Popup draft')).resolves.toBe(true)
            expect(harness.conversation.message[1].data).toBe('Popup draft')
            expect(alertToast).not.toHaveBeenCalled()
        } finally {
            if (mounted) await unmount(mounted)
            mounted = undefined
            harness.source.dispose()
        }
    })

    test('refuses an actual partial save after a source handoff without reverting the external prefix or suffix', async () => {
        const original = 'Original prefix\nSelected block\nOriginal suffix'
        const external = 'External prefix\nSelected block\nExternal suffix'
        const conversation = { id: 'partial-chat', message: [
            { role: 'char', data: original, chatId: 'partial-row' },
        ], note: '', localLore: [], bookmarks: [] } as ChatRecord
        const owner = {
            type: 'character', chaId: 'partial-owner', name: 'Synthetic partial owner', chatPage: 0,
            firstMessage: '', firstMsgIndex: -1, image: '', customscript: [], virtualscript: '',
            additionalAssets: [], emotionImages: [], triggerscript: [], chats: [conversation], ttsMode: 'none',
        } as unknown as character
        const session = new ActiveConversationSession({
            characterId: owner.chaId, conversationId: conversation.id!, conversation, storeRevision: 7,
        })
        const current = () => ({ character: owner, conversation })
        let source = new SynchronousSessionConversationViewportSource({ session, captureCurrent: current })
        const initialSource = source
        const selection = {
            characterId: owner.chaId, conversationId: conversation.id!, navigationGeneration: 1, storeRevision: 7,
        } as SelectedConversationTarget
        const operations = createSelectedConversationOperations({
            captureCurrent: current,
            captureSelectedConversationTarget: () => selection,
            getCurrentSession: () => session,
            getCurrentViewportSource: () => source,
            acquireCompleteConversation: async (reason) => ({ reason, target: selection, session, release: () => {} }),
        })
        const acquireEdit = vi.spyOn(operations, 'acquireCompleteMessageTargetForIntent')
        const rebindEdit = vi.spyOn(operations, 'rebindMessageEditIntent')
        vi.mocked(alertToast).mockClear()
        runtime.activeSession = session
        live.db = { ...live.db, theme: 'cardboard', characters: [owner], translator: '', clickToEdit: false,
            useChatCopy: false, enableBookmark: false, enableBlockPartialEdit: false,
            enableDragPartialEdit: true, swipe: false }
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        try {
            await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
            const initial = source.snapshot()
            mounted = mount(Chat, { target, props: {
                message: original, name: owner.name, role: 'char', idx: 0, totalLength: 1, isLastMemory: false,
                viewportRow: initial.rowAt(0)!, viewportSourceToken: initial.sourceToken,
                selectedConversationOperations: operations, captureViewportTarget: () => null,
            } })
            const body = await vi.waitFor(() => {
                const element = target.querySelector<HTMLElement>('[data-chat-body-probe]')
                expect(element?.textContent).toBe(original)
                return element!
            })
            const bodyRoot = target.querySelector<HTMLElement>('.chattext')!
            TestIntersectionObserver.instance?.setVisible(bodyRoot)
            await tick()
            const range = document.createRange()
            const start = original.indexOf('Selected block')
            range.setStart(body.firstChild!, start)
            range.setEnd(body.firstChild!, start + 'Selected block'.length)
            const selected = window.getSelection()!
            vi.spyOn(range, 'getBoundingClientRect').mockReturnValue({
                x: 10, y: 10, top: 10, left: 10, bottom: 30, right: 130, width: 120, height: 20,
                toJSON: () => ({}),
            })
            selected.removeAllRanges()
            selected.addRange(range)
            document.dispatchEvent(new Event('selectionchange'))
            const partialButton = await vi.waitFor(() => {
                const button = document.querySelector<HTMLButtonElement>('.partial-edit-drag-btn-wrapper .partial-edit-btn-edit')
                expect(button?.closest<HTMLElement>('.partial-edit-drag-btn-wrapper')?.style.display).not.toBe('none')
                expect(button).not.toBeNull()
                return button!
            })
            partialButton.click()
            const editor = await vi.waitFor(() => {
                const input = document.querySelector<HTMLTextAreaElement>('.partial-edit-modal textarea')
                expect(input?.value).toBe('Selected block')
                return input!
            })
            editor.value = 'Recoverable partial draft'
            editor.dispatchEvent(new Event('input', { bubbles: true }))
            await tick()
            editor.blur()
            session.edit(session.locate(0), { ...session.readMessage(session.locate(0)), data: external })
            source = new SynchronousSessionConversationViewportSource({ session, captureCurrent: current })
            await source.ensureRange({ startIndex: 0, limit: 1, reason: 'viewport' })
            const refreshed = source.snapshot()
            expect(refreshed.sourceToken).not.toBe(initial.sourceToken)
            ;(mounted as { updateViewportBinding(state: unknown): void }).updateViewportBinding({
                viewportRow: refreshed.rowAt(0)!, viewportSourceToken: refreshed.sourceToken,
                captureViewportTarget: () => null, totalMessages: refreshed.totalMessages,
            })
            await tick()
            expect(rebindEdit).toHaveBeenCalledWith(expect.objectContaining({
                messageEvidence: expect.objectContaining({ data: original }),
            }), expect.objectContaining({ message: expect.objectContaining({ data: external }) }))
            expect(document.querySelector('.partial-edit-modal textarea')).toBe(editor)
            document.querySelector<HTMLButtonElement>('.partial-edit-save-btn')!.click()
            await vi.waitFor(() => expect(alertToast).toHaveBeenCalledWith(language.chatMessageActionFailed))
            expect(acquireEdit).toHaveBeenCalledWith(expect.objectContaining({
                messageEvidence: expect.objectContaining({ data: original }),
            }), 'partial-edit-message')
            expect(conversation.message[0].data).toBe(external)
            expect(conversation.message[0].data.startsWith('External prefix\n')).toBe(true)
            expect(conversation.message[0].data.endsWith('\nExternal suffix')).toBe(true)
            expect(document.querySelector('.partial-edit-modal textarea')).toBe(editor)
            expect(editor.value).toBe('Recoverable partial draft')
            // Partial edits are not carried across a row teardown.
            expect((mounted as { captureEditorDraft(): unknown }).captureEditorDraft()).toBeNull()
        } finally {
            window.getSelection()?.removeAllRanges()
            if (mounted) await unmount(mounted)
            mounted = undefined
            source.dispose()
            initialSource.dispose()
        }
    })

    test('delays windowed edit promotion until save and commits against the original row target', async () => {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            clickToEdit: false,
            risunestChatEditPopup: false,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })

        const editButton = await vi.waitFor(() => {
            const button = target.querySelector<HTMLButtonElement>('.button-icon-edit')
            expect(button).not.toBeNull()
            return button!
        })
        expect((mounted as { hasActiveEditor(): boolean }).hasActiveEditor()).toBe(false)
        editButton.click()
        expect((mounted as { hasActiveEditor(): boolean }).hasActiveEditor()).toBe(true)
        expect(harness.captureMessageEditIntent).toHaveBeenCalledOnce()
        expect(harness.acquireCompleteMessageTargetForIntent).not.toHaveBeenCalled()

        const editor = await vi.waitFor(() => {
            const textarea = target.querySelector<HTMLTextAreaElement>('.message-edit-area')
            expect(textarea).not.toBeNull()
            return textarea!
        })
        editor.value = 'Saved after promotion'
        editor.dispatchEvent(new Event('input', { bubbles: true }))
        editButton.focus()
        expect((mounted as { hasActiveEditor(): boolean }).hasActiveEditor()).toBe(true)
        editButton.click()

        await vi.waitFor(() => {
            expect(harness.completeConversation.message[1].data).toBe('Saved after promotion')
            expect(harness.acquireCompleteMessageTargetForIntent).toHaveBeenCalledWith(
                expect.objectContaining({ rowKey: 'row-1' }),
                'edit-message',
            )
            expect(harness.release).toHaveBeenCalledOnce()
        })
        expect((mounted as { hasActiveEditor(): boolean }).hasActiveEditor()).toBe(false)
    })

    test.each(['saved', 'refused'] as const)(
        'keeps the draft when a long press closes the original editor and the save is %s',
        async (outcome) => {
            const harness = makeWindowedEditHarness()
            if (outcome === 'refused') {
                harness.acquireCompleteMessageTargetForIntent.mockResolvedValueOnce(null as never)
            }
            live.db = {
                ...live.db,
                theme: '',
                characters: [harness.metadataCharacter],
                translator: '',
                useChatCopy: false,
                enableBookmark: false,
                clickToEdit: false,
                risunestChatEditPopup: false,
            }
            mounted = mount(Chat, {
                target,
                props: {
                    message: harness.message.data,
                    name: 'Live Character',
                    role: 'char',
                    idx: 1,
                    totalLength: 2,
                    isLastMemory: false,
                    viewportRow: {
                        key: 'row-1' as ConversationViewportKey,
                        absoluteIndex: 1,
                        message: harness.message,
                        sourceVersion: 3,
                    },
                    viewportSourceToken: 'source-a',
                    selectedConversationOperations: harness.operations,
                    captureViewportTarget: () => null,
                },
            })

            const editButton = await vi.waitFor(() => {
                const button = target.querySelector<HTMLButtonElement>('.button-icon-edit')
                expect(button).not.toBeNull()
                return button!
            })
            editButton.click()
            const editor = await vi.waitFor(() => {
                const textarea = target.querySelector<HTMLTextAreaElement>('.message-edit-area')
                expect(textarea).not.toBeNull()
                return textarea!
            })
            editor.value = 'Kept after long press'
            editor.dispatchEvent(new Event('input', { bubbles: true }))

            vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
            editor.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
            await vi.advanceTimersByTimeAsync(500)
            vi.useRealTimers()

            await vi.waitFor(() => expect(
                harness.acquireCompleteMessageTargetForIntent,
            ).toHaveBeenCalledWith(expect.objectContaining({ rowKey: 'row-1' }), 'edit-message'))
            const hasActiveEditor = () =>
                (mounted as { hasActiveEditor(): boolean }).hasActiveEditor()
            if (outcome === 'saved') {
                await vi.waitFor(() => {
                    expect(harness.completeConversation.message[1].data).toBe('Kept after long press')
                    expect(hasActiveEditor()).toBe(false)
                })
                expect(target.querySelector('.message-edit-area')).toBeNull()
            } else {
                await tick()
                expect(harness.completeConversation.message[1].data).toBe('Original viewport message')
                expect(hasActiveEditor()).toBe(true)
                expect(target.querySelector<HTMLTextAreaElement>('.message-edit-area')?.value)
                    .toBe('Kept after long press')
            }
        },
    )

    function mountPopupEditHarness(
        theme: string,
        options: { harness?: ReturnType<typeof makeWindowedEditHarness>, props?: Record<string, unknown>, db?: Record<string, unknown> } = {},
    ) {
        const harness = options.harness ?? makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme,
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            clickToEdit: false,
            ...options.db,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
                ...options.props,
            },
        })
        return harness
    }

    interface RowEditor {
        hasActiveEditor(): boolean
        captureEditorDraft(): Omit<ChatEditorDraft, 'caret'> | null
        refreshMessageDisplay(state: { message: string, totalMessages: number }): void
    }

    const hasPopupRowEditor = () => (mounted as { hasActiveEditor(): boolean }).hasActiveEditor()

    async function openPopupEditor() {
        const editButton = await vi.waitFor(() => {
            const button = target.querySelector<HTMLButtonElement>('.button-icon-edit')
            expect(button).not.toBeNull()
            return button!
        })
        editButton.click()
        const request = textEditorPopup.request
        expect(request).not.toBeNull()
        return { editButton, request: request! }
    }

    test('refreshes a retained row index and replacement character used by its parser', async () => {
        const parser = await import('src/ts/parser/parser.svelte')
        const markdown = vi.spyOn(parser, 'ParseMarkdown').mockImplementation(async value => value)
        try {
            const harness = makeWindowedEditHarness()
            harness.completeConversation.message.unshift({ role: 'user', data: 'Inserted prefix', chatId: 'prefix' })
            const replacement = { ...harness.completeCharacter, name: 'Replacement parser character' }
            mountPopupEditHarness('', {
                harness,
                props: { character: harness.completeCharacter, viewportRow: undefined },
                db: { characters: [replacement], translator: 'llm', translatorType: 'llm', translateBeforeHTMLFormatting: false },
            })
            const body = await vi.waitFor(() => {
                const element = target.querySelector('[data-chat-body-probe]')
                expect(element).not.toBeNull()
                return element!
            })
            const row = target.querySelector('.risu-chat')
            parserCalls.length = 0
            ;(mounted as { refreshMessageDisplay(state: import('src/ts/chatDisplayRefresh').ChatDisplayRefresh): void })
                .refreshMessageDisplay({ message: 'Refreshed parser input', totalMessages: 3, index: 2, character: replacement.chaId })
            await tick()
            expect(target.querySelector('.risu-chat')).toBe(row)
            expect(target.querySelector('[data-chat-body-probe]')).toBe(body)
            expect(row?.getAttribute('data-chat-index')).toBe('2')
            expect(body.textContent).toBe('Refreshed parser input')
            expect(parserCalls.at(-1)?.chatID).toBe(2)
            target.querySelector<HTMLButtonElement>('.button-icon-translate')!.click()
            ;(await vi.waitFor(() => {
                const button = [...target.querySelectorAll<HTMLButtonElement>('button')]
                    .find(element => element.textContent?.includes(language.editTranslation))
                expect(button).toBeDefined()
                return button!
            })).click()
            await vi.waitFor(() => expect(markdown).toHaveBeenCalled())
            expect(markdown.mock.calls.at(-1)?.slice(0, 4)).toEqual([
                'Refreshed parser input', replacement.chaId, 'pretranslate', 2,
            ])
            expect(target.querySelector('.risu-chat')).toBe(row)
        } finally {
            markdown.mockRestore()
        }
    })

    test.each(['presentation', 'size', 'theme', 'custom-stable', 'custom-structure'] as const)(
        'keeps the actual inline editor, draft and caret through a same-row %s update',
        async (change) => {
            live.reactive = true
            const custom = change.startsWith('custom')
            const guiHTML = '<div class="before"><span>Before</span><RISUBUTTONS></RISUBUTTONS><RISUTEXTBOX></RISUTEXTBOX></div>'
            const onEditorClose = vi.fn()
            const harness = mountPopupEditHarness(custom ? 'customHTML' : 'cardboard', {
                db: { risunestChatEditPopup: false, guiHTML },
                props: { onEditorClose },
            })
            ;(await vi.waitFor(() => {
                const button = target.querySelector<HTMLButtonElement>('.button-icon-edit')
                expect(button).not.toBeNull()
                return button!
            })).click()
            const editor = await vi.waitFor(() => {
                const input = target.querySelector<HTMLTextAreaElement>('.message-edit-area')
                expect(input).not.toBeNull()
                return input!
            })
            const row = target.querySelector('.risu-chat')
            editor.value = 'Uncommitted editor draft'
            editor.dispatchEvent(new Event('input', { bubbles: true }))
            editor.focus()
            editor.setSelectionRange(4, 13, 'backward')
            await tick()
            const guiAfter = change === 'custom-structure'
                ? '<p class="after"><RISUBUTTONS></RISUBUTTONS><span><RISUTEXTBOX></RISUTEXTBOX></span></p>'
                : '<div class="after"><span>After</span><RISUBUTTONS></RISUBUTTONS><RISUTEXTBOX></RISUTEXTBOX></div>'
            live.db = {
                ...live.db,
                ...(change === 'size' ? { iconsize: 80, zoomsize: 140, lineHeight: 1.75 } : {}),
                ...(change === 'theme' ? { theme: '' } : {}),
                ...(custom ? { guiHTML: guiAfter } : {}),
            }
            if (change === 'presentation') {
                (mounted as unknown as { updatePresentation(state: import('src/ts/chatDisplayRefresh').ChatPresentationRefresh): void })
                    .updatePresentation({ img: '', name: 'Updated sender', largePortrait: true, bookmarked: true, role: 'char', messageGenerationInfo: undefined })
            }
            await tick()
            expect(target.querySelector('.risu-chat')).toBe(row)
            expect(target.querySelector('.message-edit-area')).toBe(editor)
            expect(editor.value).toBe('Uncommitted editor draft')
            expect(document.activeElement).toBe(editor)
            expect([editor.selectionStart, editor.selectionEnd, editor.selectionDirection]).toEqual([4, 13, 'backward'])
            expect(onEditorClose).not.toHaveBeenCalled()
            target.querySelector<HTMLButtonElement>('.button-icon-edit')!.click()
            await vi.waitFor(() => expect(harness.completeConversation.message[1].data).toBe('Uncommitted editor draft'))
            await vi.waitFor(() => expect(onEditorClose).toHaveBeenCalledOnce())
            expect(target.querySelector('.message-edit-area')).toBeNull()
            if (custom) expect(target.querySelector('.after')).not.toBeNull()
            if (change === 'presentation') expect(target.textContent).toContain('Updated sender')
            if (change === 'theme') expect(target.querySelector('.sm\\:h-96')).toBeNull()
        },
    )

    test.each(['save', 'cancel'] as const)('notifies the row owner once when the original popup closes by %s', async (action) => {
        const onEditorClose = vi.fn()
        mountPopupEditHarness('', { props: { onEditorClose } })
        const { request } = await openPopupEditor()
        await tick()
        expect(onEditorClose).not.toHaveBeenCalled()
        if (action === 'save') await expect(request.save('Saved popup draft')).resolves.toBe(true)
        else cancelTextEditorPopup(request)
        await tick()
        expect(onEditorClose).toHaveBeenCalledOnce()
        ReloadGUIPointer.update(value => value + 1)
        await tick()
        expect(onEditorClose).toHaveBeenCalledOnce()
    })

    test.each(['', 'cardboard'])(
        'edits the message in the popup editor by default and saves against the original row target (theme "%s")',
        async (theme) => {
            const harness = mountPopupEditHarness(theme)
            const { editButton, request } = await openPopupEditor()

            expect(request.value).toBe('Original viewport message')
            expect(hasPopupRowEditor()).toBe(true)
            await tick()
            expect(target.querySelector('.message-edit-area')).toBeNull()
            // A torn-down row keeps the popup's draft, not the text it opened with.
            request.input?.('Typed in popup')
            expect((mounted as { captureEditorDraft(): unknown }).captureEditorDraft()).toMatchObject({
                kind: 'original', draft: 'Typed in popup', popup: { request },
            })

            // The edit hotkey clicks the row button behind the popup; it must not save the stale draft.
            editButton.click()
            await tick()
            expect(harness.acquireCompleteMessageTargetForIntent).not.toHaveBeenCalled()
            expect(textEditorPopup.request).toBe(request)

            await expect(request.save('Saved in popup')).resolves.toBe(true)
            expect(harness.completeConversation.message[1].data).toBe('Saved in popup')
            expect(harness.acquireCompleteMessageTargetForIntent).toHaveBeenCalledWith(
                expect.objectContaining({ rowKey: 'row-1' }),
                'edit-message',
            )
            expect(harness.release).toHaveBeenCalledOnce()
            expect(hasPopupRowEditor()).toBe(false)
        },
    )

    test('keeps the popup editor open when its save is refused', async () => {
        const harness = mountPopupEditHarness('')
        harness.acquireCompleteMessageTargetForIntent.mockResolvedValueOnce(null as never)
        vi.mocked(alertToast).mockClear()
        const { request } = await openPopupEditor()

        await expect(request.save('Refused edit')).resolves.toBe(false)
        expect(alertToast).toHaveBeenCalledWith(language.chatMessageActionFailed)
        expect(harness.completeConversation.message[1].data).toBe('Original viewport message')
        expect(hasPopupRowEditor()).toBe(true)
        await tick()
        expect(target.querySelector('.message-edit-area')).toBeNull()

        await expect(request.save('Accepted edit')).resolves.toBe(true)
        expect(harness.completeConversation.message[1].data).toBe('Accepted edit')
        expect(hasPopupRowEditor()).toBe(false)
    })

    test('leaves the message unchanged when the popup editor is cancelled', async () => {
        const harness = mountPopupEditHarness('')
        const { editButton, request } = await openPopupEditor()

        cancelTextEditorPopup(request)
        expect(textEditorPopup.request).toBeNull()
        expect(hasPopupRowEditor()).toBe(false)
        expect(harness.acquireCompleteMessageTargetForIntent).not.toHaveBeenCalled()
        expect(harness.completeConversation.message[1].data).toBe('Original viewport message')

        editButton.click()
        expect(textEditorPopup.request).not.toBeNull()
        expect(textEditorPopup.request).not.toBe(request)
        expect(hasPopupRowEditor()).toBe(true)
    })

    test('hands an open popup to the remounted row, which saves it with a fresh intent', async () => {
        const harness = mountPopupEditHarness('')
        vi.mocked(alertToast).mockClear()
        const { request } = await openPopupEditor()
        request.input?.('Typed before teardown')
        const captured = (mounted as unknown as RowEditor).captureEditorDraft()!
        await unmount(mounted!)
        mounted = undefined
        expect(textEditorPopup.request).toBe(request)

        mountPopupEditHarness('', { harness, props: { restoredEditor: captured } })
        expect(hasPopupRowEditor()).toBe(true)
        expect(harness.captureMessageEditIntent).toHaveBeenCalledTimes(2)
        await expect(request.save('Saved by the new row')).resolves.toBe(true)
        expect(harness.completeConversation.message[1].data).toBe('Saved by the new row')
        expect(alertToast).not.toHaveBeenCalled()
        expect(hasPopupRowEditor()).toBe(false)
    })

    test('saves a popup whose row is gone and forgets the kept draft', async () => {
        const harness = mountPopupEditHarness('')
        const { request } = await openPopupEditor()
        request.input?.('Typed before teardown')
        keepEditorDraft('character-a', 'conversation-a', (mounted as unknown as RowEditor).captureEditorDraft()!)
        await unmount(mounted!)
        mounted = undefined

        await expect(request.save('Saved without a row')).resolves.toBe(true)
        expect(harness.completeConversation.message[1].data).toBe('Saved without a row')
        expect(pendingEditorDrafts('character-a', 'conversation-a')).toEqual([])
    })

    test('closes a kept popup when the remounted row cannot edit the message', async () => {
        const harness = mountPopupEditHarness('')
        const { request } = await openPopupEditor()
        const captured = (mounted as unknown as RowEditor).captureEditorDraft()!
        await unmount(mounted!)
        mounted = undefined
        harness.captureMessageEditIntent.mockReturnValueOnce(null as never)

        mountPopupEditHarness('', { harness, props: { restoredEditor: captured } })
        expect(hasPopupRowEditor()).toBe(false)
        expect(textEditorPopup.request).toBeNull()
        expect(request.value).toBe('Original viewport message')
    })

    test.each(['unchanged', 'changed'] as const)(
        'saves a restored translation draft under its kept key only while the source is %s',
        async (source) => {
            const { getLLMCache, setLLMCache } = await import('../../ts/translator/translator')
            vi.mocked(getLLMCache).mockResolvedValue('Cached translation')
            vi.mocked(setLLMCache).mockClear()
            vi.mocked(alertToast).mockClear()
            const db = { translator: 'llm', translatorType: 'llm', translateBeforeHTMLFormatting: true }
            const harness = mountPopupEditHarness('', { db })
            const translationEditor = () => target.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')
            const translationButton = (label: string) => [...target.querySelectorAll<HTMLButtonElement>('button')]
                .find((button) => button.textContent?.includes(label))
            ;(await vi.waitFor(() => {
                const button = target.querySelector<HTMLButtonElement>('.button-icon-translate')
                expect(button).not.toBeNull()
                return button!
            })).click()
            ;(await vi.waitFor(() => {
                expect(translationButton(language.editTranslation)).toBeDefined()
                return translationButton(language.editTranslation)!
            })).click()
            const opened = await vi.waitFor(() => {
                expect(translationEditor()?.value).toBe('Cached translation')
                return translationEditor()!
            })
            opened.value = 'Restored translation'
            opened.dispatchEvent(new Event('input', { bubbles: true }))
            const captured = (mounted as unknown as RowEditor).captureEditorDraft()!
            expect(captured).toMatchObject({ kind: 'translation', draft: 'Restored translation' })
            const key = vi.mocked(getLLMCache).mock.calls.at(-1)![0]
            expect(captured.translationKey).toBe(key)
            await unmount(mounted!)
            mounted = undefined

            const onEditorClose = vi.fn()
            mountPopupEditHarness('', { harness, db, props: { restoredEditor: captured, onEditorClose } })
            await tick()
            expect(translationEditor()?.value).toBe('Restored translation')
            if (source === 'changed') {
                (mounted as unknown as RowEditor).refreshMessageDisplay({ message: 'Changed source message', totalMessages: 2 })
                await tick()
            }
            translationButton(language.editTranslationSave)!.click()

            if (source === 'unchanged') {
                await vi.waitFor(() => expect(setLLMCache).toHaveBeenCalledWith(key, 'Restored translation'))
                await vi.waitFor(() => expect(translationEditor()).toBeNull())
                expect(onEditorClose).toHaveBeenCalledOnce()
                expect(alertToast).not.toHaveBeenCalled()
            } else {
                await vi.waitFor(() => expect(alertToast).toHaveBeenCalledWith(language.chatMessageActionFailed))
                expect(setLLMCache).not.toHaveBeenCalled()
                expect(translationEditor()?.value).toBe('Restored translation')
                expect(onEditorClose).not.toHaveBeenCalled()
            }
        },
    )

    async function openWindowedEditor(harness: ReturnType<typeof makeWindowedEditHarness>) {
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            clickToEdit: false,
            risunestChatEditPopup: false,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })
        const editButton = await vi.waitFor(() => {
            const button = target.querySelector<HTMLButtonElement>('.button-icon-edit')
            expect(button).not.toBeNull()
            return button!
        })
        editButton.click()
        const editor = await vi.waitFor(() => {
            const textarea = target.querySelector<HTMLTextAreaElement>('.message-edit-area')
            expect(textarea).not.toBeNull()
            return textarea!
        })
        editor.value = 'Draft across rebinding'
        editor.dispatchEvent(new Event('input', { bubbles: true }))
        return {
            editButton,
            chat: mounted as {
                hasActiveEditor(): boolean
                updateViewportBinding(state: {
                    viewportRow: {
                        key: ConversationViewportKey
                        absoluteIndex: number
                        message: Message
                        sourceVersion: number
                    }
                    viewportSourceToken: string
                    captureViewportTarget: () => null
                    totalMessages: number
                }): void
            },
        }
    }

    test('moves an open edit with its row and saves against the new position', async () => {
        const harness = makeWindowedEditHarness()
        const { editButton, chat } = await openWindowedEditor(harness)

        chat.updateViewportBinding({
            viewportRow: {
                key: 'row-1' as ConversationViewportKey,
                absoluteIndex: 0,
                message: { ...harness.message },
                sourceVersion: 4,
            },
            viewportSourceToken: 'source-a',
            captureViewportTarget: () => null,
            totalMessages: 1,
        })

        expect(harness.captureMessageEditIntent).toHaveBeenCalledOnce()
        expect(harness.rebindMessageEditIntent).toHaveBeenCalledWith(harness.intent, {
            absoluteIndex: 0,
            sourceToken: 'source-a',
            sourceVersion: 4,
            rowKey: 'row-1',
            message: harness.message,
        })
        expect(target.querySelector<HTMLTextAreaElement>('.message-edit-area')?.value)
            .toBe('Draft across rebinding')

        editButton.click()
        await vi.waitFor(() => {
            expect(harness.acquireCompleteMessageTargetForIntent).toHaveBeenCalledWith(
                expect.objectContaining({ absoluteIndex: 0, messageEvidence: harness.message }),
                'edit-message',
            )
            expect(harness.completeConversation.message[1].data).toBe('Draft across rebinding')
        })
        expect(chat.hasActiveEditor()).toBe(false)
    })

    test('keeps the original edit evidence when its row changes in place', async () => {
        const harness = makeWindowedEditHarness()
        harness.acquireCompleteMessageTargetForIntent.mockResolvedValueOnce(null as never)
        const { editButton, chat } = await openWindowedEditor(harness)

        chat.updateViewportBinding({
            viewportRow: {
                key: 'row-1' as ConversationViewportKey,
                absoluteIndex: 1,
                message: { ...harness.message, data: 'Changed elsewhere' },
                sourceVersion: 4,
            },
            viewportSourceToken: 'source-a',
            captureViewportTarget: () => null,
            totalMessages: 2,
        })

        expect(harness.captureMessageEditIntent).toHaveBeenCalledOnce()
        expect(harness.rebindMessageEditIntent).toHaveReturnedWith(harness.intent)

        editButton.click()
        await vi.waitFor(() => expect(
            harness.acquireCompleteMessageTargetForIntent,
        ).toHaveBeenCalledWith(harness.intent, 'edit-message'))
        await tick()
        expect(harness.completeConversation.message[1].data).toBe('Original viewport message')
        expect(chat.hasActiveEditor()).toBe(true)
        expect(target.querySelector<HTMLTextAreaElement>('.message-edit-area')?.value)
            .toBe('Draft across rebinding')
    })

    test.each(['cancel', 'unmount'] as const)('cleans up a pending partial edit scroll on %s', async (action) => {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            enableBlockPartialEdit: true,
            enableDragPartialEdit: false,
            swipe: false,
            clickToEdit: false,
        }
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
            callback(0)
            return 1
        })
        vi.stubGlobal('cancelAnimationFrame', vi.fn())
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })
        await tick()

        const bodyRoot = target.querySelector<HTMLElement>('.chattext')!
        const block = target.querySelector<HTMLElement>('[data-chat-body-probe]')!
        vi.spyOn(document, 'elementFromPoint').mockReturnValue(block)
        TestIntersectionObserver.instance?.setVisible(bodyRoot)
        await tick()
        document.dispatchEvent(new MouseEvent('mousemove', { clientX: 10, clientY: 10 }))
        await tick()
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        document.querySelector<HTMLButtonElement>('.partial-edit-btn-edit')!.click()
        await tick()
        await vi.advanceTimersByTimeAsync(10)

        expect((mounted as { hasActiveEditor(): boolean }).hasActiveEditor()).toBe(true)
        const cancel = document.querySelector<HTMLButtonElement>('.partial-edit-cancel-btn')
        expect(cancel).not.toBeNull()
        if (action === 'cancel') {
            cancel!.click()
            await tick()
            expect((mounted as { hasActiveEditor(): boolean }).hasActiveEditor()).toBe(false)
        } else {
            await unmount(mounted!)
            mounted = undefined
        }
        await vi.advanceTimersByTimeAsync(200)
        expect(document.querySelector('.partial-edit-modal')).toBeNull()
    })

    test('promotes and releases a windowed message operation before removing its row', async () => {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
            askRemoval: false,
            instantRemove: false,
        }
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })

        const removeButton = await vi.waitFor(() => {
            const button = target.querySelector<HTMLButtonElement>('.button-icon-remove')
            expect(button).not.toBeNull()
            return button!
        })
        removeButton.dispatchEvent(new MouseEvent('click', { bubbles: true }))

        await vi.waitFor(() => {
            expect(harness.acquireCompleteMessageTarget).toHaveBeenCalledWith(1, 'remove-message')
            expect(harness.session.totalMessages).toBe(1)
            expect(harness.release).toHaveBeenCalledOnce()
        })
    })

    async function mountWindowedTriggerRow() {
        const harness = makeWindowedEditHarness()
        live.db = {
            ...live.db,
            theme: 'cardboard',
            characters: [harness.metadataCharacter],
            translator: '',
            useChatCopy: false,
            enableBookmark: false,
        }
        const requireCurrent = vi.fn(() => ({
            character: harness.completeCharacter,
            conversation: harness.completeConversation,
            session: harness.session,
            selection: {
                characterId: harness.completeCharacter.chaId,
                conversationId: harness.completeConversation.id!,
                navigationGeneration: 1,
                storeRevision: 7,
            },
        }))
        harness.withCompleteSelectedConversation.mockImplementation(
            async (_reason, operation) => operation({ requireCurrent }),
        )
        mounted = mount(Chat, {
            target,
            props: {
                message: harness.message.data,
                name: 'Live Character',
                role: 'char',
                idx: 1,
                totalLength: 2,
                isLastMemory: false,
                viewportRow: {
                    key: 'row-1' as ConversationViewportKey,
                    absoluteIndex: 1,
                    message: harness.message,
                    sourceVersion: 3,
                },
                viewportSourceToken: 'source-a',
                selectedConversationOperations: harness.operations,
                captureViewportTarget: () => null,
            },
        })

        const body = await vi.waitFor(() => {
            const element = target.querySelector<HTMLElement>('[data-chat-body-probe]')
            expect(element).not.toBeNull()
            return element!
        })
        return { harness, requireCurrent, body }
    }

    test('runs a manual windowed trigger only inside the complete conversation gateway and a background hold', async () => {
        const heldDuringTrigger: boolean[] = []
        actionMocks.runTrigger.mockImplementation(async () => { heldDuringTrigger.push(actionMocks.hold.held); return null })
        probedDisplayRows.length = 0
        const { harness, requireCurrent, body } = await mountWindowedTriggerRow()
        const trigger = document.createElement('button')
        trigger.setAttribute('risu-trigger', 'manual-test')
        body.append(trigger)
        trigger.click()

        await vi.waitFor(() => {
            expect(harness.withCompleteSelectedConversation).toHaveBeenCalledWith(
                'manual-chat-trigger',
                expect.any(Function),
            )
            expect(actionMocks.runTrigger).toHaveBeenCalledWith(
                harness.completeCharacter,
                'manual',
                expect.objectContaining({ chat: harness.completeConversation }),
            )
            expect(requireCurrent).toHaveBeenCalledTimes(2)
        })
        // Script work a message starts keeps running after Home.
        expect(heldDuringTrigger).toEqual([true])
        expect(actionMocks.hold.held).toBe(false)
        const row = probedDisplayRows.at(-1)!
        const revision = row.rebuildRevision
        const pointer = get(ReloadChatPointer)[1] ?? 0

        // A trigger that returns a result rebuilds the clicked message.
        actionMocks.runTrigger.mockResolvedValue({ chat: harness.completeConversation })
        trigger.click()
        await vi.waitFor(() => expect(requireCurrent).toHaveBeenCalledTimes(4))
        expect(row.rebuildRevision).not.toBe(revision)
        expect(get(ReloadChatPointer)[1]).toBe(pointer + 1)
    })

    test('submits form values captured at submit time through the complete conversation gateway', async () => {
        let heldDuringForm: boolean | undefined
        actionMocks.runLuaInteractionTrigger.mockImplementation(async () => { heldDuringForm = actionMocks.hold.held; return null })
        const { harness, requireCurrent, body } = await mountWindowedTriggerRow()
        // The gateway runs the trigger later, after the row may have changed the form.
        harness.withCompleteSelectedConversation.mockImplementation(async (_reason, operation) => {
            await Promise.resolve()
            return operation({ requireCurrent })
        })
        body.innerHTML = `<form risu-form="survey">
            <input name="choice" value="a"><input name="choice" value="b"><input name="__proto__" value="kept">
            <button name="go" value="yes">Go</button><button name="go" value="no">Stop</button>
        </form>`
        const form = body.querySelector('form')!
        const submitter = form.querySelector<HTMLButtonElement>('button[value="yes"]')!

        const submit = new SubmitEvent('submit', { bubbles: true, cancelable: true, submitter })
        form.dispatchEvent(submit)
        form.querySelector('input')!.value = 'changed'
        form.remove()

        expect(submit.defaultPrevented).toBe(true)
        await vi.waitFor(() => expect(actionMocks.runLuaInteractionTrigger).toHaveBeenCalledOnce())
        const [type, character, formName, meta] = actionMocks.runLuaInteractionTrigger.mock.calls[0]
        expect([type, character, formName]).toEqual(['form', harness.completeCharacter, 'survey'])
        expect(Object.entries(meta.data)).toEqual([['choice', ['a', 'b']], ['__proto__', 'kept'], ['go', 'yes']])
        expect(harness.withCompleteSelectedConversation).toHaveBeenCalledWith('manual-chat-trigger', expect.any(Function))
        expect(heldDuringForm).toBe(true)
    })

    test('prevents the submit of a message form without risu-form and runs no trigger', async () => {
        const { harness, body } = await mountWindowedTriggerRow()
        body.innerHTML = '<form action="/elsewhere"><input name="field" value="value"></form>'

        const submit = new SubmitEvent('submit', { bubbles: true, cancelable: true })
        body.querySelector('form')!.dispatchEvent(submit)
        await Promise.resolve()

        expect(submit.defaultPrevented).toBe(true)
        expect(actionMocks.runLuaInteractionTrigger).not.toHaveBeenCalled()
        expect(harness.withCompleteSelectedConversation).not.toHaveBeenCalled()
    })
})
