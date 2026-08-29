import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'

const harness = vi.hoisted(() => {
    function store<T>(initial: T, onSet?: (value: T) => void) {
        let value = initial
        const subscribers = new Set<(value: T) => void>()
        return {
            subscribe(run: (value: T) => void) {
                run(value)
                subscribers.add(run)
                return () => subscribers.delete(run)
            },
            set(next: T) {
                value = next
                onSet?.(next)
                for (const run of subscribers) run(value)
            },
            update(updater: (value: T) => T) {
                this.set(updater(value))
            },
            value: () => value,
        }
    }

    const events: string[] = []
    const DBState = { db: {} as any }
    const selectedCharID = store(0)
    const CharEmotion = store<Record<string, Array<[string, string, number]>>>({})
    const doingChat = store(false, (value) => events.push(`doing:${value}`))
    const requests: any[] = []
    const characters = new Map<string, any>()
    const acknowledge = vi.fn(async () => {
        events.push('ack')
    })
    const peerSync = vi.fn(async () => {
        events.push('peer')
    })
    const requestChatData = vi.fn(async () => {
        events.push('request')
        const next = requests.shift()
        if (next instanceof Error) throw next
        return next
    })
    const tokenize = vi.fn(async () => 8)
    const tokenizeNum = vi.fn(async () => [1])
    const runTrigger = vi.fn(async (_char?: any, _event?: string, _context?: any): Promise<any> => null)
    const processScriptFull = vi.fn(async (_char, data: string, _mode?: string) => ({
        data,
        emoChanged: false,
    }))
    const runInlayScreen = vi.fn((_char: any, text: string): {
        text: string
        promise?: Promise<string>
    } => ({ text }))
    const sayTTS = vi.fn(async () => undefined)
    const stableDiff = vi.fn(async () => undefined)
    const notificationRequest = vi.fn(async () => 'granted')
    const notificationConstruct = vi.fn((_title?: string, _options?: NotificationOptions) => {
        events.push('notification')
    })
    const embeddingAddText = vi.fn(async () => undefined)
    const embeddingSearch = vi.fn(async () => [['happy', 1]])

    return {
        store,
        events,
        DBState,
        selectedCharID,
        CharEmotion,
        doingChat,
        generationReservation: null as symbol | null,
        requests,
        characters,
        acknowledge,
        peerSync,
        requestChatData,
        tokenize,
        tokenizeNum,
        runTrigger,
        processScriptFull,
        runInlayScreen,
        sayTTS,
        stableDiff,
        notificationRequest,
        notificationConstruct,
        embeddingAddText,
        embeddingSearch,
        isLastCharPunctuation: vi.fn(() => true),
    }
})

vi.mock('../storage/database.svelte', () => ({
    changeToPreset: vi.fn(async () => undefined),
    setCurrentChat: vi.fn(),
}))

vi.mock('../stores.svelte', () => ({
    DBState: harness.DBState,
    selectedCharID: harness.selectedCharID,
    CharEmotion: harness.CharEmotion,
}))

vi.mock('../tokenizer', () => ({
    ChatTokenizer: class {
        async tokenizeChat() {
            return 1
        }
        async tokenizeChats(chats: unknown[]) {
            return chats.length
        }
    },
    tokenize: harness.tokenize,
    tokenizeNum: harness.tokenizeNum,
}))

vi.mock('../../lang', () => ({
    language: {
        errors: { toomuchtoken: 'too many tokens', httpError: 'HTTP error: ' },
        otherUserRequesting: 'peer busy',
    },
}))

