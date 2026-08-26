import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    session: null as any,
    currentCharacter: null as any,
    events: [] as string[],
    modelResponse: null as any,
    modelRequestCount: 0,
    modelRequests: [] as any[],
    outputTrigger: null as null | ((chat: any) => Promise<any> | any),
    tokenizeResult: null as Promise<number> | null,
    inlay: null as null | ((data: string) => { text: string, promise?: Promise<string> }),
    listeners: new Set<(event: any) => Promise<void> | void>(),
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
    tokenize: vi.fn(async () => {
        mocks.events.push('tokenize-result')
        return mocks.tokenizeResult ? await mocks.tokenizeResult : 1
    }),
    tokenizeNum: vi.fn(async () => []),
}))

vi.mock('../../lang', () => ({
    changeLanguage: vi.fn(),
    language: {
        errors: { toomuchtoken: 'too many tokens', httpError: 'http error' },
        otherUserRequesting: 'other user requesting',
    },
}))

vi.mock('../alert', () => ({
    alertError: vi.fn(),
    alertToast: vi.fn(),
}))

vi.mock('../parser/chatML', () => ({ parseChatML: (value: string) => value }))
vi.mock('../parser/parser.svelte', () => ({
    risuChatParser: (value: string) => value,
}))
vi.mock('./lorebook.svelte', () => ({
    loadLoreBookV3Prompt: vi.fn(async () => ({ actives: [] })),
}))
vi.mock('../util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(),
    encryptBuffer: vi.fn(),
    selectSingleFile: vi.fn(),
    findCharacterbyId: () => mocks.currentCharacter,
    getAuthorNoteDefaultText: () => '',
    getPersonaPrompt: () => '',
    getUserName: () => 'User',
    isLastCharPunctuation: () => true,
    trimUntilPunctuation: (value: string) => value,
    parseToggleSyntax: () => [],
    prebuiltAssetCommand: '',
}))
vi.mock('./request/request', () => ({
    requestChatData: vi.fn(async (request: unknown, purpose: string) => {
        if (purpose === 'emotion') {
            mocks.events.push('igp-request')
            return '|igp'
        }
        mocks.modelRequestCount += 1
        mocks.modelRequests.push(request)
        return mocks.modelResponse
    }),
}))
vi.mock('./stableDiff', () => ({ stableDiff: vi.fn() }))
vi.mock('./scripts', () => ({
    createPromptScriptOperationScope: () => ({
        assertOwnerCurrent: vi.fn(),
        adoptMessageId: vi.fn(),
        parse: (_char: unknown, text: string) => text,
        finish: vi.fn(),
        finishAfterError: vi.fn(),
        release: vi.fn(),
    }),
    processScript: vi.fn(async (_char: unknown, data: string) => data),
    processScriptFull: vi.fn(async (
        _char: unknown,
        data: string,
        mode: string,
    ) => {
        if (mode === 'editoutput') mocks.events.push('output-script')
        return { data, emoChanged: false }
    }),
    risuChatParser: (value: string) => value,
    resetScriptCache: vi.fn(),
}))
vi.mock('./templates/templates', () => ({
    prebuiltNAIpresets: [],
    prebuiltPresets: { OAI: { mainPrompt: '', jailbreak: '' } },
}))
vi.mock('./exampleMessages', () => ({ exampleMessage: () => [] }))
vi.mock('./tts', () => ({ sayTTS: vi.fn(async () => undefined) }))
vi.mock('./memory/supaMemory', () => ({ supaMemory: vi.fn() }))
vi.mock('./group', () => ({ groupOrder: (value: unknown) => value }))
vi.mock('./triggers', () => ({
    runTrigger: vi.fn(async (_char: unknown, mode: string, arg: { chat: any }) => {
        if (mode === 'start') return null
        mocks.events.push('output-trigger')
        const clone = JSON.parse(JSON.stringify(arg.chat))
        return mocks.outputTrigger ? await mocks.outputTrigger(clone) : { chat: clone }
    }),
}))
vi.mock('./memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('./embedding/addinfo', () => ({ additionalInformations: vi.fn(async () => '') }))
vi.mock('./files/inlays', () => ({ getInlayAsset: vi.fn(async () => null) }))
vi.mock('./models/modelString', () => ({ getGenerationModelString: () => 'test-model' }))
vi.mock('../sync/multiuser', () => ({
    connectionOpen: false,
    peerRevertChat: vi.fn(),
    peerSafeCheck: vi.fn(async () => true),
    peerSync: vi.fn(async () => undefined),
}))
vi.mock('./inlayScreen', () => ({
    runInlayScreen: (_char: unknown, data: string) => {
        mocks.events.push('inlay-sync')
        return mocks.inlay ? mocks.inlay(data) : { text: data }
    },
}))
vi.mock('./prereroll', () => ({ addRerolls: vi.fn() }))
vi.mock('./transformers', () => ({ runImageEmbedding: vi.fn() }))
vi.mock('./memory/hanuraiMemory', () => ({ hanuraiMemory: vi.fn() }))
vi.mock('./memory/hypav2', () => ({ hypaMemoryV2: vi.fn() }))
vi.mock('./memory/hypav3', () => ({ hypaMemoryV3: vi.fn() }))
vi.mock('./scriptings', () => ({
    runLuaEditTrigger: vi.fn(async (_char: unknown, _mode: string, value: unknown) => value),
}))
vi.mock('../model/modellist', () => ({
    getModelInfo: () => ({ flags: [] }),
    LLMFlags: { hasImageInput: 'hasImageInput' },
}))
vi.mock('./modules', () => ({
    getModuleAssets: () => [],
    getModuleToggles: () => '',
    moduleUpdate: vi.fn(),
}))
vi.mock('../globalApi.svelte', () => ({ readImage: vi.fn() }))
vi.mock('../plugins/plugins.svelte', () => ({
    pluginV2: { chatOutput: mocks.listeners },
}))
vi.mock('./presetChain', () => ({ activatePresetChainForRequest: vi.fn(async () => undefined) }))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    acknowledgeGenerationCompletion: vi.fn(async () => undefined),
    getActiveConversationSession: () => mocks.session,
    invalidateActiveConversationSession: () => {
        mocks.events.push('invalidate-session')
        mocks.session?.invalidate()
        mocks.session = null
    },
}))

