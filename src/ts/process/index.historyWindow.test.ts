import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    diagnosticEvents: [] as Record<string, unknown>[],
    acknowledge: vi.fn(async () => undefined),
    session: null as any,
    currentCharacter: null as any,
    modelRequests: [] as any[],
    modelResponse: null as any,
    deviceSettings: {
        generationHistoryLimitEnabled: true,
        generationHistoryLimitMultiplier: 2,
    } as Record<string, unknown>,
    selectedTarget: null as any,
    selectedAuthority: null as any,
    windowedController: null as any,
    persistentStore: null as any,
    flushPendingData: vi.fn(async () => undefined),
    acquireCompleteConversation: vi.fn(),
    startTrigger: null as null | ((chat: any) => any),
    outputTrigger: null as null | ((chat: any) => any),
    hypaMemoryV3: vi.fn(),
    hypaSettings: {
        preserveOrphanedMemory: false,
        useExperimentalImpl: false,
        recentMemoryRatio: 0,
        similarMemoryRatio: 0,
        queryChatCount: 2,
    } as Record<string, unknown>,
    processScriptFull: vi.fn(),
    alertError: vi.fn(),
    onModelRequest: null as null | (() => void),
}))

vi.mock('../generationDiagnostics', async (importOriginal) =>
    (await import('./tests/sendChatTestHarness')).generationDiagnosticsModule(
        await importOriginal<typeof import('../generationDiagnostics')>(),
        async (_command, args) => {
            const event = args.event as Record<string, unknown>
            if (event.kind === 'document-start') return 1
            mocks.diagnosticEvents.push(event)
        },
    ))