vi.mock('../alert', () => ({ alertError: vi.fn(), alertToast: vi.fn() }))
vi.mock('../parser/chatML', () => ({ parseChatML: vi.fn(() => []) }))
vi.mock('./lorebook.svelte', () => ({
    loadLoreBookV3Prompt: vi.fn(async () => ({ actives: [] })),
}))
vi.mock('../util', () => ({
    findCharacterbyId: vi.fn((id: string) => harness.characters.get(id)),
    getAuthorNoteDefaultText: vi.fn(() => ''),
    getPersonaPrompt: vi.fn(() => ''),
    getUserName: vi.fn(() => 'User'),
    isLastCharPunctuation: harness.isLastCharPunctuation,
    trimUntilPunctuation: vi.fn((text: string) => text),
    parseToggleSyntax: vi.fn(() => []),
    prebuiltAssetCommand: '',
}))
vi.mock('./request/request', () => ({ requestChatData: harness.requestChatData }))
vi.mock('./stableDiff', () => ({ stableDiff: harness.stableDiff }))
vi.mock('./scripts', () => ({
    createPromptScriptOperationScope: () => ({
        assertOwnerCurrent: vi.fn(),
        adoptMessageId: vi.fn(),
        parse: (_char: unknown, text: string) => text,
        finish: vi.fn(),
        finishAfterError: vi.fn(),
        release: vi.fn(),
    }),
    processScript: vi.fn(async (_char, text: string) => text),
    processScriptFull: harness.processScriptFull,
    risuChatParser: vi.fn((text: string) => text),
}))
vi.mock('./exampleMessages', () => ({ exampleMessage: vi.fn(() => []) }))
vi.mock('./tts', () => ({ sayTTS: harness.sayTTS }))
vi.mock('./memory/supaMemory', () => ({ supaMemory: vi.fn() }))
vi.mock('uuid', () => ({ v4: vi.fn(() => `id-${Math.random()}`) }))
vi.mock('./group', () => ({ groupOrder: vi.fn((items: any[]) => items) }))
vi.mock('./triggers', () => ({ runTrigger: harness.runTrigger }))
vi.mock('./memory/hypamemory', () => ({
    HypaProcesser: class {
        addText = harness.embeddingAddText
        similaritySearchScored = harness.embeddingSearch
    },
}))
vi.mock('./embedding/addinfo', () => ({ additionalInformations: vi.fn(async () => '') }))
vi.mock('./files/inlays', () => ({ getInlayAsset: vi.fn(async () => null) }))
vi.mock('./models/modelString', () => ({ getGenerationModelString: vi.fn(() => 'test-model') }))
vi.mock('../sync/multiuser', () => ({
    connectionOpen: false,
    peerRevertChat: vi.fn(),
    peerSafeCheck: vi.fn(async () => true),
    peerSync: harness.peerSync,
}))
vi.mock('./inlayScreen', () => ({ runInlayScreen: harness.runInlayScreen }))
vi.mock('./prereroll', () => ({ addRerolls: vi.fn() }))
vi.mock('./transformers', () => ({ runImageEmbedding: vi.fn() }))
vi.mock('./memory/hanuraiMemory', () => ({ hanuraiMemory: vi.fn() }))
vi.mock('./memory/hypav2', () => ({ hypaMemoryV2: vi.fn() }))
vi.mock('./scriptings', () => ({ runLuaEditTrigger: vi.fn(async (_char, _event, value) => value) }))
vi.mock('../model/modellist', () => ({
    getModelInfo: vi.fn(() => ({ flags: [] })),
    LLMFlags: { hasImageInput: 'hasImageInput' },
}))
vi.mock('./memory/hypav3', () => ({ hypaMemoryV3: vi.fn() }))
vi.mock('./modules', () => ({ getModuleAssets: vi.fn(() => []), getModuleToggles: vi.fn(() => '') }))
vi.mock('../globalApi.svelte', () => ({ readImage: vi.fn() }))
vi.mock('../plugins/plugins.svelte', () => ({ pluginV2: { chatOutput: new Set() } }))
vi.mock('./presetChain', () => ({
    activatePresetChainForRequest: vi.fn(async (_db, _change, _random, _missing, setBusy) => {
        setBusy(true)
    }),
}))
vi.mock('./generationState', () => ({
    doingChat: harness.doingChat,
    reserveGeneration: () => {
        if (harness.generationReservation || harness.doingChat.value()) return null
        const token = Symbol('generation-reservation')
        harness.generationReservation = token
        harness.doingChat.set(true)
        let released = false
        return {
            isCurrent: () => !released && harness.generationReservation === token,
            release: () => {
                if (released) return
                released = true
                if (harness.generationReservation !== token) return
                harness.generationReservation = null
                harness.doingChat.set(false)
            },
        }
    },
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    acknowledgeGenerationCompletion: harness.acknowledge,
    captureSelectedConversationTarget: () => null,
    acquireCompleteConversation: vi.fn(),
    getActiveConversationSession: () => null,
    invalidateActiveConversationSession: vi.fn(),
}))