import type { character, Chat, Database, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { DBState, selectedCharID } from '../stores.svelte'
import { doingChat, sendChat } from './index.svelte'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function makeChat(messages: Message[] = [{
    role: 'user',
    data: 'hello',
    chatId: 'user-message',
}]) {
    return {
        id: 'chat-a',
        name: 'Chat A',
        note: '',
        localLore: [],
        fmIndex: -1,
        message: messages,
    } as Chat
}

function makeCharacter(chat: Chat, id = 'character-a') {
    return {
        type: 'character',
        chaId: id,
        name: id,
        chatPage: 0,
        chats: [chat],
        firstMessage: '',
        alternateGreetings: [''],
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
    } as unknown as character
}

function installDatabase(chat = makeChat(), extraCharacters: character[] = []) {
    const installedCharacter = makeCharacter(chat)
    DBState.db = {
        characters: [installedCharacter, ...extraCharacters],
        statics: { messages: 0 },
        botPresets: [],
        botPresetsId: 0,
        aiModel: 'test-model',
        maxContext: 8_192,
        maxResponse: 128,
        promptTemplate: [{ type: 'chat', rangeStart: 0, rangeEnd: 'end' }],
        promptSettings: {
            trimStartNewChat: true,
            sendName: false,
            sendChatAsSystem: false,
            postEndInnerFormat: '',
        },
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
        outputImageModal: false,
        rememberToolUsage: false,
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
    } as unknown as Database
    selectedCharID.set(0)
    const currentCharacter = DBState.db.characters[0] as character
    const residentChat = currentCharacter.chats[0]
    mocks.currentCharacter = currentCharacter
    mocks.session = new ActiveConversationSession({
        characterId: currentCharacter.chaId,
        conversationId: residentChat.id!,
        conversation: residentChat,
        storeRevision: 1,
    })
    return {
        chat: residentChat,
        currentCharacter,
        session: mocks.session as ActiveConversationSession,
    }
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

describe('sendChat generation session integration', () => {
    beforeEach(() => {
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        doingChat.set(false)
        mocks.session = null
        mocks.currentCharacter = null
        mocks.events.length = 0
        mocks.modelResponse = streamingResponse('answer')
        mocks.modelRequestCount = 0
        mocks.modelRequests.length = 0
        mocks.outputTrigger = null
        mocks.tokenizeResult = null
        mocks.inlay = null
        mocks.listeners.clear()
    })

    it('publishes a trigger clone through a fresh fallback and preserves final action order', async () => {
        const { session } = installDatabase()
        DBState.db.igpPrompt = 'append emotion'
        mocks.modelResponse = streamingResponse('answer')
        mocks.inlay = (data) => ({
            text: `${data}|inlay`,
            promise: Promise.resolve().then(() => {
                mocks.events.push('inlay-async')
                return `${data}|inlay-async`
            }),
        })
        mocks.listeners.add(async () => {
            mocks.events.push('output-listener')
        })

        await expect(sendChat()).resolves.toBe(true)

        expect(session.isActive).toBe(false)
        expect(mocks.session).toBeNull()
        const stored = DBState.db.characters[0].chats[0].message.at(-1)!
        expect(stored.data).toBe('answer|inlay-async|igp')
        expect(stored.generationInfo).toMatchObject({
            model: 'test-model',
            inputTokens: expect.any(Number),
            outputTokens: expect.any(Number),
        })
        expect(mocks.modelRequests[0].formated).toEqual([{
            role: 'user',
            content: 'hello',
            memo: 'user-message',
            attr: [],
            thoughts: [],
            removable: true,
        }])
        expect(mocks.events.filter((event) => [
            'output-script',
            'output-trigger',
            'invalidate-session',
            'inlay-sync',
            'inlay-async',
            'output-listener',
            'tokenize-result',
            'igp-request',
        ].includes(event))).toEqual([
            'output-script',
            'output-trigger',
            'invalidate-session',
            'inlay-sync',
            'inlay-async',
            'output-listener',
            'tokenize-result',
            'igp-request',
        ])
    })

    it('does not append after character navigation while the model request is pending', async () => {
        const second = makeCharacter(makeChat(), 'character-b')
        const { chat, session } = installDatabase(makeChat(), [second])
        const response = deferred<any>()
        mocks.modelResponse = response.promise

        const sending = sendChat()
        while (mocks.modelRequestCount === 0) {
            await Promise.resolve()
        }
        selectedCharID.set(1)
        response.resolve(streamingResponse('late answer'))

        await expect(sending).resolves.toBe(false)
        expect(chat.message).toEqual([expect.objectContaining({
            chatId: 'user-message',
            data: 'hello',
        })])
        expect(second.chats[0].message).toEqual([expect.objectContaining({
            chatId: 'user-message',
            data: 'hello',
        })])
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('does not append after the selected chat object is replaced while the model request is pending', async () => {
        const { chat, currentCharacter, session } = installDatabase()
        const response = deferred<any>()
        mocks.modelResponse = response.promise
        const replacement = makeChat([{
            role: 'user',
            data: 'replacement prompt',
            chatId: 'replacement-message',
        }])

        const sending = sendChat()
        while (mocks.modelRequestCount === 0) {
            await Promise.resolve()
        }
        currentCharacter.chats[0] = replacement
        response.resolve(streamingResponse('late answer'))

        await expect(sending).resolves.toBe(false)
        expect(chat.message).toEqual([expect.objectContaining({
            chatId: 'user-message',
            data: 'hello',
        })])
        expect(replacement.message).toEqual([expect.objectContaining({
            chatId: 'replacement-message',
            data: 'replacement prompt',
        })])
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('does not append after the active session version changes while the model request is pending', async () => {
        const { chat, session } = installDatabase()
        const response = deferred<any>()
        mocks.modelResponse = response.promise

        const sending = sendChat()
        while (mocks.modelRequestCount === 0) {
            await Promise.resolve()
        }
        session.append({
            role: 'user',
            data: 'concurrent prompt',
            chatId: 'concurrent-message',
        })
        response.resolve(streamingResponse('late answer'))

        await expect(sending).resolves.toBe(false)
        expect(chat.message).toEqual([
            expect.objectContaining({ chatId: 'user-message', data: 'hello' }),
            expect.objectContaining({ chatId: 'concurrent-message', data: 'concurrent prompt' }),
        ])
        expect(session.pinCount('transaction')).toBe(0)
    })

    it.each(['append', 'edit', 'delete'] as const)(
        'fails closed when the same session performs an %s while the output trigger is pending',
        async (mutation) => {
            const { chat, session } = installDatabase()
            const entered = deferred<void>()
            const release = deferred<void>()
            mocks.outputTrigger = async (clone) => {
                entered.resolve()
                await release.promise
                return { chat: clone }
            }

            const sending = sendChat()
            const boundary = await Promise.race([
                entered.promise.then(() => 'entered'),
                sending.then((value) => `completed:${value}`),
            ])
            expect(boundary).toBe('entered')
            if (mutation === 'append') {
                session.append({ role: 'user', data: 'concurrent append', chatId: 'race' })
            } else if (mutation === 'edit') {
                session.edit(session.locate(0), {
                    role: 'user',
                    data: 'concurrent edit',
                    chatId: 'user-message',
                })
            } else {
                session.delete(session.locate(0))
            }
            release.resolve()

            await expect(sending).resolves.toBe(false)
            expect(DBState.db.characters[0].chats[0]).toBe(chat)
            if (mutation === 'append') {
                expect(chat.message.some((message) => message.chatId === 'race')).toBe(true)
            } else if (mutation === 'edit') {
                expect(chat.message[0].data).toBe('concurrent edit')
            } else {
                expect(chat.message.some((message) => message.chatId === 'user-message')).toBe(false)
            }
            expect(session.pinCount('transaction')).toBe(0)
        },
    )

    it('does not publish an old conversation after the user navigates during the output trigger', async () => {
        const second = makeCharacter(makeChat(), 'character-b')
        const { chat, session } = installDatabase(makeChat(), [second])
        const entered = deferred<void>()
        const release = deferred<void>()
        mocks.outputTrigger = async (clone) => {
            entered.resolve()
            await release.promise
            return { chat: clone }
        }

        const sending = sendChat()
        const boundary = await Promise.race([
            entered.promise.then(() => 'entered'),
            sending.then((value) => `completed:${value}`),
        ])
        expect(boundary, mocks.events.join(',')).toBe('entered')
        selectedCharID.set(1)
        release.resolve()

        await expect(sending).resolves.toBe(false)
        expect(DBState.db.characters[0].chats[0]).toBe(chat)
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('does not postprocess an unrelated last message when the output trigger removes its target', async () => {
        installDatabase()
        DBState.db.igpPrompt = 'append emotion'
        let triggerCalls = 0
        mocks.outputTrigger = (clone) => {
            triggerCalls += 1
            if (triggerCalls > 1) return { chat: clone }
            clone.message = [
                clone.message[0],
                { role: 'char', data: 'unrelated', chatId: 'unrelated' },
            ]
            return { chat: clone, sendAIprompt: true }
        }

        await expect(sendChat()).resolves.toBe(false)

        const stored = DBState.db.characters[0].chats[0].message.at(-1)!
        expect(stored).toMatchObject({ chatId: 'unrelated', data: 'unrelated' })
        expect(mocks.modelRequestCount).toBe(1)
        expect(mocks.events).not.toContain('igp-request')
    })

    it('rechecks ownership after output listeners before auto-continue', async () => {
        const { currentCharacter } = installDatabase()
        DBState.db.autoContinueMinTokens = 2
        const entered = deferred<void>()
        const release = deferred<void>()
        mocks.listeners.add(async () => {
            entered.resolve()
            await release.promise
        })

        const sending = sendChat()
        await entered.promise
        currentCharacter.chats[0] = makeChat([{
            role: 'char',
            data: 'replacement',
            chatId: 'replacement',
        }])
        release.resolve()

        await expect(sending).resolves.toBe(false)
        expect(mocks.modelRequestCount).toBe(1)
        expect(currentCharacter.chats[0].message).toEqual([expect.objectContaining({
            chatId: 'replacement',
            data: 'replacement',
        })])
    })

    it('rechecks ownership after result tokenization', async () => {
        const { currentCharacter } = installDatabase()
        const tokenized = deferred<number>()
        mocks.tokenizeResult = tokenized.promise

        const sending = sendChat()
        while (!mocks.events.includes('tokenize-result')) {
            await Promise.resolve()
        }
        currentCharacter.chats[0] = makeChat([{
            role: 'char',
            data: 'replacement',
            chatId: 'replacement',
        }])
        tokenized.resolve(1)

        await expect(sending).resolves.toBe(false)
        expect(currentCharacter.chats[0].message[0].data).toBe('replacement')
    })

    it('continues the captured message without appending and disposes its operation pin', async () => {
        const chat = makeChat([{
            role: 'char',
            data: 'existing',
            chatId: 'existing-output',
        }])
        const { session } = installDatabase(chat)
        mocks.modelResponse = { type: 'success', result: ' plus' }

        await expect(sendChat(-1, { continue: true })).resolves.toBe(true)

        expect(DBState.db.characters[0].chats[0].message).toHaveLength(1)
        expect(DBState.db.characters[0].chats[0].message[0].data).toBe('existing plus')
        expect(session.pinCount('transaction')).toBe(0)
    })
})
