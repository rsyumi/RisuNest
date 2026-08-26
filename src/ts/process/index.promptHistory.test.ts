import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const testState = vi.hoisted(() => ({
    activeSession: null as unknown,
    runTrigger: vi.fn(),
    onTokenizeChat: null as null | (() => void | Promise<void>),
    cbsCallbacks: new Map<string, (...args: any[]) => any>(),
    pluginV2: {
        providers: new Map(),
        providerOptions: new Map(),
        editdisplay: new Set<(content: string) => string | Promise<string>>(),
        editoutput: new Set<(content: string) => string | Promise<string>>(),
        editprocess: new Set<(content: string) => string | Promise<string>>(),
        editinput: new Set<(content: string) => string | Promise<string>>(),
        replacerbeforeRequest: new Set(),
        replacerafterRequest: new Set(),
        chatOutput: new Set(),
        unload: new Set(),
        loaded: false,
    },
}))

vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/g,
    risuChatParser: (data: string, args: Record<string, any> = {}) => data.replace(
        /{{([^{}]+)}}/g,
        (source, body: string) => {
            const parts = body.includes('::') ? body.split('::') : body.split(':')
            const name = parts[0].toLocaleLowerCase().replace(/[\s_-]/g, '')
            const callback = testState.cbsCallbacks.get(name)
            if (!callback) return source
            const result = callback(source, {
                chatID: args.chatID ?? -1,
                db: {},
                chara: args.chara ?? '',
                rmVar: false,
                cbsConditions: args.cbsConditions ?? {},
            }, parts.slice(1), {})
            if (typeof result === 'string') return result
            if (result && typeof result.text === 'string') return result.text
            return source
        },
    ),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: () => testState.activeSession,
}))

vi.mock('../plugins/plugins.svelte', () => ({
    pluginV2: testState.pluginV2,
}))

vi.mock('./triggers', () => ({
    runTrigger: testState.runTrigger,
}))

vi.mock('../tokenizer', () => ({
    ChatTokenizer: class {
        async tokenizeChat(): Promise<number> {
            await testState.onTokenizeChat?.()
            return 1
        }
    },
    tokenize: vi.fn(async () => 1),
    tokenizeNum: vi.fn(async () => 1),
}))

vi.mock('./lorebook.svelte', () => ({
    loadLoreBookV3Prompt: vi.fn(async () => ({ actives: [] })),
}))

vi.mock('./request/request', () => ({
    requestChatData: vi.fn(),
}))

