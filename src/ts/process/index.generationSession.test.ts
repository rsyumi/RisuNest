import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import { createConversationOperationContext, type ConversationCommitObserver } from './conversationOperationContext'

const mocks = vi.hoisted(() => ({
    iosDependencies: undefined as any,
    ios: false,
    diagnosticEvents: [] as Record<string, unknown>[],
    diagnosticMode: 'record' as 'record' | 'throw' | 'reject' | 'noop',
    session: null as any,
    currentCharacter: null as any,
    events: [] as string[],
    modelResponse: null as any,
    modelRequestCount: 0,
    realTriggers: false,
    authorityEpoch: 0,
    diagnosticAuthorityError: null as Error | null,
    completionRequest: null as null | (() => Promise<string>),
    modelRequests: [] as any[],
    presetActivationCount: 0,
    presetActivation: null as null | (() => Promise<void> | void),
    outputTrigger: null as null | ((chat: any) => Promise<any> | any),
    outputTriggerReturnsNull: false,
    tokenizeResult: null as Promise<number> | null,
    inlay: null as null | ((data: string) => { text: string, promise?: Promise<string> }),
    inlayAsset: null as any,
    caption: vi.fn(async (_image: string | Uint8Array) => [{ generated_text: 'synthetic caption' }]),
    moduleTriggers: [] as any[],
    moduleRegex: [] as any[],
    editHooks: new Set<unknown>(),
    listeners: new Set<(event: any) => Promise<void> | void>(),
    resolvePosition: vi.fn(async (_characterId: string, _conversationId: string) => ({
        characterIndex: 0,
        chatIndex: 0,
    })),
    selectedTarget: null as any,
    selectedAuthority: null as any,
    windowedController: null as any,
    persistentStore: null as any,
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
    trimUntilPunctuation: vi.fn((value: string) => value),
}))

vi.mock('../generationDiagnostics', async (importOriginal) => {
    const actual = await importOriginal<typeof import('../generationDiagnostics')>()
    return (await import('./tests/sendChatTestHarness')).generationDiagnosticsModule(actual, (_command, args) => {
        const event = args.event as Record<string, unknown>
        if (event.kind === 'document-start') return Promise.resolve(1)
        if (mocks.diagnosticMode === 'throw') throw new Error('native diagnostic unavailable')
        if (mocks.diagnosticMode === 'reject') return Promise.reject(new Error('native diagnostic unavailable'))
        if (mocks.diagnosticMode === 'record') mocks.diagnosticEvents.push(event)
        return Promise.resolve(undefined)
    })
})

vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => mocks.ios ? { runtime: 'tauri', os: 'ios' } : { runtime: 'web', os: 'windows' }))
vi.mock('../iosNative', async (importOriginal) => {
    const original = await importOriginal<typeof import('../iosNative')>()
    return { ...original,
        beginIOSGeneration: (signal?: AbortSignal) => original.beginIOSGeneration(signal, mocks.iosDependencies),
        notifyIOSGenerationComplete: vi.fn(),
    }
})
vi.mock('../tokenizer', async () => (await import('./tests/sendChatTestHarness')).tokenizerModule({
    tokenize: vi.fn(async () => {
        mocks.events.push('tokenize-result')
        return mocks.tokenizeResult ? await mocks.tokenizeResult : 1
    }),
}))
vi.mock('../alert', async () => (await import('./tests/sendChatTestHarness')).alertModule())
vi.mock('../parser/chatML', async () => (await import('./tests/sendChatTestHarness')).chatMLModule())
vi.mock('../parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('./lorebook.svelte', async () => (await import('./tests/sendChatTestHarness')).lorebookModule())
vi.mock('../util', async () => (await import('./tests/sendChatTestHarness')).utilModule({
    findCharacterbyId: () => mocks.currentCharacter,
    isLastCharPunctuation: () => mocks.lastCharPunctuation,
    trimUntilPunctuation: mocks.trimUntilPunctuation,
}))
vi.mock('./request/request', async () => (await import('./tests/sendChatTestHarness')).requestModule(vi.fn(async (request: unknown, purpose: string) => {
    if (purpose === 'emotion') {
        mocks.events.push('igp-request')
        return mocks.completionRequest ? await mocks.completionRequest() : '|igp'
    }
    mocks.modelRequestCount += 1
    mocks.modelRequests.push(request)
    return typeof mocks.modelResponse === 'function'
        ? mocks.modelResponse()
        : mocks.modelResponse
})))
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
vi.mock('./command', async () => (await import('./tests/sendChatTestHarness')).commandModule())
vi.mock('./infunctions', () => ({ calcString: vi.fn(() => 0) }))
vi.mock('./triggers', async (importOriginal) => {
    const original = await importOriginal<typeof import('./triggers')>()
    return { runTrigger: vi.fn(async (char: any, mode: string, arg: { chat: any }) => {
        if (mocks.realTriggers) return original.runTrigger(char, mode as any, arg as any)
        if (mode === 'start') return null
        mocks.events.push('output-trigger')
        const clone = JSON.parse(JSON.stringify(arg.chat))
        if (mocks.outputTriggerReturnsNull) return null
        return mocks.outputTrigger ? await mocks.outputTrigger(clone) : { chat: clone }
    }) }
})
vi.mock('./memory/hypamemory', async () => (await import('./tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('./embedding/addinfo', async () => (await import('./tests/sendChatTestHarness')).addinfoModule())
vi.mock('./files/inlays', async () => (await import('./tests/sendChatTestHarness')).inlaysModule({
    getInlayAsset: vi.fn(async () => mocks.inlayAsset),
}))
vi.mock('./files/inlayImageEncoding', async (importOriginal) => ({
    ...await importOriginal<typeof import('./files/inlayImageEncoding')>(),
    decodeInlayImageBitmap: async () => ({ width: 4000, height: 3000, close() {} } as ImageBitmap),
}))
vi.mock('./models/modelString', async () => (await import('./tests/sendChatTestHarness')).modelStringModule())
vi.mock('./inlayScreen', async () => (await import('./tests/sendChatTestHarness')).inlayScreenModule({
    runInlayScreen: (_char: unknown, data: string) => {
        mocks.events.push('inlay-sync')
        return mocks.inlay ? mocks.inlay(data) : { text: data }
    },
}))
vi.mock('./transformers', async () => (await import('./tests/sendChatTestHarness')).transformersModule({ runImageEmbedding: mocks.caption }))
vi.mock('./memory/hanuraiMemory', async () => (await import('./tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('./memory/hypav2', async () => (await import('./tests/sendChatTestHarness')).hypav2Module())
vi.mock('./memory/hypav3', async () => (await import('./tests/sendChatTestHarness')).hypav3Module({
    getCurrentHypaV3Preset: () => ({
        settings: {
            preserveOrphanedMemory: false,
            useExperimentalImpl: false,
            recentMemoryRatio: 0,
            similarMemoryRatio: 0,
        },
    }),
    hypaMemoryV3: vi.fn(async (chats: any[], currentTokens: number, _max: number, room: any) => ({
        chats,
        currentTokens,
        memory: room.hypaV3Data,
    })),
}))
vi.mock('./scriptings', async () => (await import('./tests/sendChatTestHarness')).scriptingsModule())
vi.mock('../model/modellist', async () => (await import('./tests/sendChatTestHarness')).modellistModule())
vi.mock('./modules', async () => (await import('./tests/sendChatTestHarness')).modulesModule({
    getModuleTriggers: () => mocks.moduleTriggers,
    getModuleRegexScripts: () => mocks.moduleRegex,
}))
vi.mock('../globalApi.svelte', async () => (await import('./tests/sendChatTestHarness')).globalApiModule())
vi.mock('../plugins/plugins.svelte', async () => (await import('./tests/sendChatTestHarness')).pluginsModule({ chatOutput: mocks.listeners, editprocess: mocks.editHooks }))
vi.mock('../plugins/pluginDatabaseAccess', async (importOriginal) => (await import('./tests/sendChatTestHarness')).pluginDatabaseAccessModule(importOriginal))
vi.mock('../plugins/pinnedConversationPosition', async () => (await import('./tests/sendChatTestHarness')).pinnedConversationPositionModule(mocks.resolvePosition))
vi.mock('./presetChain', () => ({
    activatePresetChainForRequest: vi.fn(async () => {
        mocks.presetActivationCount += 1
        await mocks.presetActivation?.()
    }),
}))
vi.mock('../storage/persistentDataRuntime.svelte', async () => (await import('./tests/sendChatTestHarness')).persistentRuntimeModule({
    assertPersistentMutationAllowed: (epoch = mocks.authorityEpoch) => {
        if (mocks.diagnosticAuthorityError) {
            const error = mocks.diagnosticAuthorityError
            mocks.diagnosticAuthorityError = null
            throw error
        }
        if (epoch !== mocks.authorityEpoch) throw new PersistentMutationFencedError()
    },
    getPersistentStorageAuthorityEpoch: () => mocks.authorityEpoch,
    acknowledgeGenerationCompletion: mocks.acknowledge,
    pendingConversationMutationDiagnostics: () => ({ pendingEvents: 2, unacknowledgedMs: 1_500 }),
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    captureSelectedConversationAuthority: () => mocks.selectedAuthority,
    captureWindowedConversationMutationController: (...args: any[]) =>
        typeof mocks.windowedController === 'function'
            ? mocks.windowedController(...args)
            : mocks.windowedController,
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    getActiveConversationSession: () => mocks.session,
    peekActiveConversationSession: () => mocks.session,
    invalidateActiveConversationSession: () => {
        mocks.events.push('invalidate-session')
        mocks.session?.invalidate()
        mocks.session = null
    },
}))
vi.mock('../storage/persistentDataStoreFactory', async () => (await import('./tests/sendChatTestHarness')).persistentDataStoreFactoryModule(() => mocks.persistentStore))

import type { character, Chat, Database, Message } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { SelectedConversationPromotionStaleError } from '../storage/activeWorkingSet.svelte'
import { createLeaseStub } from '../storage/persistentDataStore.testSupport'
import { PersistentMutationFencedError } from '../storage/saveCoordinator'
import { DBState, selectedCharID } from '../stores.svelte'
import { doingChat, sendChat } from './index.svelte'
import { isGenerationReserved } from './generationState'
import type { triggerscript } from './triggers'
import { isGenerationRequestPhaseOpen, trackConversationPatch } from './generationRequestPhase'
import { beginGenerationEndRun, subscribeGenerationEnd, type GenerationEndRecord, type GenerationEndStatus } from './generationEnd'

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
        mocks.iosDependencies = undefined
        mocks.ios = false
        mocks.diagnosticEvents.length = 0
        mocks.diagnosticMode = 'record'
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        doingChat.set(false)
        mocks.session = null
        mocks.currentCharacter = null
        mocks.events.length = 0
        mocks.modelResponse = streamingResponse('answer')
        mocks.modelRequestCount = 0
        mocks.realTriggers = false
        mocks.authorityEpoch = 0
        mocks.diagnosticAuthorityError = null
        mocks.completionRequest = null
        mocks.modelRequests.length = 0
        mocks.presetActivationCount = 0
        mocks.presetActivation = null
        mocks.outputTrigger = null
        mocks.outputTriggerReturnsNull = false
        mocks.tokenizeResult = null
        mocks.inlay = null
        mocks.inlayAsset = null
        mocks.caption.mockClear()
        mocks.listeners.clear()
        mocks.resolvePosition.mockClear()
        mocks.editHooks.clear()
        mocks.moduleTriggers = []
        mocks.moduleRegex = []
        mocks.selectedTarget = null
        mocks.selectedAuthority = null
        mocks.windowedController = null
        mocks.persistentStore = null
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
        mocks.trimUntilPunctuation.mockReset().mockImplementation((value: string) => value)
        mocks.acquireCompleteConversation.mockReset()
        mocks.acquireCompleteConversation.mockImplementation(async (_reason, target) => {
            const session = mocks.session
            const pin = session.acquirePin('compatibility')
            mocks.selectedAuthority = null
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

    it.each(['buffered', 'streaming', 'multiline', 'continue', 'reroll'] as const)(
        'records bounded content-free generation phases for %s', async (kind) => {
            const canary = 'PRIVATE_DIAGNOSTIC_CONTENT_CANARY'
            const { chat, session } = installDatabase(makeChat([{ role: 'char', data: canary, chatId: 'PRIVATE_MESSAGE_ID' }]))
            mocks.outputTriggerReturnsNull = true
            if (kind === 'reroll') chat.rerollRecovery = { original: [] } as Chat['rerollRecovery']
            DBState.db.streamingDisplayOptimizationMode = 'balanced'
            DBState.db.ttsAutoSpeech = true
            mocks.modelResponse = kind === 'streaming'
                ? streamingSnapshots(...Array.from({ length: 20 }, (_, index) => `${canary}-${index}`))
                : kind === 'multiline'
                    ? { type: 'multiline', result: [['char', canary], ['char', canary + '-alternate']] }
                    : { type: 'success', result: canary }

            await expect(sendChat({ continue: kind === 'continue' })).resolves.toBe(true)

            const admission = mocks.diagnosticEvents.filter(event => event.kind === 'admission')
            expect(admission[0]).toMatchObject({ intent: kind === 'continue' || kind === 'reroll' ? kind : 'normal', route: 'unknown', displayMode: 'normal' })
            expect(admission.at(-1)).toMatchObject({ route: 'complete', admittedMessages: 1, totalMessages: 1 })
            const exits = mocks.diagnosticEvents.filter(event => event.kind === 'phase' && event.edge === 'exit')
            const outputPhases = kind === 'multiline' ? ['output-application'] : ['semantic-output', 'inlay', 'tts']
            for (const phase of ['send-admitted', 'history-materialization', 'prompt-preparation', 'awaiting-provider', 'host-response', ...outputPhases, 'current-chat-parser', 'output-trigger', 'output-listener', 'completion-processing', 'final-acknowledgement', 'settled']) {
                expect(exits.filter(event => event.phase === phase), phase).toEqual([
                    expect.objectContaining({ phase, outcome: 'ok' }),
                ])
            }
            const persistence = mocks.diagnosticEvents.find(event => event.kind === 'persistence')
            expect(persistence).toMatchObject({ boundary: 'final', savePath: 'complete', acknowledgedRevision: 1, pendingEvents: 2, unacknowledgedMs: 1_500 })
            // The mocked acknowledgement leaves the output unsaved, so the session keeps its dirty records.
            expect(persistence?.dirtyRecords).toBe(session.pinCount('dirty'))
            expect(persistence?.dirtyRecords).toBeGreaterThan(0)
            expect(mocks.diagnosticEvents.length).toBeLessThan(40)
            const encoded = JSON.stringify(mocks.diagnosticEvents)
            for (const secret of [canary, 'PRIVATE_MESSAGE_ID', 'character-a', 'chat-a', 'test-model']) expect(encoded).not.toContain(secret)
            expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toContain(canary)
        },
    )

    it('keeps the multiline aggregate phase active while a later semantic pass is pending', async () => {
        installDatabase()
        DBState.db.ttsAutoSpeech = true
        mocks.modelResponse = { type: 'multiline', result: [['char', 'first'], ['char', 'second']] }
        const entered = deferred<void>()
        const finish = deferred<void>()
        mocks.processScriptFull.mockImplementation(async (_char, data, mode) => {
            if (mode === 'editoutput' && data === 'second') {
                entered.resolve()
                await finish.promise
            }
            return { data, emoChanged: false }
        })
        const sending = sendChat()
        await entered.promise
        // The first applied candidate is saved inside the open aggregate span.
        expect(mocks.diagnosticEvents.filter(event => event.kind === 'phase').slice(-3)).toEqual([
            expect.objectContaining({ phase: 'output-application', edge: 'enter' }),
            expect.objectContaining({ phase: 'checkpoint', edge: 'enter' }),
            expect.objectContaining({ phase: 'checkpoint', edge: 'exit', outcome: 'ok' }),
        ])
        expect(mocks.diagnosticEvents.some(event => ['semantic-output', 'inlay', 'tts'].includes(String(event.phase)))).toBe(false)
        finish.resolve()
        await expect(sending).resolves.toBe(true)
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.responseVariants?.candidates.map(candidate => candidate.messages[0].data)).toEqual(['first', 'second'])
    })

    it.each(['throw', 'reject', 'noop'] as const)('does not alter generation when diagnostics %s', async (mode) => {
        installDatabase()
        mocks.diagnosticMode = mode
        await expect(sendChat()).resolves.toBe(true)
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('answer')
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(mocks.diagnosticEvents).toEqual([])
    })

    it.each(['save-path', 'authority'] as const)('isolates diagnostic %s state failures from persistence and cleanup', async (failureKind) => {
        const { session } = installDatabase()
        mocks.selectedTarget = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.outputTriggerReturnsNull = true
        const failure = new Error('synthetic diagnostic state failure')
        Object.defineProperty(session, 'residencyFallbackActive', {
            get() {
                // The checkpoint reads the save path while applying output; only the reads for completion diagnostics fail.
                if (!mocks.diagnosticEvents.some(event => event.phase === 'completion-processing')) return false
                if (failureKind === 'save-path') throw failure
                mocks.diagnosticAuthorityError = failure
                return false
            },
        })

        await expect(sendChat()).resolves.toBe(true)

        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('answer')
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(session.pinCount('compatibility')).toBe(0)
        expect(session.pinCount('transaction')).toBe(0)
        expect(mocks.activeCompleteLeases).toBe(0)
        expect(mocks.completeLeaseReleases).toBe(1)
        expect(isGenerationReserved()).toBe(false)
        expect(get(doingChat)).toBe(false)
    })

    it.each(['success', 'failure'] as const)('records auxiliary completion work and preserves output and cleanup on %s', async (outcome) => {
        const { session } = installDatabase()
        mocks.selectedTarget = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.outputTriggerReturnsNull = true
        DBState.db.igpPrompt = 'synthetic auxiliary prompt'
        const entered = deferred<void>()
        const finish = deferred<void>()
        const failure = new Error('synthetic completion failure')
        mocks.completionRequest = async () => {
            entered.resolve()
            await finish.promise
            if (outcome === 'failure') throw failure
            return '|auxiliary'
        }
        const sending = sendChat()
        await entered.promise

        let settled: { value?: boolean; error?: unknown }
        try {
            expect(mocks.diagnosticEvents.filter(event => event.kind === 'phase').at(-1)).toMatchObject({
                phase: 'completion-processing', edge: 'enter',
            })
            expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('answer')
            expect(mocks.acknowledge).not.toHaveBeenCalled()
        } finally {
            finish.resolve()
            settled = await sending.then(value => ({ value }), error => ({ error }))
        }
        expect(settled).toEqual(outcome === 'failure' ? { error: failure } : { value: true })

        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({
            phase: 'completion-processing', edge: 'exit', outcome: outcome === 'failure' ? 'failed' : 'ok',
        }))
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe(outcome === 'failure' ? 'answer' : 'answer|auxiliary')
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
        expect(session.pinCount('compatibility')).toBe(0)
        expect(mocks.activeCompleteLeases).toBe(0)
        expect(isGenerationReserved()).toBe(false)
    })

    it.each([
        ['provider', 'awaiting-provider', 'error'],
        ['semantic', 'semantic-output', 'error'],
        ['stream', 'semantic-output', 'stream-error'],
    ] as const)('records a failed %s phase and its reason without recording the error', async (failureStage, phase, reason) => {
        installDatabase()
        const failure = new Error('PRIVATE_FAILURE_CANARY')
        if (failureStage === 'provider') mocks.modelResponse = () => { throw failure }
        else if (failureStage === 'stream') mocks.modelResponse = () => ({
            type: 'streaming',
            result: new ReadableStream<Record<string, string>>({
                start(controller) {
                    controller.enqueue({ response: 'partial' })
                    controller.error(failure)
                },
            }),
        })
        else mocks.processScriptFull.mockImplementation(async (_char, data, mode) => {
            if (mode === 'editoutput') throw failure
            return { data, emoChanged: false }
        })
        await expect(sendChat()).rejects.toBe(failure)
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase, edge: 'exit', outcome: 'failed', reason }))
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase: 'settled', edge: 'exit', outcome: 'failed' }))
        expect(JSON.stringify(mocks.diagnosticEvents)).not.toContain('PRIVATE_FAILURE_CANARY')
    })

    it('records why prompt preparation stopped', async () => {
        installDatabase()
        DBState.db.maxContext = 0
        await expect(sendChat()).resolves.toBe(false)
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({
            phase: 'prompt-preparation', edge: 'exit', outcome: 'failed', reason: 'token-limit',
        }))
        expect(mocks.modelRequestCount).toBe(0)
    })

    it('caps the local caption image without changing the stored attachment', async () => {
        installDatabase(makeChat([{ role: 'user', data: '{{inlay::caption-budget}}', chatId: 'image' }]))
        const original = { type: 'image', data: 'data:image/png;base64,iVBORw0KGgoH', width: 4000, height: 3000 }
        mocks.inlayAsset = { ...original }
        const create = document.createElement.bind(document)
        const canvas = vi.spyOn(document, 'createElement').mockImplementation((tag, options) => {
            const element = create(tag, options)
            if (tag === 'canvas') {
                const target = element as HTMLCanvasElement
                target.getContext = (() => ({ drawImage() {} })) as any
                target.toBlob = (callback) => callback(new Blob([JSON.stringify({ width: target.width, height: target.height })], { type: 'image/webp' }))
            }
            return element
        })
        try {
            await expect(sendChat()).resolves.toBe(true)
            const sent = mocks.caption.mock.calls[0]?.[0] as string
            expect(sent).toMatch(/^data:image\/webp;base64,/)
            const size = JSON.parse(atob(sent.split(',')[1]))
            expect(size.width * size.height).toBeLessThanOrEqual(1024 * 1024)
            expect(size.width).toBeGreaterThan(0)
            expect(mocks.inlayAsset).toEqual(original)
        } finally {
            canvas.mockRestore()
        }
    })

    it.each(['disabled', 'v2', 'hanurai'])('retains covered messages when the active memory route is %s', async (route) => {
        const installed = installDatabase(makeChat([
            { role: 'user', data: 'covered-a', chatId: 'a' },
            { role: 'user', data: 'tail', chatId: 'b' },
        ]))
        installed.chat.hypaV3Data = {
            summaries: [{ chatMemos: ['a'], text: 'covered' }],
        } as any
        DBState.db.hypaV3 = true
        installed.currentCharacter.supaMemory = route !== 'disabled'
        DBState.db.hypav2 = route === 'v2'
        DBState.db.hanuraiEnable = route === 'hanurai'

        await sendChat()

        expect(mocks.processScriptFull.mock.calls.some((call) => call[1] === 'covered-a')).toBe(true)
    })

    it.each(['plugin-editprocess', 'plugin-output', 'character-trigger', 'module-trigger', 'character-regex', 'module-regex'])
    ('preserves complete compatibility history with %s', async (consumer) => {
        const installed = installDatabase(makeChat([
            { role: 'user', data: 'covered-a', chatId: 'a' },
            { role: 'user', data: 'tail', chatId: 'b' },
        ]))
        installed.chat.hypaV3Data = { summaries: [{ chatMemos: ['a'], text: 'summary' }] } as any
        installed.currentCharacter.supaMemory = true
        DBState.db.hypaV3 = true
        mocks.selectedTarget = { characterId: installed.currentCharacter.chaId, conversationId: installed.chat.id, storeRevision: 1 }
        mocks.selectedAuthority = {
            kind: 'windowed',
            characterId: installed.currentCharacter.chaId,
            conversationId: installed.chat.id,
            sessionToken: 'windowed-session',
            storeRevision: 1,
            sessionVersion: 0,
            persistedSessionVersion: 0,
            totalMessages: installed.chat.message.length,
        }
        const output = vi.fn(async () => undefined)
        if (consumer === 'plugin-editprocess') mocks.editHooks.add(() => undefined)
        if (consumer === 'plugin-output') mocks.listeners.add(output)
        if (consumer === 'character-trigger') installed.currentCharacter.triggerscript = [{}] as any
        if (consumer === 'module-trigger') mocks.moduleTriggers = [{}]
        if (consumer === 'character-regex') installed.currentCharacter.customscript = [{ type: 'editprocess', in: 'a', out: '@@inject', flag: 'g' }] as any
        if (consumer === 'module-regex') mocks.moduleRegex = [{ type: 'editprocess', in: 'a', out: '{{setvar::test::1}}', flag: 'g' }]

        await expect(sendChat()).resolves.toBe(true)

        expect(mocks.acquireCompleteConversation).toHaveBeenCalledOnce()
        const preservesEffects = ['plugin-editprocess', 'character-regex', 'module-regex'].includes(consumer)
        expect(mocks.processScriptFull.mock.calls.some((call) => call[1] === 'covered-a')).toBe(preservesEffects)
        expect(mocks.events).toContain('output-trigger')
        expect(mocks.events).toContain('output-script')
        if (consumer === 'plugin-output') {
            expect(mocks.resolvePosition).toHaveBeenCalledExactlyOnceWith(installed.currentCharacter.chaId, installed.chat.id)
            expect(output).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({
                characterIndex: 0,
                chatIndex: 0,
                chat: expect.objectContaining({
                    message: [
                        expect.objectContaining({ data: 'covered-a' }),
                        expect.objectContaining({ data: 'tail' }),
                        expect.objectContaining({ data: 'answer' }),
                    ],
                }),
            }))
        }
    })

    it('generates from a pinned unsummarized tail without complete promotion', async () => {
        const installed = installDatabase(makeChat([]))
        const storedMessages = [
            { role: 'user', data: 'covered-a', chatId: 'a' },
            { role: 'char', data: 'covered-b', chatId: 'b' },
            { role: 'user', data: 'tail', chatId: 'c' },
        ] as Message[]
        installed.chat.hypaV3Data = {
            summaries: [{ chatMemos: ['a', 'b'], summary: 'covered' }],
        } as any
        DBState.db.hypaV3 = true
        installed.currentCharacter.supaMemory = true
        mocks.session = null
        mocks.outputTriggerReturnsNull = true
        mocks.selectedTarget = {
            characterId: installed.currentCharacter.chaId,
            conversationId: installed.chat.id,
            navigationGeneration: 0,
            storeRevision: 1,
        }
        mocks.selectedAuthority = {
            kind: 'windowed',
            characterId: installed.currentCharacter.chaId,
            conversationId: installed.chat.id,
            sessionToken: 'windowed-session',
            storeRevision: 1,
            persistedSessionVersion: 0,
            sessionVersion: 0,
            totalMessages: storedMessages.length,
        }
        const bodyReads: number[] = []
        const lease = createLeaseStub({
            revision: 1,
            async readConversationMessageMetadataWindow(input) {
                const end = Math.min(storedMessages.length, input.startIndex + input.limit)
                return {
                    revision: 1,
                    value: {
                        characterId: input.characterId,
                        conversationId: input.conversationId,
                        messages: storedMessages.slice(input.startIndex, end).map((message) => ({
                            chatId: message.chatId,
                            role: message.role,
                            parserInert: true,
                        })),
                        startIndex: input.startIndex,
                        endIndex: end,
                        totalMessages: storedMessages.length,
                        hasMoreBefore: input.startIndex > 0,
                        hasMoreAfter: end < storedMessages.length,
                    },
                }
            },
            async readConversationWindow(input) {
                bodyReads.push(input.startIndex)
                return {
                    revision: 1,
                    value: {
                        characterId: input.characterId,
                        conversationId: input.conversationId,
                        messages: [structuredClone(storedMessages[input.startIndex])],
                        startIndex: input.startIndex,
                        endIndex: input.startIndex + 1,
                        totalMessages: storedMessages.length,
                        hasMoreBefore: input.startIndex > 0,
                        hasMoreAfter: input.startIndex + 1 < storedMessages.length,
                    },
                }
            },
            release: vi.fn(async () => undefined),
        })
        mocks.persistentStore = {
            readConversationMessageMetadataWindow: vi.fn(),
            acquireRevision: vi.fn(async () => lease),
        }
        const mutations: any[] = []
        mocks.windowedController = (_target: any, chat: Chat, absoluteStartIndex: number) => ({
            chat,
            absoluteStartIndex,
            isCurrent: () => true,
            applyRange(localStart: number, deleteCount: number, messages: Message[]) {
                chat.message.splice(localStart, deleteCount, ...structuredClone(messages))
                mutations.push({
                    start: absoluteStartIndex + localStart,
                    deleteCount,
                    messages: structuredClone(messages),
                })
                return true
            },
            release: vi.fn(),
        })

        await expect(sendChat()).resolves.toBe(true)

        expect(mocks.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(bodyReads).toEqual([2])
        expect(mutations.some((mutation) =>
            mutation.start === 3 && mutation.messages[0]?.data === 'answer',
        )).toBe(true)
        expect(lease.release).toHaveBeenCalledOnce()
        expect(mocks.acknowledge).toHaveBeenCalledOnce()
    })

    it('discards a provider result from an old authority even when target IDs and objects are retained', async () => {
        installDatabase()
        const response = deferred<any>()
        mocks.modelResponse = response.promise
        const sending = sendChat()
        await vi.waitFor(() => expect(mocks.modelRequestCount).toBe(1))
        const before = JSON.stringify(DBState.db.characters[0].chats[0].message)
        mocks.authorityEpoch++
        response.resolve(streamingResponse('must not be applied'))

        await expect(sending).resolves.toBe(false)
        expect(JSON.stringify(DBState.db.characters[0].chats[0].message)).toBe(before)
        expect(mocks.acknowledge).not.toHaveBeenCalled()
        expect(get(doingChat)).toBe(false)
    })

    it('refuses late output-script results after an authority replacement', async () => {
        installDatabase()
        const script = deferred<{ data: string; emoChanged: boolean }>()
        mocks.processScriptFull.mockImplementation(async (_char, data, mode) =>
            mode === 'editoutput' ? script.promise : { data, emoChanged: false })
        const sending = sendChat()
        await vi.waitFor(() => expect(mocks.processScriptFull.mock.calls.some(
            (call) => call[2] === 'editoutput',
        )).toBe(true))
        mocks.authorityEpoch++
        const before = JSON.stringify(DBState.db.characters[0].chats[0].message)
        script.resolve({ data: 'late transformed text', emoChanged: false })

        await expect(sending).resolves.toBe(false)
        expect(JSON.stringify(DBState.db.characters[0].chats[0].message)).toBe(before)
        expect(mocks.acknowledge).not.toHaveBeenCalled()
        expect(get(doingChat)).toBe(false)
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

    it('applies the response only after a plugin patch admitted during the request lands', async () => {
        const { chat, currentCharacter, session } = installDatabase()
        const target = { characterId: currentCharacter.chaId, conversationId: chat.id! }
        let release: (() => void) | undefined
        let openDuringRequest = false
        mocks.modelResponse = () => {
            openDuringRequest = isGenerationRequestPhaseOpen(target)
            release = trackConversationPatch(target)
            return streamingResponse('answer')
        }

        const sending = sendChat()
        while (!release) await Promise.resolve()
        for (let tick = 0; tick < 20; tick++) await Promise.resolve()
        expect(openDuringRequest).toBe(true)
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase: 'awaiting-provider', edge: 'exit', outcome: 'ok' }))
        expect(mocks.diagnosticEvents.filter(event => event.phase === 'host-response')).toEqual([
            expect.objectContaining({ phase: 'host-response', edge: 'enter', resultKind: 'streaming' }),
        ])
        expect(isGenerationRequestPhaseOpen(target)).toBe(false)
        expect(chat.message.map((message) => message.data)).toEqual(['hello'])
        const next = JSON.parse(JSON.stringify(chat))
        next.message[0].__translation = 'record'
        next.scriptstate = { $bridge: 'on' }
        expect(session.adoptPersistedMetadata(next, 2)).toBe(true)
        release()

        await expect(sending).resolves.toBe(true)
        const completed = DBState.db.characters[0].chats[0]
        expect(completed.message.at(-1)?.data).toBe('answer')
        expect(completed.message[0]).toMatchObject({ data: 'hello', __translation: 'record' })
        expect(completed.scriptstate).toEqual({ $bridge: 'on' })
    })

    it('closes the request phase when the provider request throws', async () => {
        const { chat, currentCharacter } = installDatabase()
        const target = { characterId: currentCharacter.chaId, conversationId: chat.id! }
        let openDuringRequest = false
        mocks.modelResponse = () => {
            openDuringRequest = isGenerationRequestPhaseOpen(target)
            throw new Error('provider failed')
        }

        await sendChat().catch(() => undefined)

        expect(openDuringRequest).toBe(true)
        expect(mocks.modelRequestCount).toBe(1)
        expect(isGenerationRequestPhaseOpen(target)).toBe(false)
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
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({
            phase: 'completion-processing', edge: 'exit', outcome: 'stale',
        }))
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({
            phase: 'settled', edge: 'exit', outcome: 'stale',
        }))
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
        expect(publishedChat.message.at(-1)?.responseVariants?.candidates.map(
            (candidate) => candidate.messages[0].data,
        )).toEqual([
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
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase: 'inlay', edge: 'exit', outcome: 'stale' }))
        expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase: 'settled', edge: 'exit', outcome: 'stale' }))
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
        },
    ])('preserves removeIncompleteResponse trimming for $name response application', async ({
        response,
        expectedStored,
        expectedOutputInputs,
        expectedTrimInputs,
        name,
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
        const candidates = DBState.db.characters[0].chats[0].message.at(-1)
            ?.responseVariants?.candidates.map((candidate) => candidate.messages[0].data)
        expect(candidates).toEqual(name === 'multiline'
            ? ['trim:script:First raw', 'trim:script:Second raw']
            : undefined)
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
            const sending = sendChat({ continue: continuing, signal: controller.signal })
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
            expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase: 'semantic-output', edge: 'exit', outcome: 'aborted' }))
            expect(mocks.diagnosticEvents).toContainEqual(expect.objectContaining({ phase: 'settled', edge: 'exit', outcome: 'aborted' }))
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
            const sending = sendChat({ signal: controller.signal })
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
            const sending = sendChat({ signal: controller.signal })
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
        const sending = sendChat({ signal: controller.signal })
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

        await expect(sendChat({ continue: true })).resolves.toBe(true)

        expect(DBState.db.characters[0].chats[0].message).toHaveLength(1)
        expect(DBState.db.characters[0].chats[0].message[0].data).toBe('existing plus')
        expect(DBState.db.characters[0].chats[0].message[0]).toMatchObject({
            __translation: 'existing-record',
        })
        expect(session.pinCount('transaction')).toBe(0)
    })

    it.each(['expired', 'foreground-expired', 'user-stop'] as const)('retains partial text and reports only background expiry for %s', async (end) => {
        const { alertNormal } = await import('../alert')
        const { notifyIOSGenerationComplete } = await import('../iosNative')
        const { language } = await import('../../lang')
        const { setGenerationBackground } = await import('./generationState')
        const { beginGenerationBackground } = await import('./generationBackground')
        installDatabase()
        mocks.ios = true
        setGenerationBackground(beginGenerationBackground)
        const events = new EventTarget()
        const dispose = vi.fn(async () => undefined)
        mocks.iosDependencies = {
            enabled: () => true, begin: async () => ({ id: 'generation' }), end: dispose,
            events, state: async () => ({ activeTasks: [], expiredTasks: ['generation'] }),
        }
        const user = new AbortController()
        mocks.modelResponse = { type: 'streaming', result: new ReadableStream<Record<string, string>>({
            start(controller) { controller.enqueue({ 0: 'Kept partial' }) },
        }) }
        vi.mocked(alertNormal).mockClear()
        vi.mocked(notifyIOSGenerationComplete).mockClear()
        const pending = sendChat({ signal: user.signal })
        await vi.waitFor(() => expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('Kept partial'))
        if (end === 'user-stop') user.abort()
        else events.dispatchEvent(new CustomEvent('risunest-ios-lifecycle', { detail: { event: end === 'expired' ? 'expired' : 'active', id: 'generation' } }))
        await expect(pending).resolves.toBe(false)
        expect(DBState.db.characters[0].chats[0].message.at(-1)?.data).toBe('Kept partial')
        if (end === 'user-stop') expect(alertNormal).not.toHaveBeenCalled()
        else expect(alertNormal).toHaveBeenCalledExactlyOnceWith(language.generationInterruptedInBackground)
        expect(notifyIOSGenerationComplete).not.toHaveBeenCalled()
        await vi.waitFor(() => expect(dispose).toHaveBeenCalledWith('generation', false))
        mocks.iosDependencies = undefined
        mocks.ios = false
        setGenerationBackground(undefined)
    })

    describe('generation end notes', () => {
        const target = { characterId: 'character-a', conversationId: 'chat-a' }
        const stopListening: Array<() => void> = []
        afterEach(() => {
            while (stopListening.length) stopListening.pop()!()
        })

        async function runGeneration(status: GenerationEndStatus, generate: () => Promise<boolean>) {
            const records: GenerationEndRecord[] = []
            stopListening.push(subscribeGenerationEnd((record) => records.push(record)))
            const run = beginGenerationEndRun(target)
            const result = await run.capture(generate)
            run.finish(status)
            return { result, records }
        }

        it('collects an auto-continue chain into one generation that names the message it wrote', async () => {
            installDatabase()
            DBState.db.autoContinueChat = true
            mocks.modelResponse = () => streamingResponse('answer')
            mocks.outputTrigger = () => null
            mocks.lastCharPunctuation = false
            mocks.listeners.add(() => {
                if (mocks.modelRequestCount === 2) mocks.lastCharPunctuation = true
            })

            const { result, records } = await runGeneration('completed', () => sendChat())

            expect(result).toBe(true)
            expect(mocks.modelRequestCount).toBe(2)
            const written = DBState.db.characters[0].chats[0].message.at(-1)!
            expect(written.role).toBe('char')
            expect(records).toHaveLength(1)
            expect(records[0].messageIds).toHaveLength(2)
            expect(records[0].messageIds).toContain(written.chatId)
        })

        it.each([
            { name: 'appended after a user tail', messages: [{ role: 'user', data: 'prompt', chatId: 'user-1' }] as Message[] },
            { name: 'added to the character tail', messages: [{ role: 'char', data: 'partial', chatId: 'response-1' }] as Message[] },
        ])('names the inlay error $name', async ({ messages }) => {
            const { chat } = installDatabase(makeChat(messages))
            DBState.db.inlayErrorResponse = true
            mocks.modelResponse = { type: 'fail', result: 'request failed' }

            const { result, records } = await runGeneration('failed', () => sendChat())

            expect(result).toBe(false)
            const written = chat.message.at(-1)!
            expect(written.data).toContain('request failed')
            expect(records).toHaveLength(1)
            expect(records[0].messageIds).toContain(written.chatId)
        })

        it('reports nothing for a prompt preview', async () => {
            installDatabase()

            const { result, records } = await runGeneration('completed', () => sendChat({ preview: true }))

            expect(result).toBe(true)
            expect(records).toEqual([])
        })
    })

    describe('start triggers', () => {
        beforeEach(() => {
            mocks.realTriggers = true
        })

        function installStartTrigger(
            trigger: triggerscript,
            onMutation: ConstructorParameters<typeof ActiveConversationSession>[0]['onMutation'],
        ) {
            const installed = installDatabase(makeChat(), [], onMutation)
            installed.currentCharacter.triggerscript = [trigger]
            DBState.db.templateDefaultVariables = ''
            return installed
        }

        it('completes generation when a setvar start trigger commits through the active session', async () => {
            const onMutation = vi.fn()
            const { chat, session } = installStartTrigger({
                comment: 'set flag on start',
                type: 'start',
                conditions: [],
                effect: [{ type: 'setvar', operator: '=', var: 'flag', value: 'on' }],
            }, onMutation)

            await expect(sendChat()).resolves.toBe(true)

            expect(chat.scriptstate).toEqual({ $flag: 'on' })
            expect(chat.message.at(-1)).toMatchObject({ role: 'char', data: 'answer' })
            expect(mocks.modelRequestCount).toBe(1)
            expect(session.matchesConversation('character-a', chat)).toBe(true)
            expect(onMutation.mock.calls.map(([event]) => event.commands)).not.toContainEqual(
                ['replace-conversation'],
            )
        })

        it('does not self-replace the conversation when the start trigger changes nothing', async () => {
            const onMutation = vi.fn()
            const { chat } = installStartTrigger({
                comment: 'never passes',
                type: 'start',
                conditions: [{ type: 'value', var: 'a', value: 'b', operator: '=' }],
                effect: [{ type: 'setvar', operator: '=', var: 'flag', value: 'on' }],
            }, onMutation)

            await expect(sendChat()).resolves.toBe(true)

            expect(chat.scriptstate).toBeUndefined()
            expect(onMutation.mock.calls.map(([event]) => event.commands)).not.toContainEqual(
                ['replace-conversation'],
            )
        })
    })
})
