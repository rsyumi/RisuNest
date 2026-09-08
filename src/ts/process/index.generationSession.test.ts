import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import { createConversationOperationContext, type ConversationCommitObserver } from './conversationOperationContext'

const mocks = vi.hoisted(() => ({
    session: null as any,
    currentCharacter: null as any,
    events: [] as string[],
    modelResponse: null as any,
    modelRequestCount: 0,
    modelRequests: [] as any[],
    presetActivationCount: 0,
    presetActivation: null as null | (() => Promise<void> | void),
    outputTrigger: null as null | ((chat: any) => Promise<any> | any),
    tokenizeResult: null as Promise<number> | null,
    inlay: null as null | ((data: string) => { text: string, promise?: Promise<string> }),
    listeners: new Set<(event: any) => Promise<void> | void>(),
    selectedTarget: null as any,
    acquireCompleteConversation: vi.fn(),
    activeCompleteLeases: 0,
    completeLeaseReleases: 0,
    lastCharPunctuation: true,
    acknowledge: vi.fn(async () => undefined),
    processScriptFull: vi.fn(async (
        _char: unknown,
        data: string,
        mode: string,
        _messageIndex?: number,
        _conditions?: Record<string, unknown>,
        _processing?: {
            cache?: 'normal' | 'bypass'
            signal?: AbortSignal
            regexWorker?: boolean
            onConversationCommit?: ConversationCommitObserver
        },
    ) => {
        if (mode === 'editoutput') mocks.events.push('output-script')
        return { data, emoChanged: false }
    }),
    sayTTS: vi.fn(async (_char: unknown, _data: string) => undefined),
    addRerolls: vi.fn((_generationId: string, _values: string[]) => undefined),
    trimUntilPunctuation: vi.fn((value: string) => value),
}))