import { sendChat } from './index.svelte'

function makeCharacter(id = 'char-a') {
    return {
        type: 'character',
        chaId: id,
        name: id,
        desc: '',
        firstMessage: 'Hello',
        alternateGreetings: [],
        bias: [],
        chats: [{
            id: `chat-${id}`,
            name: 'Chat',
            note: '',
            fmIndex: -1,
            localLore: [],
            message: [{ role: 'user', data: 'Hi', chatId: `user-${id}` }],
        }],
        chatPage: 0,
        reloadKeys: 0,
        emotionImages: [],
        viewScreen: 'none',
        inlayViewScreen: false,
        utilityBot: false,
        supaMemory: false,
        additionalAssets: [],
    }
}

function makeDatabase(character = makeCharacter()) {
    return {
        characters: [character],
        statics: { messages: 0 },
        botPresets: [],
        botPresetsId: 0,
        globalChatVariables: {},
        promptInfoInsideChat: false,
        promptTextInfoInsideChat: false,
        aiModel: 'gpt-test',
        maxContext: 4096,
        maxResponse: 128,
        promptTemplate: null,
        promptSettings: { trimStartNewChat: false },
        mainPrompt: '@@system\nMain',
        additionalPrompt: '',
        promptPreprocess: false,
        jailbreakToggle: false,
        jailbreak: '',
        globalNote: '',
        chainOfThought: false,
        formatingOrder: ['main', 'description', 'chats', 'lastChat'],
        bias: [],
        outputImageModal: false,
        rememberToolUsage: false,
        removeIncompleteResponse: false,
        autoContinueMinTokens: 0,
        autoContinueChat: false,
        igpPrompt: '',
        ttsAutoSpeech: false,
        notification: false,
        streamingDisplayOptimizationMode: 'off',
    }
}

function success(result: string) {
    return { type: 'success', result }
}

function streaming(result: string) {
    return {
        type: 'streaming',
        result: new ReadableStream({
            start(controller) {
                controller.enqueue({ 0: result })
                controller.close()
            },
        }),
    }
}

function streamingSnapshots(...results: string[]) {
    return {
        type: 'streaming',
        result: new ReadableStream({
            start(controller) {
                for (const result of results) controller.enqueue({ 0: result })
                controller.close()
            },
        }),
    }
}

function deferred<T = void>() {
    let resolve!: (value: T | PromiseLike<T>) => void
    let reject!: (reason?: unknown) => void
    const promise = new Promise<T>((res, rej) => {
        resolve = res
        reject = rej
    })
    return { promise, resolve, reject }
}

let consoleLog: ReturnType<typeof vi.spyOn>

beforeAll(() => {
    consoleLog = vi.spyOn(console, 'log').mockImplementation(() => undefined)
})

afterAll(() => {
    consoleLog.mockRestore()
    vi.unstubAllGlobals()
})

beforeEach(() => {
    vi.clearAllMocks()
    harness.events.length = 0
    harness.requests.length = 0
    harness.characters.clear()
    const character = makeCharacter()
    harness.characters.set(character.chaId, character)
    harness.DBState.db = makeDatabase(character)
    harness.selectedCharID.set(0)
    harness.CharEmotion.set({})
    harness.generationReservation = null
    harness.doingChat.set(false)
    harness.events.length = 0
    harness.tokenize.mockResolvedValue(8)
    harness.isLastCharPunctuation.mockReturnValue(true)
    harness.acknowledge.mockImplementation(async () => {
        harness.events.push('ack')
    })
    harness.requestChatData.mockImplementation(async () => {
        harness.events.push('request')
        const next = harness.requests.shift()
        if (next instanceof Error) throw next
        return next
    })
    harness.runTrigger.mockResolvedValue(null)
    harness.processScriptFull.mockImplementation(async (_char, data: string, _mode?: string) => ({
        data,
        emoChanged: false,
    }))
    harness.runInlayScreen.mockImplementation((_char, text: string) => ({ text }))
    harness.sayTTS.mockResolvedValue(undefined)
    harness.stableDiff.mockResolvedValue(undefined)
    harness.embeddingAddText.mockResolvedValue(undefined)
    harness.embeddingSearch.mockResolvedValue([['happy', 1]])
    harness.notificationRequest.mockResolvedValue('granted')

    class TestNotification {
        static requestPermission = harness.notificationRequest
        onclick: (() => void) | null = null
        constructor(title: string, options?: NotificationOptions) {
            harness.notificationConstruct(title, options)
        }
    }
    vi.stubGlobal('Notification', TestNotification)
})