vi.mock('./stableDiff', () => ({ stableDiff: vi.fn() }))
vi.mock('./tts', () => ({ sayTTS: vi.fn() }))
vi.mock('./exampleMessages', () => ({ exampleMessage: () => [] }))
vi.mock('./group', () => ({ groupOrder: (items: unknown[]) => items }))
vi.mock('./memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('./memory/supaMemory', () => ({ supaMemory: vi.fn() }))
vi.mock('./memory/hanuraiMemory', () => ({ hanuraiMemory: vi.fn() }))
vi.mock('./memory/hypav2', () => ({ hypaMemoryV2: vi.fn() }))
vi.mock('./memory/hypav3', () => ({
    createHypaV3Preset: (name: string, settings: Record<string, unknown>) => ({ name, settings }),
    hypaMemoryV3: vi.fn(),
}))
vi.mock('./embedding/addinfo', () => ({ additionalInformations: vi.fn(async () => '') }))
vi.mock('./files/inlays', () => ({
    getInlayAsset: vi.fn(async () => null),
    getInlayAssetMetadata: vi.fn(async () => null),
}))
vi.mock('./models/modelString', () => ({ getGenerationModelString: () => 'test-model' }))
vi.mock('../sync/multiuser', () => ({
    connectionOpen: false,
    peerRevertChat: vi.fn(),
    peerSafeCheck: vi.fn(async () => true),
    peerSync: vi.fn(),
}))
vi.mock('./inlayScreen', () => ({ runInlayScreen: vi.fn() }))
vi.mock('./prereroll', () => ({ addRerolls: vi.fn() }))
vi.mock('./transformers', () => ({ runImageEmbedding: vi.fn(async () => []) }))
vi.mock('./scriptings', () => ({
    runLuaEditTrigger: vi.fn(async (_char, _mode, content) => content),
}))
vi.mock('../model/modellist', () => ({
    getModelInfo: () => ({ flags: [] }),
    LLMFlags: { hasImageInput: 0 },
    LLMFormat: { OpenAICompatible: 0, Ollama: 15 },
}))
vi.mock('./modules', () => ({
    getModuleAssets: () => [],
    getModuleLorebooks: () => [],
    getModuleRegexScripts: () => [],
    getModuleToggles: () => '',
    getModules: () => [],
    moduleUpdate: vi.fn(),
}))
vi.mock('../globalApi.svelte', () => ({
    aiWatermarkingLawApplies: () => false,
    downloadFile: vi.fn(),
    getFileSrc: vi.fn(async () => ''),
    readImage: vi.fn(async () => new Uint8Array()),
}))
vi.mock('./presetChain', () => ({ activatePresetChainForRequest: vi.fn() }))
vi.mock('./streamingDisplayStream', () => ({
    captureStreamingMessageTarget: vi.fn(),
    consumeStreamingDisplayStream: vi.fn(),
}))

import { get } from 'svelte/store'
import { defaultCBSRegisterArg, registerCBS } from '../cbs'
import type { Chat, Database, Message, character } from '../storage/database.svelte'
import {
    normalizeDatabaseDefaults,
    setDatabaseLite,
} from '../storage/database.svelte'
import { roadmap14Corpus } from '../storage/tests/roadmap14/losslessCorpus'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { DBState, selectedCharID } from '../stores.svelte'
import { doingChat } from './generationState'
import { previewFormated, sendChat } from './index.svelte'
import { resetScriptCache } from './scripts'

const TAIL_TOKEN = 'TAIL_TOKEN'
const ACTIVE_MESSAGE_COUNT = 130

registerCBS({
    ...defaultCBSRegisterArg,
    registerFunction: ({ name, alias, callback }) => {
        if (callback === 'doc_only') return
        for (const key of [name, ...alias]) {
            testState.cbsCallbacks.set(
                key.toLocaleLowerCase().replace(/[\s_-]/g, ''),
                callback,
            )
        }
    },
    getDatabase: () => DBState.db,
    getSelectedCharID: () => get(selectedCharID),
})

function makeActiveMessages(): Message[] {
    return Array.from({ length: ACTIVE_MESSAGE_COUNT }, (_, index) => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: index === ACTIVE_MESSAGE_COUNT - 1
            ? TAIL_TOKEN
            : index === 10 || index === 11
                ? `duplicate ${TAIL_TOKEN}`
                : `entry-${index.toString().padStart(3, '0')} ${TAIL_TOKEN}`,
        chatId: index === 0 ? '' : `active-${index}`,
    }))
}

function makeDatabase(): Database {
    const database = structuredClone(roadmap14Corpus.database)
    const sourceCharacter = database.characters.find((value) => value.type === 'character') as character
    const activeMessages = makeActiveMessages()
    const chat: Chat = {
        id: 'prompt-characterization',
        name: 'Before trigger',
        note: '',
        localLore: [],
        message: [
            { role: 'user', data: 'old history without id' },
            { role: 'char', data: 'disabled history without id', disabled: true },
            { role: 'user', data: 'reset history without id', disabled: 'allBefore' },
            ...activeMessages,
        ],
    }
    const selectedCharacter: character = {
        ...sourceCharacter,
        chaId: 'prompt-character',
        name: 'Prompt Character',
        chats: [chat],
        chatPage: 0,
        customscript: [{
            comment: 'CBS-backed prompt regex',
            in: '{{lastmessage}}',
            out: 'CBS_REGEX',
            type: 'editprocess',
            flag: 'g<cbs>',
            ableFlag: true,
        }],
        triggerscript: [{
            comment: 'Trigger identity characterization',
            type: 'start',
            conditions: [],
            effect: [],
        }],
        globalLore: [],
        firstMessage: 'unused greeting',
        exampleMessage: '',
        desc: '',
        personality: '',
        scenario: '',
        bias: [],
    }
    database.characters = [selectedCharacter]
    database.aiModel = 'gpt-test'
    database.maxContext = 100_000
    database.maxResponse = 0
    database.promptTemplate = [{ type: 'chat', rangeStart: 0, rangeEnd: 'end' }]
    database.formatingOrder = []
    normalizeDatabaseDefaults(database)
    database.promptSettings.trimStartNewChat = true
    database.promptSettings.sendName = false
    database.promptSettings.sendChatAsSystem = false
    database.promptSettings.maxThoughtTagDepth = -1
    database.promptInfoInsideChat = false
    database.automaticCachePoint = false
    return database
}