vi.mock('../tokenizer', async () => (await import('./tests/sendChatTestHarness')).tokenizerModule({
    tokenize: vi.fn(async () => {
        mocks.events.push('tokenize-result')
        return mocks.tokenizeResult ? await mocks.tokenizeResult : 1
    }),
}))
vi.mock('../../lang', async () => (await import('./tests/sendChatTestHarness')).langModule())
vi.mock('../alert', async () => (await import('./tests/sendChatTestHarness')).alertModule())
vi.mock('../parser/chatML', async () => (await import('./tests/sendChatTestHarness')).chatMLModule())
vi.mock('../parser/parser.svelte', async () => (await import('./tests/sendChatTestHarness')).parserModule())
vi.mock('./lorebook.svelte', async () => (await import('./tests/sendChatTestHarness')).lorebookModule())
vi.mock('../util', async () => (await import('./tests/sendChatTestHarness')).utilModule({
    findCharacterbyId: () => mocks.currentCharacter,
    isLastCharPunctuation: () => mocks.lastCharPunctuation,
    trimUntilPunctuation: mocks.trimUntilPunctuation,
}))
vi.mock('./request/request', () => ({
    requestChatData: vi.fn(async (request: unknown, purpose: string) => {
        if (purpose === 'emotion') {
            mocks.events.push('igp-request')
            return '|igp'
        }
        mocks.modelRequestCount += 1
        mocks.modelRequests.push(request)
        return typeof mocks.modelResponse === 'function'
            ? mocks.modelResponse()
            : mocks.modelResponse
    }),
}))
vi.mock('./stableDiff', async () => (await import('./tests/sendChatTestHarness')).stableDiffModule())
vi.mock('./scripts', async () => (await import('./tests/sendChatTestHarness')).scriptsModule({
    processScriptFull: mocks.processScriptFull,
}))
vi.mock('./templates/templates', async () => (await import('./tests/sendChatTestHarness')).templatesModule())
vi.mock('./exampleMessages', async () => (await import('./tests/sendChatTestHarness')).exampleMessagesModule())
vi.mock('./tts', async () => (await import('./tests/sendChatTestHarness')).ttsModule({
    sayTTS: mocks.sayTTS,
}))
vi.mock('./memory/supaMemory', async () => (await import('./tests/sendChatTestHarness')).supaMemoryModule())
vi.mock('./group', async () => (await import('./tests/sendChatTestHarness')).groupModule())
vi.mock('./triggers', () => ({
    runTrigger: vi.fn(async (_char: unknown, mode: string, arg: { chat: any }) => {
        if (mode === 'start') return null
        mocks.events.push('output-trigger')
        const clone = JSON.parse(JSON.stringify(arg.chat))
        return mocks.outputTrigger ? await mocks.outputTrigger(clone) : { chat: clone }
    }),
}))
vi.mock('./memory/hypamemory', async () => (await import('./tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('./embedding/addinfo', async () => (await import('./tests/sendChatTestHarness')).addinfoModule())
vi.mock('./files/inlays', async () => (await import('./tests/sendChatTestHarness')).inlaysModule())
vi.mock('./models/modelString', async () => (await import('./tests/sendChatTestHarness')).modelStringModule())
vi.mock('../sync/multiuser', async () => (await import('./tests/sendChatTestHarness')).multiuserModule())
vi.mock('./inlayScreen', () => ({
    runInlayScreen: (_char: unknown, data: string) => {
        mocks.events.push('inlay-sync')
        return mocks.inlay ? mocks.inlay(data) : { text: data }
    },
}))
vi.mock('./prereroll', async () => (await import('./tests/sendChatTestHarness')).prerollModule({
    addRerolls: mocks.addRerolls,
}))
vi.mock('./transformers', async () => (await import('./tests/sendChatTestHarness')).transformersModule())
vi.mock('./memory/hanuraiMemory', async () => (await import('./tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('./memory/hypav2', async () => (await import('./tests/sendChatTestHarness')).hypav2Module())
vi.mock('./memory/hypav3', async () => (await import('./tests/sendChatTestHarness')).hypav3Module())
vi.mock('./scriptings', async () => (await import('./tests/sendChatTestHarness')).scriptingsModule())
vi.mock('../model/modellist', async () => (await import('./tests/sendChatTestHarness')).modellistModule())
vi.mock('./modules', async () => (await import('./tests/sendChatTestHarness')).modulesModule())
vi.mock('../globalApi.svelte', async () => (await import('./tests/sendChatTestHarness')).globalApiModule())
vi.mock('../plugins/plugins.svelte', async () => (await import('./tests/sendChatTestHarness')).pluginsModule(mocks.listeners))
vi.mock('./presetChain', () => ({
    activatePresetChainForRequest: vi.fn(async () => {
        mocks.presetActivationCount += 1
        await mocks.presetActivation?.()
    }),
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    acknowledgeGenerationCompletion: mocks.acknowledge,
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    getActiveConversationSession: () => mocks.session,
    invalidateActiveConversationSession: () => {
        mocks.events.push('invalidate-session')
        mocks.session?.invalidate()
        mocks.session = null
    },
}))

import type { character, Chat, Database, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { SelectedConversationPromotionStaleError } from '../storage/activeWorkingSet.svelte'
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

function installDatabase(
    chat = makeChat(),
    extraCharacters: character[] = [],
    onMutation?: ConstructorParameters<typeof ActiveConversationSession>[0]['onMutation'],
) {
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
        onMutation,
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

function streamingSnapshots(...values: string[]) {
    return {
        type: 'streaming',
        result: new ReadableStream<Record<string, string>>({
            start(controller) {
                for (const value of values) controller.enqueue({ response: value })
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
        mocks.presetActivationCount = 0
        mocks.presetActivation = null
        mocks.outputTrigger = null
        mocks.tokenizeResult = null
        mocks.inlay = null
        mocks.listeners.clear()
        mocks.selectedTarget = null
        mocks.activeCompleteLeases = 0
        mocks.completeLeaseReleases = 0
        mocks.lastCharPunctuation = true
        mocks.acknowledge.mockReset().mockResolvedValue(undefined)
        mocks.processScriptFull.mockReset().mockImplementation(async (
            _char: unknown,
            data: string,
            mode: string,
            _messageIndex?: number,
            _conditions?: Record<string, unknown>,
            _processing?: {
                cache?: 'normal' | 'bypass'
                signal?: AbortSignal
                regexWorker?: boolean
            },
        ) => {
            if (mode === 'editoutput') mocks.events.push('output-script')
            return { data, emoChanged: false }
        })
        mocks.sayTTS.mockReset().mockResolvedValue(undefined)
        mocks.addRerolls.mockReset()
        mocks.trimUntilPunctuation.mockReset().mockImplementation((value: string) => value)
        mocks.acquireCompleteConversation.mockReset()
        mocks.acquireCompleteConversation.mockImplementation(async (_reason, target) => {
            const session = mocks.session
            const pin = session.acquirePin('compatibility')
            mocks.activeCompleteLeases += 1
            let released = false
            return {
                session,
                target,
                release() {
                    if (released) return
                    released = true
                    mocks.completeLeaseReleases += 1
                    mocks.activeCompleteLeases -= 1
                    pin.release()
                },
            }
        })
    })

    it('promotes before generation reads history and holds one lease across awaited streaming work', async () => {
        const { session } = installDatabase()
        const target = { characterId: 'character-a', conversationId: 'chat-a' }
        const promotion = deferred<any>()
        const response = deferred<any>()
        mocks.selectedTarget = target
        mocks.acquireCompleteConversation.mockImplementationOnce(() => promotion.promise)
        mocks.modelResponse = response.promise

        const sending = sendChat()
        await Promise.resolve()

        expect(mocks.presetActivationCount).toBe(0)
        expect(DBState.db.statics.messages).toBe(0)

        const pin = session.acquirePin('compatibility')
        mocks.activeCompleteLeases = 1
        promotion.resolve({
            session,
            target,
            release() {
                mocks.completeLeaseReleases += 1
                mocks.activeCompleteLeases -= 1
                pin.release()
            },
        })
        while (mocks.modelRequestCount === 0) await Promise.resolve()

        expect(mocks.activeCompleteLeases).toBe(1)
        expect(session.pinCount('compatibility')).toBe(1)

        response.resolve(streamingResponse('answer'))
        await expect(sending).resolves.toBe(true)
        expect(mocks.activeCompleteLeases).toBe(0)
        expect(mocks.completeLeaseReleases).toBe(1)
        expect(session.pinCount('compatibility')).toBe(0)
    })

    it('reserves generation before async promotion so a concurrent public call cannot acquire', async () => {
        const { session } = installDatabase()
        const target = { characterId: 'character-a', conversationId: 'chat-a' }
        const promotion = deferred<any>()
        const secondReachedPromotion = new Error('second call reached promotion')
        mocks.selectedTarget = target
        mocks.acquireCompleteConversation
            .mockReturnValueOnce(promotion.promise)
            .mockRejectedValueOnce(secondReachedPromotion)

        const first = sendChat()
        while (mocks.acquireCompleteConversation.mock.calls.length === 0) await Promise.resolve()
        const secondOutcome = await sendChat().catch((error) => error)

        expect(get(doingChat)).toBe(true)

        const pin = session.acquirePin('compatibility')
        promotion.resolve({ session, target, release: () => pin.release() })
        await expect(first).resolves.toBe(true)

        expect(secondOutcome).toBe(false)
        expect(mocks.acquireCompleteConversation).toHaveBeenCalledTimes(1)
        expect(session.pinCount('compatibility')).toBe(0)
    })

    it('holds the outer lease while auto-continue recursively owns and releases its exact lease', async () => {
        const { session } = installDatabase()
        mocks.selectedTarget = { characterId: 'character-a', conversationId: 'chat-a' }
        DBState.db.autoContinueChat = true
        mocks.modelResponse = () => streamingResponse('answer')
        mocks.outputTrigger = () => null
        mocks.lastCharPunctuation = false
        mocks.listeners.add(() => {
            if (mocks.modelRequestCount === 2) mocks.lastCharPunctuation = true
        })
        let maximumLeases = 0
        mocks.acquireCompleteConversation.mockImplementation(async (_reason, target) => {
            const pin = session.acquirePin('compatibility')
            mocks.activeCompleteLeases += 1
            maximumLeases = Math.max(maximumLeases, mocks.activeCompleteLeases)
            let released = false
            return {
                session,
                target,
                release() {
                    if (released) return
                    released = true
                    mocks.completeLeaseReleases += 1
                    mocks.activeCompleteLeases -= 1
                    pin.release()
                },
            }
        })

        await expect(sendChat()).resolves.toBe(true)

        expect(mocks.modelRequestCount).toBe(2)
        expect(maximumLeases).toBe(2)
        expect(mocks.completeLeaseReleases).toBe(2)
        expect(mocks.activeCompleteLeases).toBe(0)
        expect(session.pinCount('compatibility')).toBe(0)
    })

    it('releases the complete generation lease exactly once when awaited provider work throws', async () => {
        const { session } = installDatabase()
        const failure = new Error('provider failed')
        mocks.selectedTarget = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.modelResponse = Promise.reject(failure)

        await expect(sendChat()).rejects.toBe(failure)

        expect(mocks.completeLeaseReleases).toBe(1)
        expect(mocks.activeCompleteLeases).toBe(0)
        expect(session.pinCount('compatibility')).toBe(0)
    })

    it('returns false without reading or mutating history when complete promotion is stale', async () => {
        const { chat, currentCharacter } = installDatabase()
        currentCharacter.lastInteraction = 123
        mocks.selectedTarget = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.acquireCompleteConversation.mockRejectedValueOnce(
            new SelectedConversationPromotionStaleError(),
        )

        await expect(sendChat()).resolves.toBe(false)

        expect(chat.message).toEqual([expect.objectContaining({ data: 'hello' })])
        expect(DBState.db.statics.messages).toBe(0)
        expect(currentCharacter.lastInteraction).toBe(123)
        expect(mocks.presetActivationCount).toBe(0)
    })

    it('records generation-start message ID assignment through the active session', async () => {
        const onMutation = vi.fn()
        const source = makeChat([
            { role: 'user', data: 'missing ID' },
            { role: 'char', data: 'kept empty ID', chatId: '' },
        ])
        installDatabase(source, [], onMutation)

        await expect(sendChat()).resolves.toBe(true)

        expect(onMutation).toHaveBeenCalled()
        expect(onMutation.mock.calls[0][0]).toMatchObject({
            commands: ['edit'],
            mutations: [{ start: 0, deleteCount: 1 }],
        })
    })

    it.each([
        {
            name: 'edits the existing character tail',
            messages: [{ role: 'char', data: 'partial', chatId: 'response-1' }] as Message[],
            command: 'edit',
            start: 0,
            deleteCount: 1,
            expectedData: 'partial\n```risuerror\nrequest failed\n```',
        },
        {
            name: 'appends after a user tail',
            messages: [{ role: 'user', data: 'prompt', chatId: 'user-1' }] as Message[],
            command: 'append',
            start: 1,
            deleteCount: 0,
            expectedData: '```risuerror\nrequest failed\n```',
        },
    ])('$name through the active session when an inlay error is produced', async ({
        messages,
        command,
        start,
        deleteCount,
        expectedData,
    }) => {
        const onMutation = vi.fn()
        const { chat, session } = installDatabase(makeChat(messages), [], onMutation)
        DBState.db.inlayErrorResponse = true
        mocks.modelResponse = { type: 'fail', result: 'request failed' }

        await expect(sendChat()).resolves.toBe(false)

        expect(chat.message.at(-1)?.data).toBe(expectedData)
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 1,
            commands: [command],
            mutations: [expect.objectContaining({ start, deleteCount })],
        }))
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('retains the direct error response fallback when no active session exists', async () => {
        const { chat } = installDatabase(makeChat([{
            role: 'user',
            data: 'prompt',
            chatId: 'user-1',
        }]))
        mocks.session = null
        DBState.db.inlayErrorResponse = true
        mocks.modelResponse = { type: 'fail', result: 'request failed' }

        await expect(sendChat()).resolves.toBe(false)

        expect(chat.message.at(-1)).toMatchObject({
            role: 'char',
            data: '```risuerror\nrequest failed\n```',
        })
    })

    it('fails closed before assigning IDs when the active session owns another chat', async () => {
        const source = makeChat([{ role: 'user', data: 'must remain unchanged' }])
        const { currentCharacter } = installDatabase(source)
        currentCharacter.lastInteraction = 123
        const otherChat = makeChat([{ role: 'user', data: 'other', chatId: 'other' }])
        otherChat.id = 'chat-b'
        mocks.session = new ActiveConversationSession({
            characterId: currentCharacter.chaId,
            conversationId: otherChat.id,
            conversation: otherChat,
            storeRevision: 1,
        })

        await expect(sendChat()).resolves.toBe(false)

        expect(source.message).toEqual([{ role: 'user', data: 'must remain unchanged' }])
        expect(mocks.presetActivationCount).toBe(0)
        expect(mocks.modelRequestCount).toBe(0)
        expect(DBState.db.statics.messages).toBe(0)
        expect(currentCharacter.lastInteraction).toBe(123)
    })

    it('fails closed when the matching session disappears during preset activation', async () => {
        const source = makeChat([{ role: 'user', data: 'must remain unchanged' }])
        const { currentCharacter } = installDatabase(source)
        currentCharacter.lastInteraction = 123
        mocks.presetActivation = () => {
            mocks.session?.invalidate()
            mocks.session = null
        }

        await expect(sendChat()).resolves.toBe(false)

        expect(source.message).toEqual([{ role: 'user', data: 'must remain unchanged' }])
        expect(mocks.presetActivationCount).toBe(1)
        expect(mocks.modelRequestCount).toBe(0)
        expect(DBState.db.statics.messages).toBe(0)
        expect(currentCharacter.lastInteraction).toBe(123)
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

    it.each(['success', 'streaming'] as const)(
        'applies a %s response after persisted plugin metadata and character publication',
        async (type) => {
            const { chat, currentCharacter, session } = installDatabase()
            const response = deferred<any>()
            mocks.modelResponse = response.promise
            const sending = sendChat()
            while (mocks.modelRequestCount === 0) await Promise.resolve()
            const next = JSON.parse(JSON.stringify(currentCharacter))
            next.name = 'Updated character metadata'
            next.chats[0].scriptstate = { $bridge: 'on' }
            next.chats[0].message[0].__translation = 'record'
            expect(session.adoptPersistedMetadata(next.chats[0], 2)).toBe(true)
            next.chats[0] = chat
            DBState.db.characters[0] = next
            response.resolve(
                type === 'success'
                    ? { type: 'success', result: 'answer' }
                    : streamingResponse('answer'),
            )
            await expect(sending).resolves.toBe(true)
            const completed = DBState.db.characters[0].chats[0]
            expect(completed.message.at(-1)?.data).toBe('answer')
            expect(completed.message[0]).toMatchObject({
                data: 'hello',
                __translation: 'record',
            })
            expect(completed.scriptstate).toEqual({ $bridge: 'on' })
            expect(session.pinCount('transaction')).toBe(0)
        },
    )

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

    it('preserves complete successful non-empty multiline response behavior', async () => {
        const { currentCharacter, session } = installDatabase()
        DBState.db.ttsAutoSpeech = true
        mocks.modelResponse = {
            type: 'multiline',
            result: [
                ['char', 'First choice'],
                ['char', 'Second choice'],
            ],
        }
        mocks.processScriptFull.mockImplementation(async (
            _char: unknown,
            data: string,
            mode: string,
        ) => {
            if (mode === 'editoutput') mocks.events.push(`output-script:${data}`)
            return { data: `${data}|script`, emoChanged: false }
        })
        mocks.inlay = (data) => ({ text: `${data}|inlay` })
        mocks.sayTTS.mockImplementation(async (_char, data: string) => {
            mocks.events.push(`tts:${data}`)
        })
        mocks.addRerolls.mockImplementation((_generationId, values: string[]) => {
            mocks.events.push(`rerolls:${values.join(',')}`)
        })
        mocks.listeners.add(async () => {
            mocks.events.push('output-listener')
        })

        await expect(sendChat()).resolves.toBe(true)

        const publishedChat = DBState.db.characters[0].chats[0]
        expect(publishedChat.message).toHaveLength(2)
        expect(publishedChat.message.at(-1)).toMatchObject({
            role: 'char',
            data: 'First choice|script|inlay',
            saying: currentCharacter.chaId,
            generationInfo: expect.objectContaining({ generationId: expect.any(String) }),
        })
        expect(mocks.addRerolls).toHaveBeenCalledWith(expect.any(String), [
            'First choice|script|inlay',
            'Second choice|script|inlay',
        ])
        expect(mocks.sayTTS.mock.calls.map((call) => call[1])).toEqual([
            'First choice|script|inlay',
            'Second choice|script|inlay',
        ])
        expect(mocks.events.filter((event) =>
            event.startsWith('output-script:')
            || event === 'inlay-sync'
            || event.startsWith('tts:')
            || event.startsWith('rerolls:')
            || event === 'output-trigger'
            || event === 'output-listener'
            || event === 'tokenize-result'
        )).toEqual([
            'output-script:First choice',
            'inlay-sync',
            'tts:First choice|script|inlay',
            'output-script:Second choice',
            'inlay-sync',
            'tts:Second choice|script|inlay',
            'rerolls:First choice|script|inlay,Second choice|script|inlay',
            'output-trigger',
            'output-listener',
            'tokenize-result',
        ])
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(session.pinCount('transaction')).toBe(0)
    })

    it.each([
        {
            mode: 'balanced' as const,
            expectedCommits: ['semantic:First snapshot', 'semantic:Latest snapshot'],
            expectedOutputPasses: [
                ['First snapshot', 'bypass'],
                ['Latest snapshot', 'bypass'],
            ],
        },
        {
            mode: 'strong' as const,
            expectedCommits: [
                'First snapshot',
                'Latest snapshot',
                'semantic:Latest snapshot',
            ],
            expectedOutputPasses: [['Latest snapshot', 'normal']],
        },
    ])('wires $mode streaming mode through the production preview and semantic sequence', async ({
        mode,
        expectedCommits,
        expectedOutputPasses,
    }) => {
        const { session } = installDatabase()
        const edit = vi.spyOn(session, 'edit')
        DBState.db.streamingDisplayOptimizationMode = mode
        mocks.modelResponse = streamingSnapshots('First snapshot', 'Latest snapshot')
        mocks.processScriptFull.mockImplementation(async (
            _char: unknown,
            data: string,
            _mode: string,
        ) => ({ data: `semantic:${data}`, emoChanged: false }))

        await expect(sendChat()).resolves.toBe(true)

        const outputPasses = mocks.processScriptFull.mock.calls
            .filter((call) => call[2] === 'editoutput')
            .map((call) => [call[1], call[5]?.cache])
        expect(outputPasses).toEqual(expectedOutputPasses)
        expect(edit.mock.calls.map((call) => call[1].data)).toEqual(expectedCommits)
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe(
            'semantic:Latest snapshot',
        )
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('fails closed when multiline ownership becomes stale during async inlay processing', async () => {
        const { chat, session } = installDatabase()
        const enteredInlay = deferred<void>()
        const finishInlay = deferred<string>()
        mocks.modelResponse = {
            type: 'multiline',
            result: [['char', 'Owned response']],
        }
        mocks.inlay = (data) => {
            enteredInlay.resolve()
            return {
                text: `${data}|sync-inlay`,
                promise: finishInlay.promise,
            }
        }

        const sending = sendChat()
        await enteredInlay.promise
        session.append({
            role: 'user',
            data: 'Concurrent mutation',
            chatId: 'concurrent-message',
        })
        finishInlay.resolve('Late inlay result')

        await expect(sending).resolves.toBe(false)
        expect(chat.message.map((message) => [message.chatId, message.data])).toEqual([
            ['user-message', 'hello'],
            [expect.any(String), 'Owned response|sync-inlay'],
            ['concurrent-message', 'Concurrent mutation'],
        ])
        expect(chat.message.some((message) => message.data === 'Late inlay result')).toBe(false)
        expect(mocks.addRerolls).not.toHaveBeenCalled()
        expect(mocks.sayTTS).not.toHaveBeenCalled()
        expect(mocks.events).not.toContain('output-trigger')
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(session.pinCount('transaction')).toBe(0)
    })

    it.each([
        {
            name: 'streaming',
            response: () => streamingSnapshots('First raw', 'Second raw'),
            expectedStored: 'script:trim:Second raw',
            expectedOutputInputs: ['trim:First raw', 'trim:Second raw'],
            expectedTrimInputs: ['First raw', 'Second raw'],
            expectedRerolls: ['Second raw'],
        },
        {
            name: 'multiline',
            response: () => ({
                type: 'multiline' as const,
                result: [
                    ['char', 'First raw'],
                    ['char', 'Second raw'],
                ] as ['char', string][],
            }),
            expectedStored: 'trim:script:First raw',
            expectedOutputInputs: ['First raw', 'Second raw'],
            expectedTrimInputs: ['script:First raw', 'script:Second raw'],
            expectedRerolls: ['trim:script:First raw', 'trim:script:Second raw'],
        },
    ])('preserves removeIncompleteResponse trimming for $name response application', async ({
        response,
        expectedStored,
        expectedOutputInputs,
        expectedTrimInputs,
        expectedRerolls,
    }) => {
        const { session } = installDatabase()
        DBState.db.removeIncompleteResponse = true
        mocks.trimUntilPunctuation.mockImplementation((value: string) => `trim:${value}`)
        mocks.processScriptFull.mockImplementation(async (
            _char: unknown,
            data: string,
        ) => ({ data: `script:${data}`, emoChanged: false }))
        mocks.modelResponse = response()

        await expect(sendChat()).resolves.toBe(true)

        expect(mocks.processScriptFull.mock.calls
            .filter((call) => call[2] === 'editoutput')
            .map((call) => call[1])).toEqual(expectedOutputInputs)
        expect(mocks.trimUntilPunctuation.mock.calls.map((call) => call[0])).toEqual(
            expectedTrimInputs,
        )
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe(expectedStored)
        expect(mocks.addRerolls).toHaveBeenCalledWith(expect.any(String), expectedRerolls)
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('keeps an empty multiline response bound to the existing session tail', async () => {
        const { chat, session } = installDatabase()
        mocks.modelResponse = { type: 'multiline', result: [] }
        mocks.outputTrigger = () => ({ chat })

        await expect(sendChat()).resolves.toBe(true)

        expect(chat.message).toEqual([expect.objectContaining({
            data: 'hello',
            chatId: 'user-message',
        })])
        expect(session.pinCount('transaction')).toBe(0)
    })

    it.each([
        { continuing: false, rejects: false },
        { continuing: true, rejects: false },
        { continuing: false, rejects: true },
        { continuing: true, rejects: true },
    ])(
        'does not apply non-streamed output after cancellation (continue=$continuing, rejects=$rejects)',
        async ({ continuing, rejects }) => {
            const initialChat = continuing
                ? makeChat([
                      {
                          role: 'char',
                          data: 'existing',
                          chatId: 'existing-output',
                      },
                  ])
                : makeChat()
            const { chat, session } = installDatabase(initialChat)
            DBState.db.ttsAutoSpeech = true
            const controller = new AbortController()
            const enteredOutput = deferred<void>()
            const finishOutput = deferred<void>()
            mocks.modelResponse = { type: 'success', result: ' late' }
            mocks.processScriptFull.mockImplementation(async (_char, data, mode) => {
                if (mode === 'editoutput') {
                    enteredOutput.resolve()
                    await finishOutput.promise
                    if (rejects) throw controller.signal.reason
                }
                return { data, emoChanged: false }
            })
            const sending = sendChat(-1, { continue: continuing, signal: controller.signal })
            await enteredOutput.promise
            expect(
                mocks.processScriptFull.mock.calls.find((call) => call[2] === 'editoutput')?.[5]
                    ?.signal,
            ).toBe(controller.signal)
            controller.abort()
            finishOutput.resolve()

            await expect(sending).resolves.toBe(false)
            expect(chat.message.map((message) => message.data)).toEqual([
                continuing ? 'existing' : 'hello',
            ])
            expect(mocks.events).not.toContain('output-trigger')
            expect(mocks.sayTTS).not.toHaveBeenCalled()
            expect(session.pinCount('transaction')).toBe(0)
        },
    )

    it.each(['streaming', 'success'] as const)(
        'retains committed output when cancelled during %s inlay work',
        async (responseType) => {
            const { session } = installDatabase()
            DBState.db.ttsAutoSpeech = true
            const controller = new AbortController()
            const enteredInlay = deferred<void>()
            const finishInlay = deferred<string>()
            mocks.modelResponse =
                responseType === 'streaming'
                    ? streamingResponse('answer')
                    : { type: 'success', result: 'answer' }
            mocks.outputTrigger = (currentChat) => ({ chat: currentChat })
            mocks.inlay = (data) => {
                enteredInlay.resolve()
                return { text: data, promise: finishInlay.promise }
            }
            const sending = sendChat(-1, { signal: controller.signal })
            await enteredInlay.promise
            controller.abort()
            finishInlay.resolve('late inlay')

            await expect(sending).resolves.toBe(false)
            expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('answer')
            expect(mocks.sayTTS).not.toHaveBeenCalled()
            expect(session.pinCount('transaction')).toBe(0)
        },
    )

    it.each(['streaming', 'success'] as const)(
        'discards a pending %s output trigger result after cancellation',
        async (responseType) => {
            const { session } = installDatabase()
            const controller = new AbortController()
            const enteredTrigger = deferred<void>()
            const finishTrigger = deferred<void>()
            mocks.modelResponse =
                responseType === 'streaming'
                    ? streamingResponse('answer')
                    : { type: 'success', result: 'answer' }
            mocks.outputTrigger = async (chat) => {
                enteredTrigger.resolve()
                await finishTrigger.promise
                chat.message.at(-1).data = 'late trigger'
                return { chat }
            }
            const listener = vi.fn()
            mocks.listeners.add(listener)
            const sending = sendChat(-1, { signal: controller.signal })
            await enteredTrigger.promise
            controller.abort()
            finishTrigger.resolve()

            await expect(sending).resolves.toBe(false)
            expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('answer')
            expect(listener).not.toHaveBeenCalled()
            expect(session.pinCount('transaction')).toBe(0)
        },
    )

    it('does not start another output listener after cancellation during a pending listener', async () => {
        const { session } = installDatabase()
        const controller = new AbortController()
        const enteredListener = deferred<void>()
        const finishListener = deferred<void>()
        mocks.listeners.add(async () => {
            enteredListener.resolve()
            await finishListener.promise
        })
        const nextListener = vi.fn()
        mocks.listeners.add(nextListener)
        const sending = sendChat(-1, { signal: controller.signal })
        await enteredListener.promise
        controller.abort()
        finishListener.resolve()

        await expect(sending).resolves.toBe(false)
        expect(nextListener).not.toHaveBeenCalled()
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('answer')
        expect(session.pinCount('transaction')).toBe(0)
    })

    it('passes the moved output index to later streaming edit hooks', async () => {
        const { chat, session } = installDatabase()
        mocks.modelResponse = streamingSnapshots('First', 'Second')
        const indexes: number[] = []
        mocks.processScriptFull.mockImplementation(
            async (_char, data, mode, index, _conditions, processing) => {
                if (mode === 'editoutput') {
                    indexes.push(index!)
                    if (indexes.length === 1) {
                        const operation = createConversationOperationContext(
                            session,
                            chat,
                            processing?.onConversationCommit,
                        )
                        operation.chat.message.unshift({
                            role: 'user',
                            data: 'inserted',
                            chatId: 'inserted',
                        })
                        operation.commit(session)
                    }
                }
                return { data, emoChanged: false }
            },
        )

        await expect(sendChat()).resolves.toBe(true)
        expect(indexes).toEqual([1, 2])
        expect(chat.message.map((message) => message.data)).toEqual(['inserted', 'hello', 'Second'])
        expect(session.activePinReasons).toEqual([])
    })

    it('continues the captured message without appending and disposes its operation pin', async () => {
        const chat = makeChat([{
            role: 'char',
            data: 'existing',
            chatId: 'existing-output',
        }])
        const { session } = installDatabase(chat)
        Object.assign(chat.message[0], { __translation: 'existing-record' })
        mocks.modelResponse = { type: 'success', result: ' plus' }

        await expect(sendChat(-1, { continue: true })).resolves.toBe(true)

        expect(DBState.db.characters[0].chats[0].message).toHaveLength(1)
        expect(DBState.db.characters[0].chats[0].message[0].data).toBe('existing plus')
        expect(DBState.db.characters[0].chats[0].message[0]).toMatchObject({
            __translation: 'existing-record',
        })
        expect(session.pinCount('transaction')).toBe(0)
    })
})
