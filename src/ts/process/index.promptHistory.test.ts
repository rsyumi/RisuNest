import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const testState = vi.hoisted(() => ({
    unexpectedNativeRuntimeAccess: () => {
        throw new Error('Unexpected native runtime access in this test')
    },
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
                ...args,
                chatID: args.chatID ?? -1,
                chara: args.chara ?? '',
                rmVar: false,
                runVar: args.runVar ?? false,
                cbsConditions: args.cbsConditions ?? {},
            }, parts.slice(1), {})
            if (typeof result === 'string') return result
            if (result && typeof result.text === 'string') return result.text
            return source
        },
    ),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    acknowledgeGenerationCompletion: vi.fn(async () => undefined),
    acquireCompleteConversation: testState.unexpectedNativeRuntimeAccess,
    acquireDestructiveReplacementFence: testState.unexpectedNativeRuntimeAccess,
    captureSelectedConversationTarget: () => null,
    capturePersistentMutationToken: testState.unexpectedNativeRuntimeAccess,
    getActiveConversationSession: () => testState.activeSession,
    getPersistentDataRuntime: testState.unexpectedNativeRuntimeAccess,
    peekActiveConversationSession: () => testState.activeSession,
}))

vi.mock('../plugins/plugins.svelte', () => ({
    pluginV2: testState.pluginV2,
}))

vi.mock('./triggers', () => ({
    runTrigger: testState.runTrigger,
}))

vi.mock('../tokenizer', async () => (await import('./tests/sendChatTestHarness')).tokenizerModule({
    tokenizeChat: async () => {
        await testState.onTokenizeChat?.()
        return 1
    },
    tokenizeChats: async (chats: unknown[]) => {
        for(const _chat of chats) await testState.onTokenizeChat?.()
        return chats.length
    },
    tokenizeNum: vi.fn(async () => 1),
}))

vi.mock('./lorebook.svelte', async () => (await import('./tests/sendChatTestHarness')).lorebookModule())

vi.mock('./request/request', () => ({
    requestChatData: vi.fn(),
}))