function mockTriggerClone(): void {
    testState.runTrigger.mockImplementation(async (_char, mode, { chat: triggerChat }) => {
        expect(mode).toBe('start')
        return {
            additonalSysPrompt: { start: '', historyend: '', promptend: '' },
            chat: {
                ...triggerChat,
                name: 'After trigger',
                message: triggerChat.message.map((message) => ({ ...message })),
            },
            tokens: 0,
            stopSending: false,
            sendAIprompt: false,
        }
    })
}

describe('sendChat prompt history characterization', () => {
    beforeEach(() => {
        selectedCharID.set(0)
        doingChat.set(false)
        testState.activeSession = null
        testState.runTrigger.mockReset()
        testState.onTokenizeChat = null
        testState.pluginV2.editprocess.clear()
        resetScriptCache()
    })

    afterEach(() => {
        doingChat.set(false)
        testState.activeSession = null
        testState.onTokenizeChat = null
        testState.pluginV2.editprocess.clear()
    })

    it('preserves final OpenAIChat parity through trigger cloning, scripts, CBS, regex, and 128-message pages', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 41,
        })
        const readRange = vi.spyOn(session, 'readRange')
        testState.activeSession = session
        mockTriggerClone()

        const result = await sendChat(-1, { preview: true })

        expect(result).toBe(true)
        expect(testState.runTrigger).toHaveBeenCalledTimes(1)
        expect(liveChat.name).toBe('After trigger')
        expect(selectedCharacter.chats[0]).toBe(liveChat)
        expect(session.matchesConversation(selectedCharacter.chaId, liveChat)).toBe(true)
        expect(session.activePinReasons).toEqual([])
        expect(readRange.mock.calls.map(([start, limit]) => [start, limit])).toEqual([
            [3, 128],
            [131, 2],
        ])

        const liveActiveMessages = liveChat.message.slice(3)
        expect(liveActiveMessages[0].chatId).not.toBe('')
        const expected = liveActiveMessages.map((message) => ({
            role: message.role === 'user' ? 'user' : 'assistant',
            content: message.data.replaceAll(TAIL_TOKEN, 'CBS_REGEX'),
            memo: message.chatId,
            attr: [],
            thoughts: [],
            removable: true,
        }))
        expect(previewFormated).toEqual(expected)
        expect(previewFormated).toHaveLength(ACTIVE_MESSAGE_COUNT)
        expect(previewFormated[10].content).toBe('duplicate CBS_REGEX')
        expect(previewFormated[11].content).toBe('duplicate CBS_REGEX')
        expect(get(doingChat)).toBe(false)
    })

    it('uses an explicit compatibility snapshot for stateful Plugin v2 prompt listeners', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 42,
        })
        const readRange = vi.spyOn(session, 'readRange')
        testState.activeSession = session
        let pluginCalls = 0
        testState.pluginV2.editprocess.add((content) => {
            pluginCalls += 1
            if (pluginCalls === 1) {
                const retainedSecondMessage = liveChat.message[4]
                retainedSecondMessage.data = `plugin-mutated ${TAIL_TOKEN}`
                liveChat.message.splice(4, 1, {
                    role: 'char',
                    data: `replacement must not enter prompt ${TAIL_TOKEN}`,
                    chatId: 'replacement',
                })
            }
            return `${content}|PLUGIN`
        })
        mockTriggerClone()

        const result = await sendChat(-1, { preview: true })

        expect(result).toBe(true)
        expect(readRange).not.toHaveBeenCalled()
        expect(session.matchesConversation(selectedCharacter.chaId, liveChat)).toBe(true)
        expect(session.activePinReasons).toEqual([])
        expect(previewFormated).toHaveLength(ACTIVE_MESSAGE_COUNT)
        expect(previewFormated[0].content).toBe('entry-000 CBS_REGEX|PLUGIN')
        expect(previewFormated[1].content).toBe('plugin-mutated CBS_REGEX|PLUGIN')
        expect(previewFormated[ACTIVE_MESSAGE_COUNT - 1].content).toBe('CBS_REGEX|PLUGIN')
        expect(previewFormated.some((message) => message.memo === 'replacement')).toBe(false)
    })

    it('persists a compatibility-snapshot ID across prompt builds without an active session', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        testState.activeSession = null
        mockTriggerClone()

        expect(await sendChat(-1, { preview: true })).toBe(true)
        const firstMemo = previewFormated[0].memo
        expect(firstMemo).toBeTruthy()
        expect(selectedCharacter.chats[0].message[3].chatId).toBe(firstMemo)

        expect(await sendChat(-1, { preview: true })).toBe(true)
        expect(previewFormated[0].memo).toBe(firstMemo)
        expect(selectedCharacter.chats[0].message[3].chatId).toBe(firstMemo)
    })

    it.each(['edit', 'delete'] as const)(
        'fails closed before formatting another cached page entry after a same-page session %s',
        async (mutation) => {
            setDatabaseLite(makeDatabase())
            const selectedCharacter = DBState.db.characters[0] as character
            const liveChat = selectedCharacter.chats[0]
            const session = new ActiveConversationSession({
                characterId: selectedCharacter.chaId,
                conversationId: liveChat.id!,
                conversation: liveChat,
                storeRevision: 43,
            })
            testState.activeSession = session
            mockTriggerClone()
            liveChat.message[4].chatId = ''

            let mutationApplied = false
            let tokenizeCalls = 0
            let mutationTarget: Message | undefined
            testState.onTokenizeChat = () => {
                tokenizeCalls += 1
                if (mutationApplied) return
                mutationApplied = true
                const locator = session.locate(4)
                mutationTarget = liveChat.message[4]
                if (mutation === 'edit') {
                    session.edit(locator, {
                        ...liveChat.message[4],
                        data: `ui-edited ${TAIL_TOKEN}`,
                    })
                } else {
                    session.delete(locator)
                }
            }

            await expect(sendChat(-1, { preview: true })).rejects.toMatchObject({
                name: 'ConversationSessionStaleError',
            })

            expect(mutationApplied).toBe(true)
            expect(tokenizeCalls).toBe(1)
            expect(session.version).toBe(2)
            expect(mutationTarget?.chatId).toBe('')
            if (mutation === 'edit') {
                expect(liveChat.message[4].data).toBe(`ui-edited ${TAIL_TOKEN}`)
                expect(liveChat.message[4].chatId).toBe('')
            } else {
                expect(liveChat.message[4].data).toBe(`entry-002 ${TAIL_TOKEN}`)
            }
            expect(session.activePinReasons).toEqual([])
        },
    )

    it.each(['edit', 'delete'] as const)(
        'rejects a deferred trigger clone when a concurrent UI %s wins the session CAS',
        async (mutation) => {
            setDatabaseLite(makeDatabase())
            const selectedCharacter = DBState.db.characters[0] as character
            const liveChat = selectedCharacter.chats[0]
            const session = new ActiveConversationSession({
                characterId: selectedCharacter.chaId,
                conversationId: liveChat.id!,
                conversation: liveChat,
                storeRevision: 44,
            })
            testState.activeSession = session

            let releaseTrigger: (() => void) | undefined
            testState.runTrigger.mockImplementation((_char, mode, { chat: triggerChat }) => {
                expect(mode).toBe('start')
                const staleTriggerChat: Chat = {
                    ...triggerChat,
                    name: 'Stale trigger clone',
                    message: triggerChat.message.map((message) => ({ ...message })),
                }
                return new Promise((resolve) => {
                    releaseTrigger = () => resolve({
                        additonalSysPrompt: { start: '', historyend: '', promptend: '' },
                        chat: staleTriggerChat,
                        tokens: 0,
                        stopSending: false,
                        sendAIprompt: false,
                    })
                })
            })

            const pendingSend = sendChat(-1, { preview: true })
            await vi.waitFor(() => expect(releaseTrigger).toBeTypeOf('function'))
            const locator = session.locate(4)
            if (mutation === 'edit') {
                session.edit(locator, {
                    ...liveChat.message[4],
                    data: `concurrent-ui-edit ${TAIL_TOKEN}`,
                })
            } else {
                session.delete(locator)
            }
            releaseTrigger!()

            await expect(pendingSend).rejects.toMatchObject({
                name: 'ConversationSessionStaleError',
            })
            expect(liveChat.name).toBe('Before trigger')
            expect(selectedCharacter.chats[0]).toBe(liveChat)
            expect(session.matchesConversation(selectedCharacter.chaId, liveChat)).toBe(true)
            expect(session.version).toBe(1)
            if (mutation === 'edit') {
                expect(liveChat.message[4].data).toBe(`concurrent-ui-edit ${TAIL_TOKEN}`)
            } else {
                expect(liveChat.message[4].data).toBe(`entry-002 ${TAIL_TOKEN}`)
            }
            expect(session.activePinReasons).toEqual([])
        },
    )
})