describe('sendChat generation durability control flow', () => {
    it('acknowledges a non-streaming response before notification and peer publication', async () => {
        const acknowledgement = deferred()
        harness.acknowledge.mockImplementationOnce(async () => {
            harness.events.push('ack:start')
            await acknowledgement.promise
            harness.events.push('ack:done')
        })
        harness.DBState.db.notification = true
        harness.requests.push(success('Response'))

        const sending = sendChat()
        await vi.waitFor(() => expect(harness.acknowledge).toHaveBeenCalledOnce())

        expect(harness.notificationConstruct).not.toHaveBeenCalled()
        expect(harness.peerSync).not.toHaveBeenCalled()
        acknowledgement.resolve()
        await expect(sending).resolves.toBe(true)

        expect(harness.DBState.db.characters[0].chats[0].message.at(-1).data).toBe('Response')
        expect(harness.events.indexOf('ack:done')).toBeLessThan(harness.events.indexOf('notification'))
        expect(harness.events.indexOf('ack:done')).toBeLessThan(harness.events.indexOf('peer'))
    })

    it.each([
        ['streaming', streaming('Stream response'), {}],
        ['non-streaming continue', success(' continued'), { continue: true }],
    ])('acknowledges %s completion after applying the response', async (_name, response, arg) => {
        if ('continue' in arg) {
            harness.DBState.db.characters[0].chats[0].message.push({
                role: 'char',
                data: 'Existing',
                chatId: 'existing-response',
            })
        }
        harness.requests.push(response)

        await expect(sendChat(-1, arg)).resolves.toBe(true)

        expect(harness.acknowledge).toHaveBeenCalledOnce()
        expect(harness.DBState.db.characters[0].chats[0].message.at(-1).data).toContain(
            'continue' in arg ? 'Existing' : 'Stream response',
        )
    })

    it('does not acknowledge preview, abort, or provider failure paths', async () => {
        await expect(sendChat(-1, { preview: true })).resolves.toBe(true)

        harness.doingChat.set(false)
        harness.events.length = 0
        harness.requests.push(success('Ignored'))
        const controller = new AbortController()
        controller.abort()
        await expect(sendChat(-1, { signal: controller.signal })).resolves.toBe(false)

        harness.doingChat.set(false)
        harness.events.length = 0
        harness.requests.push({ type: 'fail', result: 'provider failed' })
        await expect(sendChat()).resolves.toBe(false)

        expect(harness.acknowledge).not.toHaveBeenCalled()
    })

    it('durably acknowledges a live response before propagating tokenization failure', async () => {
        const failure = new Error('tokenization failed')
        harness.requests.push(success('Kept response'))
        harness.tokenize.mockRejectedValueOnce(failure)

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.DBState.db.characters[0].chats[0].message.at(-1).data).toBe('Kept response')
        expect(harness.acknowledge).toHaveBeenCalledOnce()
    })

    it('stages the first raw provider result when its semantic output pass fails', async () => {
        const failure = new Error('first semantic output pass failed')
        const acknowledgement = deferred()
        harness.requests.push(success('Raw provider response'))
        harness.processScriptFull.mockImplementation(async (_char, data: string, mode?: string) => {
            if (mode === 'editoutput') throw failure
            return { data, emoChanged: false }
        })
        harness.acknowledge.mockImplementationOnce(async () => {
            await acknowledgement.promise
        })

        const sending = sendChat()
        await vi.waitFor(() => expect(harness.acknowledge).toHaveBeenCalledOnce())

        const output = harness.DBState.db.characters[0].chats[0].message.at(-1)
        expect(output).toMatchObject({
            role: 'char',
            data: 'Raw provider response',
            generationInfo: expect.objectContaining({ generationId: expect.any(String) }),
        })
        const state = await Promise.race([
            sending.then(() => 'resolved', () => 'rejected'),
            new Promise<string>((resolve) => setTimeout(() => resolve('pending'), 0)),
        ])
        expect(state).toBe('pending')

        acknowledgement.resolve()
        await expect(sending).rejects.toBe(failure)
    })

    it('stages the latest multiline raw result when a middle semantic pass fails', async () => {
        const failure = new Error('middle multiline semantic pass failed')
        let outputPass = 0
        harness.requests.push({
            type: 'multiline',
            result: [
                ['char', 'First choice'],
                ['char', 'Raw second choice'],
                ['char', 'Unreached third choice'],
            ],
        })
        harness.processScriptFull.mockImplementation(async (_char, data: string, mode?: string) => {
            if (mode === 'editoutput' && ++outputPass === 2) throw failure
            return { data, emoChanged: false }
        })

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.DBState.db.characters[0].chats[0].message.at(-1).data).toBe(
            'Raw second choice',
        )
        expect(harness.acknowledge).toHaveBeenCalledOnce()
    })

    it('stages the latest streaming snapshot when an intermediate semantic pass fails', async () => {
        const failure = new Error('streaming semantic pass failed')
        let outputPass = 0
        harness.requests.push(streamingSnapshots('First snapshot', 'Latest raw snapshot'))
        harness.processScriptFull.mockImplementation(async (_char, data: string, mode?: string) => {
            if (mode === 'editoutput' && ++outputPass === 2) throw failure
            return { data, emoChanged: false }
        })

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.DBState.db.characters[0].chats[0].message.at(-1).data).toBe(
            'Latest raw snapshot',
        )
        expect(harness.acknowledge).toHaveBeenCalledOnce()
    })

    it.each([
        ['inlay processing', (failure: Error) => {
            harness.runInlayScreen.mockReturnValueOnce({
                text: 'Kept response',
                promise: Promise.reject(failure),
            })
        }],
        ['TTS', (failure: Error) => {
            harness.DBState.db.ttsAutoSpeech = true
            harness.sayTTS.mockRejectedValueOnce(failure)
        }],
        ['IGP', (failure: Error) => {
            harness.DBState.db.igpPrompt = 'IGP prompt'
            harness.requests.push(failure)
        }],
        ['emotion embedding', (failure: Error) => {
            const character = harness.DBState.db.characters[0]
            character.viewScreen = 'emotion'
            character.emotionImages = [['happy', 'asset']]
            harness.DBState.db.emotionProcesser = 'embedding'
            harness.embeddingAddText.mockRejectedValueOnce(failure)
        }],
        ['image generation', (failure: Error) => {
            harness.DBState.db.characters[0].viewScreen = 'imggen'
            harness.stableDiff.mockRejectedValueOnce(failure)
        }],
    ])('acknowledges before propagating %s failure after a response is live', async (_name, configure) => {
        const failure = new Error(`${_name} failed`)
        harness.requests.push(success('Kept response'))
        configure(failure)

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.DBState.db.characters[0].chats[0].message.at(-1).data).toContain('Kept response')
        expect(harness.acknowledge).toHaveBeenCalledOnce()
        expect(harness.peerSync).not.toHaveBeenCalled()
    })

    it('acknowledges an emotion embedding early return', async () => {
        const character = harness.DBState.db.characters[0]
        character.viewScreen = 'emotion'
        character.emotionImages = [['happy', 'asset']]
        harness.DBState.db.emotionProcesser = 'embedding'
        harness.requests.push(success('Emotion response'))

        await expect(sendChat()).resolves.toBe(true)

        expect(harness.acknowledge).toHaveBeenCalledOnce()
        expect(harness.CharEmotion.value()[character.chaId]?.at(-1)?.[0]).toBe('happy')
    })

    it('acknowledges before auto-continue recursion and before releasing doingChat', async () => {
        const firstAcknowledgement = deferred()
        harness.DBState.db.autoContinueMinTokens = 2
        harness.tokenize.mockResolvedValue(1)
        harness.requests.push(success('First part'), success('Second part'))
        harness.acknowledge
            .mockImplementationOnce(async () => {
                harness.events.push('ack:first:start')
                await firstAcknowledgement.promise
                harness.events.push('ack:first:done')
            })
            .mockImplementationOnce(async () => {
                harness.events.push('ack:second')
            })

        const sending = sendChat()
        await vi.waitFor(() => expect(harness.acknowledge).toHaveBeenCalledTimes(1))

        expect(harness.requestChatData).toHaveBeenCalledOnce()
        expect(harness.events).not.toContain('doing:false')
        firstAcknowledgement.resolve()
        await expect(sending).resolves.toBe(true)

        expect(harness.requestChatData).toHaveBeenCalledTimes(2)
        expect(harness.acknowledge).toHaveBeenCalledTimes(2)
        expect(harness.events.indexOf('ack:first:done')).toBeLessThan(
            harness.events.indexOf('doing:false'),
        )
    })

    it('does not recurse or release doingChat when auto-continue acknowledgement fails', async () => {
        const failure = new Error('local commit failed')
        harness.DBState.db.autoContinueMinTokens = 2
        harness.tokenize.mockResolvedValue(1)
        harness.requests.push(success('First part'), success('Must not run'))
        harness.acknowledge.mockRejectedValueOnce(failure)

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.requestChatData).toHaveBeenCalledOnce()
        expect(harness.acknowledge).toHaveBeenCalledOnce()
        expect(harness.events).not.toContain('doing:false')
    })

    it('acknowledges before output-trigger resend recursion', async () => {
        let outputCount = 0
        harness.runTrigger.mockImplementation(async (_char, event, context) => {
            if (event !== 'output') return null
            outputCount++
            return outputCount === 1
                ? { chat: context.chat, sendAIprompt: true }
                : { chat: context.chat }
        })
        harness.requests.push(success('First response'), success('Resent response'))

        await expect(sendChat()).resolves.toBe(true)

        expect(harness.requestChatData).toHaveBeenCalledTimes(2)
        expect(harness.acknowledge).toHaveBeenCalledTimes(2)
        const firstAck = harness.events.indexOf('ack')
        expect(firstAck).toBeGreaterThan(-1)
        expect(firstAck).toBeLessThan(harness.events.indexOf('doing:false'))
    })

    it('does not resend when output-trigger acknowledgement fails', async () => {
        const failure = new Error('local commit failed')
        harness.runTrigger.mockImplementation(async (_char, event, context) =>
            event === 'output'
                ? { chat: context.chat, sendAIprompt: true }
                : null,
        )
        harness.requests.push(success('First response'), success('Must not run'))
        harness.acknowledge.mockRejectedValueOnce(failure)

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.requestChatData).toHaveBeenCalledOnce()
        expect(harness.acknowledge).toHaveBeenCalledOnce()
        expect(harness.events).not.toContain('doing:false')
    })

    it('acknowledges each generated group member response', async () => {
        const first = makeCharacter('member-a')
        const second = makeCharacter('member-b')
        harness.characters.clear()
        harness.characters.set(first.chaId, first)
        harness.characters.set(second.chaId, second)
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: [first.chaId, second.chaId],
            characterActive: [true, true],
            characterTalks: [1, 1],
            orderByOrder: true,
            chatPage: 0,
            reloadKeys: 0,
            supaMemory: false,
            chats: [{
                id: 'group-chat',
                name: 'Group chat',
                note: '',
                localLore: [],
                message: [{ role: 'user', data: 'Hi group', saying: first.chaId, chatId: 'group-user' }],
            }],
        }
        harness.DBState.db = makeDatabase(group as any)
        harness.requests.push(success('Member A'), success('Member B'))

        await expect(sendChat()).resolves.toBe(true)

        expect(harness.requestChatData).toHaveBeenCalledTimes(2)
        expect(harness.acknowledge).toHaveBeenCalledTimes(2)
        expect(group.chats[0].message.filter((message) => message.role === 'char')).toHaveLength(2)
    })

    it('does not publish terminal side effects when local acknowledgement fails', async () => {
        const failure = new Error('local PDS failed')
        harness.DBState.db.notification = true
        harness.requests.push(success('Locally dirty response'))
        harness.acknowledge.mockRejectedValueOnce(failure)

        await expect(sendChat()).rejects.toBe(failure)

        expect(harness.acknowledge).toHaveBeenCalledOnce()
        expect(harness.notificationConstruct).not.toHaveBeenCalled()
        expect(harness.peerSync).not.toHaveBeenCalled()
    })
})