vi.mock('./stableDiff', async () => (await import('./tests/sendChatTestHarness')).stableDiffModule())
vi.mock('./tts', async () => (await import('./tests/sendChatTestHarness')).ttsModule())
vi.mock('./exampleMessages', async () => (await import('./tests/sendChatTestHarness')).exampleMessagesModule())
vi.mock('./group', async () => (await import('./tests/sendChatTestHarness')).groupModule())
vi.mock('./memory/hypamemory', async () => (await import('./tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('./memory/supaMemory', async () => (await import('./tests/sendChatTestHarness')).supaMemoryModule())
vi.mock('./memory/hanuraiMemory', async () => (await import('./tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('./memory/hypav2', async () => (await import('./tests/sendChatTestHarness')).hypav2Module())
vi.mock('./memory/hypav3', async () => (await import('./tests/sendChatTestHarness')).hypav3Module({
    createHypaV3Preset: (name: string, settings: Record<string, unknown>) => ({ name, settings }),
}))
vi.mock('./embedding/addinfo', async () => (await import('./tests/sendChatTestHarness')).addinfoModule())
vi.mock('./files/inlays', async () => (await import('./tests/sendChatTestHarness')).inlaysModule({
    getInlayAssetMetadata: vi.fn(async () => null),
}))
vi.mock('./models/modelString', async () => (await import('./tests/sendChatTestHarness')).modelStringModule())
vi.mock('../sync/multiuser', async () => (await import('./tests/sendChatTestHarness')).multiuserModule())
vi.mock('./inlayScreen', () => ({ runInlayScreen: vi.fn() }))
vi.mock('./prereroll', async () => (await import('./tests/sendChatTestHarness')).prerollModule())
vi.mock('./transformers', async () => (await import('./tests/sendChatTestHarness')).transformersModule({
    runImageEmbedding: vi.fn(async () => []),
}))
vi.mock('./scriptings', async () => (await import('./tests/sendChatTestHarness')).scriptingsModule())
vi.mock('../model/modellist', async () => (await import('./tests/sendChatTestHarness')).modellistModule({
    LLMFlags: { hasImageInput: 0 },
    LLMFormat: { OpenAICompatible: 0, Ollama: 15 },
}))
vi.mock('./modules', async () => (await import('./tests/sendChatTestHarness')).modulesModule({
    getModuleLorebooks: () => [],
    getModuleRegexScripts: () => [],
    getModules: () => [],
}))
vi.mock('../globalApi.svelte', async () => (await import('./tests/sendChatTestHarness')).globalApiModule({
    aiWatermarkingLawApplies: () => false,
    downloadFile: vi.fn(),
    getFileSrc: vi.fn(async () => ''),
    readImage: vi.fn(async () => new Uint8Array()),
}))
vi.mock('./presetChain', async () => (await import('./tests/sendChatTestHarness')).presetChainModule())
vi.mock('./streamingDisplayStream', () => ({
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
            [0, 128],
            [128, 5],
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

    it('commits runVar message data and ordered chat variables through the paged session pass', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        liveChat.fmIndex = -1
        liveChat.message = [
            {
                role: 'user',
                data: '{{setvar::counter::1}}first',
                chatId: 'first',
            },
            {
                role: 'char',
                data: '{{addvar::counter::1}}{{getvar::counter}}',
                chatId: 'second',
            },
            { role: 'user', data: 'plain third', chatId: 'third' },
        ]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 45,
        })
        const acquirePin = vi.spyOn(session, 'acquirePin')
        const applyOperation = vi.spyOn(session, 'applyOperation')
        testState.activeSession = session
        mockTriggerClone()

        await expect(sendChat(-1, { preview: true })).resolves.toBe(true)

        expect(liveChat.message.map((message) => message.data)).toEqual([
            'first',
            '2',
            'plain third',
        ])
        expect(liveChat.scriptstate).toEqual({ '$counter': '2' })
        expect(acquirePin.mock.calls).toContainEqual(['compatibility'])
        expect(applyOperation.mock.calls[0][0]).toMatchObject({
            expectedVersion: 0,
            metadata: expect.objectContaining({
                scriptstate: { '$counter': '2' },
            }),
            ranges: [{
                deleteCount: 2,
                messages: [
                    expect.objectContaining({ data: 'first' }),
                    expect.objectContaining({ data: '2' }),
                ],
            }],
        })
        expect(session.activePinReasons).toEqual([])
    })

    it('shares one mutating prompt operation and exposes earlier injections to later outer CBS parsing', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        liveChat.message = [
            { role: 'user', data: 'WRITE:first', chatId: 'first' },
            { role: 'char', data: 'seen={{previouschatlog::0}}', chatId: 'second' },
            { role: 'user', data: 'seen={{previouschatlog::1}}', chatId: 'third' },
        ]
        liveChat.fmIndex = -1
        selectedCharacter.customscript = [{
            comment: 'Prepare stored content',
            in: 'WRITE:',
            out: 'STORED:',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }, {
            comment: 'Persist processed content',
            in: 'STORED:',
            out: '@@inject',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 46,
        })
        const readRange = vi.spyOn(session, 'readRange')
        testState.activeSession = session
        mockTriggerClone()

        await expect(sendChat(-1, { preview: true })).resolves.toBe(true)

        expect(previewFormated.slice(-3).map((message) => message.content)).toEqual([
            'first',
            'seen=first',
            'seen=seen=first',
        ])
        expect(liveChat.message.map((message) => message.data)).toEqual([
            'STORED:first',
            'seen=STORED:first',
            'seen=seen=STORED:first',
        ])
        expect(readRange.mock.calls.map(([start, limit]) => [start, limit])).toEqual([
            [0, 3],
            [0, 3],
            [0, 3],
            [0, 3],
        ])
        expect(session.version).toBe(3)
        expect(session.activePinReasons).toEqual([])
    })

    it('adopts a later prompt message ID without treating it as an unversioned baseline edit', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        liveChat.message = [
            { role: 'user', data: 'WRITE:first', chatId: 'first' },
            { role: 'char', data: 'plain second', chatId: '' },
        ]
        liveChat.fmIndex = -1
        selectedCharacter.customscript = [{
            comment: 'Prepare stored content',
            in: 'WRITE:',
            out: 'STORED:',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }, {
            comment: 'Persist processed content',
            in: 'STORED:',
            out: '@@inject',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 47,
        })
        testState.activeSession = session
        mockTriggerClone()

        await expect(sendChat(-1, { preview: true })).resolves.toBe(true)

        const promptMessages = previewFormated.slice(-2)
        expect(promptMessages.map((message) => message.content)).toEqual([
            'first',
            'plain second',
        ])
        expect(promptMessages[1].memo).toBeTruthy()
        expect(liveChat.message[1].chatId).toBe(promptMessages[1].memo)
        expect(liveChat.message.map((message) => message.data)).toEqual([
            'STORED:first',
            'plain second',
        ])
        expect(session.version).toBe(2)
        expect(session.activePinReasons).toEqual([])
    })

    it('discards a pending prompt operation when a same-owner session edit advances the version', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        liveChat.message = [
            { role: 'user', data: 'WRITE:first', chatId: 'first' },
            { role: 'char', data: 'second', chatId: 'second' },
            { role: 'user', data: 'third', chatId: 'third' },
        ]
        liveChat.fmIndex = -1
        selectedCharacter.customscript = [{
            comment: 'Prepare stored content',
            in: 'WRITE:',
            out: 'STORED:',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }, {
            comment: 'Persist processed content',
            in: 'STORED:',
            out: '@@inject',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 51,
        })
        testState.activeSession = session
        mockTriggerClone()

        let tokenizeCalls = 0
        let edited = false
        testState.onTokenizeChat = () => {
            tokenizeCalls += 1
            if (edited || tokenizeCalls < 2) return
            edited = true
            session.edit(session.locate(1), {
                ...liveChat.message[1],
                data: 'concurrent UI edit',
            })
        }

        await expect(sendChat(-1, { preview: true })).rejects.toMatchObject({
            name: 'ConversationSessionStaleError',
        })

        expect(edited).toBe(true)
        expect(liveChat.message.map((message) => message.data)).toEqual([
            'WRITE:first',
            'concurrent UI edit',
            'third',
        ])
        expect(session.version).toBe(2)
        expect(session.activePinReasons).toEqual([])
    })

    it('fails closed for an active session on another chat that aliases the same message array', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const sharedMessages: Message[] = [
            { role: 'user', data: 'reset', chatId: 'reset', disabled: 'allBefore' },
            { role: 'user', data: 'WRITE:first', chatId: 'first' },
        ]
        const sessionChat: Chat = {
            id: 'session-chat',
            name: 'Session chat',
            note: '',
            localLore: [],
            message: sharedMessages,
        }
        const selectedChat: Chat = {
            id: 'selected-chat',
            name: 'Selected chat',
            note: '',
            localLore: [],
            message: sharedMessages,
        }
        selectedCharacter.chats = [sessionChat, selectedChat]
        selectedCharacter.chatPage = 1
        selectedCharacter.customscript = [{
            comment: 'Must not route to the aliased session',
            in: 'WRITE:',
            out: '{{setvar::wrong_owner::value}}',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: sessionChat.id!,
            conversation: sessionChat,
            storeRevision: 52,
        })
        testState.activeSession = session
        testState.runTrigger.mockResolvedValue(null)

        await expect(sendChat(-1, { preview: true })).resolves.toBe(false)

        expect(sessionChat.scriptstate).toBeUndefined()
        expect(selectedChat.scriptstate).toBeUndefined()
        expect(sharedMessages[1].data).toBe('WRITE:first')
        expect(session.version).toBe(0)
        expect(session.activePinReasons).toEqual([])
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
        const acquirePin = vi.spyOn(session, 'acquirePin')
        testState.activeSession = session
        let pluginCalls = 0
        const tokenizePinReasons: string[][] = []
        testState.onTokenizeChat = () => {
            tokenizePinReasons.push([...session.activePinReasons])
        }
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
        expect(readRange.mock.calls.map(([start, limit]) => [start, limit])).toEqual([
            [0, 128],
            [128, 5],
        ])
        expect(session.matchesConversation(selectedCharacter.chaId, liveChat)).toBe(true)
        expect(session.activePinReasons).toEqual([])
        expect(previewFormated).toHaveLength(ACTIVE_MESSAGE_COUNT)
        expect(previewFormated[0].content).toBe('entry-000 CBS_REGEX|PLUGIN')
        expect(previewFormated[1].content).toBe('plugin-mutated CBS_REGEX|PLUGIN')
        expect(previewFormated[ACTIVE_MESSAGE_COUNT - 1].content).toBe('CBS_REGEX|PLUGIN')
        expect(previewFormated.some((message) => message.memo === 'replacement')).toBe(false)
        expect(tokenizePinReasons).toHaveLength(ACTIVE_MESSAGE_COUNT * 2)
        expect(tokenizePinReasons.slice(0, ACTIVE_MESSAGE_COUNT).every(
            (reasons) => reasons.includes('compatibility'),
        )).toBe(true)
        expect(acquirePin.mock.calls.filter(([reason]) => reason === 'compatibility')).toHaveLength(2)
        expect(acquirePin.mock.calls.filter(([reason]) => reason === 'transaction')).toHaveLength(0)
    })

    it('keeps mutating Plugin v2 prompt scripts on the pinned live-reference path', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        liveChat.message = [
            { role: 'user', data: 'WRITE:first', chatId: 'first' },
            { role: 'char', data: 'plain second', chatId: 'second' },
        ]
        liveChat.fmIndex = -1
        selectedCharacter.customscript = [{
            comment: 'Persist Plugin-processed content',
            in: 'STORE:',
            out: '@@inject',
            type: 'editprocess',
            flag: 'g',
            ableFlag: true,
        }]
        const session = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 48,
        })
        testState.activeSession = session
        testState.pluginV2.editprocess.add((content) => content.replace('WRITE:', 'STORE:'))
        mockTriggerClone()

        await expect(sendChat(-1, { preview: true })).resolves.toBe(true)

        expect(liveChat.message[0].data).toBe('STORE:first')
        expect(session.activePinReasons).toEqual([])
    })

    it('fails before the next outer CBS parse when Plugin v2 loses its prompt owner', async () => {
        setDatabaseLite(makeDatabase())
        const selectedCharacter = DBState.db.characters[0] as character
        const liveChat = selectedCharacter.chats[0]
        liveChat.message = [
            { role: 'user', data: 'reset', chatId: 'reset', disabled: 'allBefore' },
            { role: 'user', data: 'first {{lastmessage}}', chatId: 'first' },
            { role: 'char', data: 'second {{lastmessage}}', chatId: '' },
        ]
        selectedCharacter.customscript = []
        const originalSession = new ActiveConversationSession({
            characterId: selectedCharacter.chaId,
            conversationId: liveChat.id!,
            conversation: liveChat,
            storeRevision: 49,
        })
        testState.activeSession = originalSession
        mockTriggerClone()

        let pluginCalls = 0
        testState.pluginV2.editprocess.add((content) => {
            pluginCalls += 1
            return content
        })
        let replacementChat: Chat | undefined
        testState.onTokenizeChat = () => {
            if (replacementChat) return
            const replacementDatabase = makeDatabase()
            const replacementCharacter = replacementDatabase.characters[0] as character
            replacementCharacter.chaId = 'replacement-character'
            replacementChat = replacementCharacter.chats[0]
            replacementChat.id = 'replacement-conversation'
            replacementChat.message = [{
                role: 'user',
                data: 'replacement must remain untouched',
                chatId: 'replacement-message',
            }]
            setDatabaseLite(replacementDatabase)
            testState.activeSession = new ActiveConversationSession({
                characterId: replacementCharacter.chaId,
                conversationId: replacementChat.id,
                conversation: replacementChat,
                storeRevision: 50,
            })
        }

        await expect(sendChat(-1, { preview: true })).rejects.toMatchObject({
            name: 'ConversationSessionInactiveError',
        })

        expect(pluginCalls).toBe(1)
        expect(replacementChat?.message.map((message) => message.data)).toEqual([
            'replacement must remain untouched',
        ])
        expect(liveChat.message[2].chatId).toBe('')
        expect(originalSession.activePinReasons).toEqual([])
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
            expect(session.version).toBe(3)
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
            expect(session.version).toBe(2)
            if (mutation === 'edit') {
                expect(liveChat.message[4].data).toBe(`concurrent-ui-edit ${TAIL_TOKEN}`)
            } else {
                expect(liveChat.message[4].data).toBe(`entry-002 ${TAIL_TOKEN}`)
            }
            expect(session.activePinReasons).toEqual([])
        },
    )
})