vi.mock('../iosNative', async () => (await import('./tests/sendChatTestHarness')).iosNativeModule())
vi.mock('../tokenizer', async () => (await import('./tests/sendChatTestHarness')).tokenizerModule({
    // Every message costs ten tokens in the window walk.
    tokenize: vi.fn(async () => 10),
    encodeWithTokenizer: vi.fn(async () => new Array(10).fill(0)),
}))
vi.mock('../alert', async () => (await import('./tests/sendChatTestHarness')).alertModule({
    alertError: mocks.alertError,
}))
vi.mock('../parser/chatML', async () => (await import('./tests/sendChatTestHarness')).chatMLModule())
vi.mock('../parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('./lorebook.svelte', async () => (await import('./tests/sendChatTestHarness')).lorebookModule())
vi.mock('../util', async () => (await import('./tests/sendChatTestHarness')).utilModule({
    findCharacterbyId: () => mocks.currentCharacter,
}))
vi.mock('./request/request', async () => (await import('./tests/sendChatTestHarness')).requestModule(vi.fn(async (request: unknown) => {
    mocks.modelRequests.push(request)
    mocks.onModelRequest?.()
    return mocks.modelResponse
})))
vi.mock('./stableDiff', async () => (await import('./tests/sendChatTestHarness')).stableDiffModule())
vi.mock('./scripts', async () => (await import('./tests/sendChatTestHarness')).scriptsModule({
    processScriptFull: mocks.processScriptFull,
}))
vi.mock('./templates/templates', async () => (await import('./tests/sendChatTestHarness')).templatesModule())
vi.mock('./exampleMessages', async () => (await import('./tests/sendChatTestHarness')).exampleMessagesModule())
vi.mock('./tts', async () => (await import('./tests/sendChatTestHarness')).ttsModule())
vi.mock('./memory/supaMemory', async () => (await import('./tests/sendChatTestHarness')).supaMemoryModule())
vi.mock('./triggers', () => ({
    runTrigger: vi.fn(async (_char: unknown, mode: string, arg: { chat: any }) => {
        const clone = JSON.parse(JSON.stringify(arg.chat))
        if (mode === 'start') return mocks.startTrigger ? mocks.startTrigger(clone) : null
        return mocks.outputTrigger ? mocks.outputTrigger(clone) : null
    }),
}))
vi.mock('./memory/hypamemory', async () => (await import('./tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('./embedding/addinfo', async () => (await import('./tests/sendChatTestHarness')).addinfoModule())
vi.mock('./files/inlays', async () => (await import('./tests/sendChatTestHarness')).inlaysModule())
vi.mock('./models/modelString', async () => (await import('./tests/sendChatTestHarness')).modelStringModule())
vi.mock('./inlayScreen', async () => (await import('./tests/sendChatTestHarness')).inlayScreenModule())
vi.mock('./transformers', async () => (await import('./tests/sendChatTestHarness')).transformersModule())
vi.mock('./memory/hanuraiMemory', async () => (await import('./tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('./memory/hypav2', async () => (await import('./tests/sendChatTestHarness')).hypav2Module())
vi.mock('./memory/hypav3', async () => (await import('./tests/sendChatTestHarness')).hypav3Module({
    getCurrentHypaV3Preset: () => ({ settings: mocks.hypaSettings }),
    hypaMemoryV3: mocks.hypaMemoryV3,
}))
vi.mock('./scriptings', async () => (await import('./tests/sendChatTestHarness')).scriptingsModule())
vi.mock('../model/modellist', async () => (await import('./tests/sendChatTestHarness')).modellistModule())
vi.mock('./modules', async () => (await import('./tests/sendChatTestHarness')).modulesModule())
vi.mock('../globalApi.svelte', async () => (await import('./tests/sendChatTestHarness')).globalApiModule())
vi.mock('../plugins/plugins.svelte', async () => (await import('./tests/sendChatTestHarness')).pluginsModule())
vi.mock('../plugins/pluginDatabaseAccess', async (importOriginal) => (await import('./tests/sendChatTestHarness')).pluginDatabaseAccessModule(importOriginal))
vi.mock('./presetChain', () => ({ activatePresetChainForRequest: vi.fn(async () => undefined) }))
vi.mock('../storage/deviceSettings', () => ({ getDeviceSettings: () => mocks.deviceSettings }))
vi.mock('../storage/persistentDataRuntime.svelte', async () => (await import('./tests/sendChatTestHarness')).persistentRuntimeModule({
    acknowledgeGenerationCompletion: mocks.acknowledge,
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    captureSelectedConversationAuthority: () => mocks.selectedAuthority,
    captureWindowedConversationMutationController: (...args: any[]) => mocks.windowedController?.(...args) ?? null,
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    flushPendingData: mocks.flushPendingData,
    getActiveConversationSession: () => mocks.session,
}))
vi.mock('../storage/persistentDataStoreFactory', async () => (await import('./tests/sendChatTestHarness')).persistentDataStoreFactoryModule(() => mocks.persistentStore))

import type { character, Chat, Database, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { createLeaseStub } from '../storage/persistentDataStore.testSupport'
import { createMetadataOnlySelectedConversation } from '../storage/selectedConversationLifecycle'
import { DBState, selectedCharID } from '../stores.svelte'
import { doingChat, openSelectedHistoryWindow, sendChat } from './index.svelte'
import { findActiveHistoryWindow, getHistoryWindowStart } from './historyWindowIndex'
import { setChatVarOnConversation } from '../parser/chatVar.svelte'
import { language } from '../../lang'

function messages(count: number): Message[] {
    return Array.from({ length: count }, (_, index): Message => ({
        role: index % 2 ? 'char' : 'user',
        data: `m${index}`,
        chatId: `id-${index}`,
    }))
}

function conversationOf(stored: Message[]): Chat {
    return { id: 'chat-a', name: 'Chat A', note: '', localLore: [], fmIndex: -1, message: stored } as Chat
}

function installDatabase(chat: Chat) {
    const owner = {
        type: 'character',
        chaId: 'character-a',
        name: 'character-a',
        chatPage: 0,
        chats: [chat],
        firstMessage: 'greeting',
        alternateGreetings: [],
        desc: '',
        personality: '',
        scenario: '',
        bias: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [],
        defaultVariables: '',
        reloadKeys: 0,
        viewScreen: 'none',
        inlayViewScreen: false,
        supaMemory: false,
        customscript: [],
    } as unknown as character
    DBState.db = {
        characters: [owner],
        statics: { messages: 0 },
        botPresets: [],
        botPresetsId: 0,
        aiModel: 'test-model',
        maxContext: 500,
        maxResponse: 0,
        promptTemplate: [{ type: 'chat', rangeStart: 0, rangeEnd: 'end' }],
        promptSettings: { trimStartNewChat: true, sendName: false, sendChatAsSystem: false, postEndInnerFormat: '' },
        promptInfoInsideChat: false,
        promptTextInfoInsideChat: false,
        customPromptTemplateToggle: '',
        globalChatVariables: {},
        mainPrompt: '',
        additionalPrompt: '',
        globalNote: '',
        jailbreak: '',
        jailbreakToggle: false,
        chainOfThought: false,
        personaPrompt: false,
        promptPreprocess: false,
        descriptionPrefix: '',
        formatingOrder: [],
        bias: [],
        removeIncompleteResponse: false,
        streamingDisplayOptimizationMode: 'off',
        autoContinueMinTokens: 0,
        autoContinueChat: false,
        igpPrompt: '',
        notification: false,
        ttsAutoSpeech: false,
        supaModelType: 'none',
        hanuraiEnable: false,
        hypav2: false,
        hypaV3: false,
        inlayErrorResponse: false,
        presetRegex: [],
    } as unknown as Database
    selectedCharID.set(0)
    // The database proxies its records, so the session must see the proxied objects.
    mocks.currentCharacter = DBState.db.characters[0]
    return DBState.db.characters[0] as character
}

const target = { characterId: 'character-a', conversationId: 'chat-a', navigationGeneration: 0, storeRevision: 1 }

/** A windowed selected conversation backed by an in-memory store. */
function installWindowed(stored: Message[], conversation: Partial<Chat> = {}) {
    const owner = installDatabase({ ...conversationOf([]), ...conversation })
    owner.chats[0] = createMetadataOnlySelectedConversation({ ...conversationOf([]), ...conversation }) as Chat
    const bodyReads: Array<[number, number]> = []
    const mutations: Array<{ start: number, deleteCount: number, messages: Message[] }> = []
    const windowed = { current: true }
    mocks.session = null
    mocks.selectedTarget = target
    mocks.selectedAuthority = {
        kind: 'windowed',
        characterId: 'character-a',
        conversationId: 'chat-a',
        sessionToken: 'windowed-session',
        storeRevision: 1,
        persistedSessionVersion: 0,
        sessionVersion: 0,
        get totalMessages() { return stored.length },
    }
    const page = <T>(startIndex: number, items: T[]) => ({
        revision: 1,
        value: {
            characterId: 'character-a',
            conversationId: 'chat-a',
            messages: items,
            startIndex,
            endIndex: startIndex + items.length,
            totalMessages: stored.length,
            hasMoreBefore: startIndex > 0,
            hasMoreAfter: startIndex + items.length < stored.length,
        },
    })
    const lease = createLeaseStub({
        revision: 1,
        async readConversationWindow(input) {
            bodyReads.push([input.startIndex, input.limit])
            return page(input.startIndex, structuredClone(stored.slice(input.startIndex, input.startIndex + input.limit)))
        },
        async readConversationMessageMetadataWindow(input) {
            return page(input.startIndex, stored.slice(input.startIndex, input.startIndex + input.limit)
                .map((message) => ({ chatId: message.chatId, role: message.role, disabled: message.disabled, parserInert: true })))
        },
        release: vi.fn(async () => undefined),
    })
    mocks.persistentStore = { acquireRevision: vi.fn(async () => lease) }
    mocks.windowedController = (_target: unknown, chat: Chat, absoluteStartIndex: number) => {
        if (!windowed.current || absoluteStartIndex + chat.message.length !== stored.length) return null
        return {
            chat,
            absoluteStartIndex,
            isCurrent: () => windowed.current && absoluteStartIndex + chat.message.length === stored.length,
            applyRange(localStart: number, deleteCount: number, replacement: Message[]) {
                if (!windowed.current) return false
                stored.splice(absoluteStartIndex + localStart, deleteCount, ...structuredClone(replacement))
                chat.message.splice(localStart, deleteCount, ...structuredClone(replacement))
                const { message: _message, ...metadata } = chat
                Object.assign(owner.chats[0], structuredClone(metadata))
                mutations.push({ start: absoluteStartIndex + localStart, deleteCount, messages: structuredClone(replacement) })
                return true
            },
            release: vi.fn(),
        }
    }
    return { owner, bodyReads, mutations, windowed, lease }
}

/** A complete selected conversation with an active session. */
function installComplete(stored: Message[], conversation: Partial<Chat> = {}) {
    const chat = { ...conversationOf(stored), ...conversation }
    const owner = installDatabase(chat)
    mocks.session = new ActiveConversationSession({
        characterId: 'character-a',
        conversationId: 'chat-a',
        conversation: owner.chats[0],
        storeRevision: 1,
    })
    mocks.selectedTarget = target
    mocks.selectedAuthority = null
    mocks.windowedController = null
    mocks.persistentStore = null
    return { owner, session: mocks.session as ActiveConversationSession }
}

function streamingResponse(value: string) {
    return {
        type: 'streaming',
        result: new ReadableStream<Record<string, string>>({
            start(controller) {
                controller.enqueue({ response: value })
                controller.close()
            },
        }),
    }
}

const promptContents = () => (mocks.modelRequests.at(-1)?.formated ?? []).map((chat: any) => chat.content)

describe('sendChat with the history window', () => {
    beforeEach(() => {
        mocks.diagnosticEvents.length = 0
        mocks.acknowledge.mockReset().mockResolvedValue(undefined)
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        vi.spyOn(console, 'debug').mockImplementation(() => undefined)
        doingChat.set(false)
        mocks.modelRequests.length = 0
        mocks.modelResponse = streamingResponse('answer')
        mocks.onModelRequest = null
        mocks.deviceSettings = { generationHistoryLimitEnabled: true, generationHistoryLimitMultiplier: 2 }
        mocks.startTrigger = null
        mocks.outputTrigger = null
        mocks.flushPendingData.mockClear()
        mocks.alertError.mockReset()
        mocks.hypaSettings = {
            preserveOrphanedMemory: false,
            useExperimentalImpl: false,
            recentMemoryRatio: 0,
            similarMemoryRatio: 0,
            queryChatCount: 2,
        }
        mocks.hypaMemoryV3.mockReset().mockImplementation(async (chats: any[], currentTokens: number, _max: number, room: any) => ({
            chats,
            currentTokens,
            memory: room.hypaV3Data,
        }))
        mocks.processScriptFull.mockReset().mockImplementation(async (_char: unknown, data: string) => ({ data, emoChanged: false }))
        mocks.acquireCompleteConversation.mockReset().mockImplementation(async (_reason: string, selected: unknown) => {
            const session = mocks.session
            if (!session) throw new Error('windowed conversation was promoted')
            const pin = session.acquirePin('compatibility')
            return { session, target: selected, release: () => pin.release() }
        })
    })

    const range = (from: number, to: number) => Array.from({ length: to - from }, (_, index) => `m${from + index}`)
    const processed = () => mocks.processScriptFull.mock.calls
        .filter((call) => call[2] === 'editprocess')
        .map((call) => call[1])

    it('reads only the newest messages of a windowed conversation and appends the response after them', async () => {
        const stored = messages(1000)
        const before = structuredClone(stored.slice(0, 900))
        const { bodyReads, mutations } = installWindowed(stored)
        mocks.acknowledge.mockImplementation(async () => { mocks.selectedAuthority.storeRevision = 2 })

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
        expect(mocks.diagnosticEvents.filter(event => event.kind === 'admission').at(-1)).toMatchObject({
            route: 'history-window', admittedMessages: 100, totalMessages: 1000,
        })
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({
            kind: 'persistence', boundary: 'final', savePath: 'windowed', acknowledgedRevision: 2,
        }))
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({
            phase: 'settled', edge: 'exit', outcome: 'ok', savePath: 'windowed',
        }))

        // maxContext 500 times 2 at ten tokens per message keeps a hundred messages.
        expect(promptContents()).toEqual(range(900, 1000))
        expect(Math.min(...bodyReads.map(([start]) => start))).toBeGreaterThanOrEqual(900 - 64)
        expect(mocks.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(stored.slice(0, 900)).toEqual(before)
        expect(stored.at(-1)?.data).toBe('answer')
        expect(mutations.length).toBeGreaterThan(0)
        expect(mutations.every((mutation) => mutation.start >= 900)).toBe(true)
    })

    it('counts the window with o200k_base when the model tokenizer is remote', async () => {
        const { encodeWithTokenizer } = await import('../tokenizer')
        vi.mocked(encodeWithTokenizer).mockClear()
        installWindowed(messages(1000))
        DBState.db.googleClaudeTokenizing = true

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        expect(promptContents()).toEqual(range(900, 1000))
        const tokenizers = new Set(vi.mocked(encodeWithTokenizer).mock.calls.map((call) => call[1]))
        expect([...tokenizers]).toEqual(['o200k_base'])
    })

    it('honors the multiplier', async () => {
        installWindowed(messages(1000))
        mocks.deviceSettings = { generationHistoryLimitEnabled: true, generationHistoryLimitMultiplier: 1.5 }

        await sendChat({ historyLimit: true })

        expect(promptContents()).toEqual(range(925, 1000))
    })

    it('leaves the greeting out when older messages are left out, and keeps it for a short conversation', async () => {
        installWindowed(messages(1000))
        await sendChat({ historyLimit: true })
        expect(promptContents()).not.toContain('greeting')

        installWindowed(messages(4))
        mocks.modelResponse = streamingResponse('answer')
        await sendChat({ historyLimit: true })
        expect(promptContents()).toEqual(['greeting', 'm0', 'm1', 'm2', 'm3'])
    })

    it.each([
        ['without the chat-screen flag', {}, () => {}],
        ['with the device option off', { historyLimit: true }, () => { mocks.deviceSettings.generationHistoryLimitEnabled = false }],
        ['with maxContext 0', { historyLimit: true }, () => { DBState.db.maxContext = 0 }],
        ['with HypaV2', { historyLimit: true }, () => {
            DBState.db.hypav2 = true
            mocks.currentCharacter.supaMemory = true
        }],
    ])('processes the whole conversation %s', async (_name, arg, configure) => {
        installComplete(messages(300))
        configure()

        await sendChat(arg)

        expect(processed()).toEqual(range(0, 300))
    })

    it('builds the same prompt and result from a complete conversation', async () => {
        const windowedStore = messages(300)
        installWindowed(windowedStore)
        await sendChat({ historyLimit: true })
        const windowedPrompt = promptContents()

        const completeStore = messages(300)
        const before = structuredClone(completeStore.slice(0, 200))
        const { session } = installComplete(completeStore)
        mocks.processScriptFull.mockClear()
        mocks.modelResponse = streamingResponse('answer')
        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        expect(processed()).toEqual(range(200, 300))
        expect(promptContents()).toEqual(windowedPrompt)
        const after = session.materializeCompatibilityArray()
        expect(after.slice(0, 200)).toEqual(before)
        expect(after.map((message) => message.data)).toEqual(windowedStore.map((message) => message.data))
    })

    it('opens the window of a complete conversation when a chat variable commits while it is measured', async () => {
        const { owner, session } = installComplete(messages(300))
        const tokenizer = await import('../tokenizer')
        vi.mocked(tokenizer.tokenize).mockImplementationOnce(async () => {
            session.updateChatVariables(new Map([['scratch', 'display']]), { origin: 'display' })
            return 10
        })
        mocks.modelResponse = streamingResponse('answer')

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        expect(session.materializeCompatibilityArray().at(-1)?.data).toBe('answer')
        expect(owner.chats[0].scriptstate).toEqual({ $scratch: 'display' })
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('passes absolute message indices to editprocess scripts', async () => {
        installWindowed(messages(1000))

        await sendChat({ historyLimit: true })

        const indices = mocks.processScriptFull.mock.calls
            .filter((call) => call[2] === 'editprocess' && String(call[1]).startsWith('m'))
            .map((call) => call[3])
        expect(indices[0]).toBe(900)
        expect(indices.at(-1)).toBe(999)
    })

    it('passes the stored index of each message when a disabled message lies inside the window', async () => {
        const stored = messages(1000)
        stored[950].disabled = true
        installWindowed(stored)

        await sendChat({ historyLimit: true })

        const indexOf = new Map(mocks.processScriptFull.mock.calls
            .filter((call) => call[2] === 'editprocess')
            .map((call) => [call[1], call[3]]))
        expect(indexOf.get('m949')).toBe(949)
        expect(indexOf.has('m950')).toBe(false)
        expect(indexOf.get('m951')).toBe(951)
    })

    it.each([
        ['streamed', () => streamingResponse('answer')],
        ['complete', () => ({ type: 'success', result: 'answer' })],
    ])('passes the absolute index of a %s response to editoutput scripts', async (_name, response) => {
        installWindowed(messages(1000))
        mocks.modelResponse = response()

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        const indices = mocks.processScriptFull.mock.calls
            .filter((call) => call[2] === 'editoutput')
            .map((call) => call[3])
        expect(indices.length).toBeGreaterThan(0)
        expect(new Set(indices)).toEqual(new Set([1000]))
    })

    it('stores ids for window messages that lack one before reading them', async () => {
        const stored = messages(30)
        delete stored[29].chatId
        const { mutations } = installWindowed(stored)

        await sendChat({ historyLimit: true })

        expect(stored[29].chatId).toEqual(expect.any(String))
        expect(mutations[0]).toMatchObject({ start: 29, deleteCount: 1 })
    })

    it('stores the ids a run of window messages lacks in one write', async () => {
        const stored = messages(1000)
        for (const message of stored.slice(0, 990)) delete message.chatId
        const before = structuredClone(stored)
        const { mutations } = installWindowed(stored)

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        // Generation also records its streaming flags as metadata-only updates, which write no message.
        const idWrites = mutations.filter((mutation) => mutation.start < 1000 && mutation.messages.length > 0)
        expect(idWrites).toHaveLength(1)
        expect(idWrites[0]).toMatchObject({ start: 900, deleteCount: 90 })
        expect(stored.slice(900, 990).every((message) => typeof message.chatId === 'string')).toBe(true)
        expect(stored.slice(0, 900)).toEqual(before.slice(0, 900))
        expect(stored.slice(900, 1000).map((message) => message.data)).toEqual(before.slice(900).map((message) => message.data))
    })

    it('writes start trigger edits to the window and keeps chat variables', async () => {
        const stored = messages(1000)
        const { owner } = installWindowed(stored)
        mocks.startTrigger = (chat) => {
            chat.message.at(-1).data = 'edited by trigger'
            chat.scriptstate = { $turn: '1' }
            return {
                chat,
                tokens: 0,
                stopSending: false,
                additonalSysPrompt: { start: '', historyend: '', promptend: '' },
            }
        }

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        expect(stored[999].data).toBe('edited by trigger')
        expect(stored.slice(0, 900)).toEqual(messages(900))
        expect(owner.chats[0].scriptstate).toEqual({ $turn: '1' })
        expect(promptContents()).toContain('edited by trigger')
    })

    it('stores chat variables set while the prompt is built when the send stops before a reply', async () => {
        const { owner } = installWindowed(messages(1000))
        const controller = new AbortController()
        mocks.processScriptFull.mockImplementation(async (_char: unknown, data: string, mode: string) => {
            if (mode === 'editprocess') {
                setChatVarOnConversation(findActiveHistoryWindow('character-a', 'chat-a')!.chat, 'mood', 'set')
            }
            return { data, emoChanged: false }
        })
        mocks.onModelRequest = () => controller.abort()
        mocks.modelResponse = { type: 'fail', result: 'stopped' }

        await expect(sendChat({ historyLimit: true, signal: controller.signal })).resolves.toBe(false)

        expect(owner.chats[0].scriptstate).toEqual({ $mood: 'set' })
    })

    it('stores chat variables set before the prompt build failed', async () => {
        const { owner } = installWindowed(messages(1000))
        const failure = new Error('synthetic script failure')
        mocks.processScriptFull.mockImplementation(async (_char: unknown, _data: string, mode: string) => {
            if (mode !== 'editprocess') throw new Error(`unexpected ${mode}`)
            setChatVarOnConversation(findActiveHistoryWindow('character-a', 'chat-a')!.chat, 'mood', 'set')
            throw failure
        })

        await expect(sendChat({ historyLimit: true })).rejects.toBe(failure)

        expect(mocks.modelRequests).toHaveLength(0)
        expect(owner.chats[0].scriptstate).toEqual({ $mood: 'set' })
    })

    it('writes output trigger edits through the window', async () => {
        const stored = messages(1000)
        installWindowed(stored)
        mocks.outputTrigger = (chat) => {
            chat.message.at(-1).data += ' (checked)'
            return { chat }
        }

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        expect(stored.at(-1)?.data).toBe('answer (checked)')
        expect(stored).toHaveLength(1001)
        expect(stored.slice(0, 900)).toEqual(messages(900))
    })

    it('extends a HypaV3 window to the summary boundary and passes the whole memo set', async () => {
        const stored = messages(1000)
        installWindowed(stored, {
            hypaV3Data: { summaries: [{ text: 'summary', chatMemos: ['id-0', 'id-400'], isImportant: false }] } as any,
        })
        DBState.db.hypaV3 = true
        mocks.currentCharacter.supaMemory = true
        mocks.hypaMemoryV3.mockImplementation(async (chats: any[], currentTokens: number, _max: number, room: any) => ({
            chats: chats.slice(-50),
            currentTokens,
            memory: room.hypaV3Data,
        }))

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        const [chats, , , , , , prepared] = mocks.hypaMemoryV3.mock.calls[0]
        expect(chats[0].memo).toBe('id-400')
        expect(chats).toHaveLength(600)
        expect(prepared).toEqual({ effectiveMessageMemos: messages(1000).map((message) => message.chatId) })
    })

    it('carries the response over to the promoted conversation', async () => {
        const stored = messages(1000)
        const { windowed } = installWindowed(stored)
        mocks.onModelRequest = () => {
            // The display parser promotes the conversation while the request is in flight.
            windowed.current = false
            const owner = DBState.db.characters[0]
            owner.chats[0] = conversationOf(structuredClone(stored))
            mocks.session = new ActiveConversationSession({
                characterId: 'character-a',
                conversationId: 'chat-a',
                conversation: owner.chats[0],
                storeRevision: 1,
            })
        }

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)

        const promoted = (mocks.session as ActiveConversationSession).materializeCompatibilityArray()
        expect(promoted).toHaveLength(1001)
        expect(promoted.at(-1)?.data).toBe('answer')
        expect(promoted.slice(0, 1000)).toEqual(messages(1000))
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('carries a streaming response over to a conversation promoted after its first part was stored', async () => {
        const stored = messages(1000)
        const { windowed, mutations } = installWindowed(stored)
        let stream!: ReadableStreamDefaultController<Record<string, string>>
        mocks.modelResponse = {
            type: 'streaming',
            result: new ReadableStream<Record<string, string>>({
                start(controller) {
                    stream = controller
                    controller.enqueue({ response: 'first' })
                },
            }),
        }
        let firstStored!: () => void
        const firstPart = new Promise<void>((resolve) => { firstStored = resolve })
        const record = mutations.push.bind(mutations)
        mutations.push = (...entries) => {
            const length = record(...entries)
            if (entries.some((entry) => entry.messages.some((message) => message.data === 'first'))) firstStored()
            return length
        }

        const sending = sendChat({ historyLimit: true })
        await firstPart
        // The promotion reads the store, which already holds the first part of the response.
        windowed.current = false
        const owner = DBState.db.characters[0]
        owner.chats[0] = conversationOf(structuredClone(stored))
        mocks.session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'chat-a',
            conversation: owner.chats[0],
            storeRevision: 1,
        })
        stream.enqueue({ response: 'first second' })
        stream.close()

        await expect(sending).resolves.toBe(true)
        const promoted = (mocks.session as ActiveConversationSession).materializeCompatibilityArray()
        expect(promoted).toHaveLength(1001)
        expect(promoted.at(-1)?.data).toBe('first second')
        expect(promoted.slice(0, 1000)).toEqual(messages(1000))
        // The rest of the response was written through the promoted session, not the windowed store.
        expect((mocks.session as ActiveConversationSession).version).toBeGreaterThan(0)
        expect(stored.at(-1)?.data).toBe('first')
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('tells the user when no window opens because pending edits could not be saved', async () => {
        installWindowed(messages(1000))
        mocks.selectedAuthority.sessionVersion = 1

        await expect(sendChat({ historyLimit: true })).resolves.toBe(false)

        expect(mocks.flushPendingData).toHaveBeenCalledWith('generation-history-window')
        expect(mocks.modelRequests).toHaveLength(0)
        expect(mocks.alertError).toHaveBeenCalledWith(language.chatConversationActionFailed)
    })

    it.each([false, true])('retries one window change and refuses a second change (twice: %s)', async (twice) => {
        const stored = messages(1000)
        const { lease } = installWindowed(stored)
        const read = lease.readConversationWindow
        lease.readConversationWindow = async (input) => {
            const page = await read(input)
            const version = mocks.selectedAuthority.sessionVersion
            if (twice || version === 0) {
                mocks.selectedAuthority = { ...mocks.selectedAuthority, sessionVersion: version + 1, persistedSessionVersion: version + 1 }
            }
            return page
        }

        await expect(sendChat({ historyLimit: true })).resolves.toBe(!twice)

        expect(mocks.modelRequests).toHaveLength(twice ? 0 : 1)
        if (twice) expect(mocks.alertError).toHaveBeenCalledWith(language.chatConversationActionFailed)
        else expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('restarts preparation from a conversation promoted while the window is read', async () => {
        const stored = messages(1000)
        const { lease, windowed, owner } = installWindowed(stored)
        const read = lease.readConversationWindow
        lease.readConversationWindow = async (input) => {
            const page = await read(input)
            windowed.current = false
            mocks.selectedAuthority = null
            owner.chats[0] = conversationOf(structuredClone(stored))
            owner.chats[0].scriptstate = { $button: 'applied' }
            mocks.session = new ActiveConversationSession({
                characterId: 'character-a', conversationId: 'chat-a', conversation: owner.chats[0], storeRevision: 1,
            })
            return page
        }
        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
        expect(mocks.session.materializeCompatibilityArray().at(-1)?.data).toBe('answer')
        expect(mocks.session.readChatVariable('button')).toBe('applied')
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('refuses to write the response when the promoted conversation no longer holds the window', async () => {
        const stored = messages(1000)
        const { windowed } = installWindowed(stored)
        mocks.onModelRequest = () => {
            windowed.current = false
            const owner = DBState.db.characters[0]
            const shifted = structuredClone(stored)
            shifted.splice(995, 1)
            owner.chats[0] = conversationOf(shifted)
            mocks.session = new ActiveConversationSession({
                characterId: 'character-a',
                conversationId: 'chat-a',
                conversation: owner.chats[0],
                storeRevision: 1,
            })
        }

        await expect(sendChat({ historyLimit: true })).resolves.toBe(false)

        expect((mocks.session as ActiveConversationSession).totalMessages).toBe(999)
        expect(mocks.alertError).toHaveBeenCalledWith(language.generationConversationChanged)
    })
})

describe('openSelectedHistoryWindow', () => {
    beforeEach(() => {
        mocks.deviceSettings = { generationHistoryLimitEnabled: true, generationHistoryLimitMultiplier: 2 }
        mocks.flushPendingData.mockClear()
    })

    it('opens the token window and registers it for the step that runs over it', async () => {
        const stored = messages(1000)
        installWindowed(stored)

        const window = await openSelectedHistoryWindow({ register: true })

        expect(getHistoryWindowStart(window!.chat)).toBe(900)
        expect(window!.chat.message.map((message) => message.data)).toEqual(
            Array.from({ length: 100 }, (_, index) => `m${900 + index}`),
        )
        expect(findActiveHistoryWindow('character-a', 'chat-a')).toBe(window!.controller)
        window!.release()
        expect(findActiveHistoryWindow('character-a', 'chat-a')).toBeNull()
    })

    it('opens a tail from an absolute index and writes it back there', async () => {
        const stored = messages(1000)
        const before = structuredClone(stored.slice(0, 995))
        const { bodyReads } = installWindowed(stored)

        const window = await openSelectedHistoryWindow({ tailStart: (total) => total - 5 })

        expect(bodyReads).toEqual([[995, 5]])
        expect(findActiveHistoryWindow('character-a', 'chat-a')).toBeNull()
        expect(window!.controller.applyRange(4, 1, [{ role: 'char', data: 'edited', chatId: 'id-999' }], 'edit')).toBe(true)
        window!.release()
        expect(stored.slice(0, 995)).toEqual(before)
        expect(stored.at(-1)?.data).toBe('edited')
    })

    it('opens a tail of a complete conversation from its session', async () => {
        const stored = messages(1000)
        const { session } = installComplete(stored)

        const window = await openSelectedHistoryWindow({ tailStart: () => 998 })

        expect(window!.chat.message.map((message) => message.data)).toEqual(['m998', 'm999'])
        expect(window!.controller.applyRange(2, 0, [{ role: 'user', data: 'appended', chatId: 'appended' }], 'append')).toBe(true)
        window!.release()
        expect(session.totalMessages).toBe(1001)
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('appended')
    })
})
