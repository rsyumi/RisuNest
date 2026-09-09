import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Chat, character, customscript } from '../storage/database.svelte'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import {
    ActiveConversationSession,
    cloneConversationMetadata,
    ConversationSessionStaleError,
} from '../storage/activeConversationSession'

const mocks = vi.hoisted(() => {
    const state = {
        emotions: {} as Record<string, [string, string, number][]>,
        cbsPatternCalls: 0,
        cbsFirstPattern: 'x',
        cbsFirstError: null as Error | null,
        workerFailure: null as unknown,
        workerCalls: 0,
        workerAvailable: false,
        workerData: 'worker-output',
        workerErrors: [] as Array<{ sourceIndex: number; error: Error }>,
        workerInput: null as string | null,
        nativeFailure: null as unknown,
        nativeCalls: 0,
        nativeData: undefined as string | undefined,
        nativeInput: null as string | null,
        currentChat: null as Chat | null,
        session: null as ActiveConversationSession | null,
        selectedCharIndex: 0,
        onHypaAddText: null as null | (() => void),
    }
    const charEmotionStore = {
        set(value: Record<string, [string, string, number][]>) {
            state.emotions = value
        },
    }
    return {
        state,
        charEmotionStore,
        selectedCharStore: {},
        pluginV2: {
            editinput: new Set<(data: string) => Promise<string | null>>(),
            editoutput: new Set<(data: string) => Promise<string | null>>(),
            editprocess: new Set<(data: string) => Promise<string | null>>(),
            editdisplay: new Set<(data: string) => Promise<string | null>>(),
        },
        database: {
            dynamicAssets: false,
            presetRegex: [] as customscript[],
            characters: [] as never[],
        },
    }
})
const moduleMocks = vi.hoisted(() => ({
    getModuleAssets: vi.fn(() => []),
    getModuleRegexScripts: vi.fn(() => []),
}))
const scriptingsMocks = vi.hoisted(() => ({
    runLuaEditTrigger: vi.fn(async (_char: unknown, _mode: unknown, data: string) => data),
}))

vi.mock('svelte/store', () => ({
    get: (store: unknown) => {
        if (store === mocks.charEmotionStore) return mocks.state.emotions
        if (store === mocks.selectedCharStore) return mocks.state.selectedCharIndex
        return 0
    },
}))
vi.mock('src/ts/stores.svelte', () => ({
    CharEmotion: mocks.charEmotionStore,
    selectedCharID: mocks.selectedCharStore,
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => mocks.database,
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(() => mocks.state.currentChat),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    peekActiveConversationSession: () => mocks.state.session,
}))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('src/ts/util', () => ({ selectSingleFile: vi.fn() }))
vi.mock('src/ts/parser/parser.svelte', () => ({
    assetRegex: /$^/g,
    risuChatParser: (data: string, options?: { setChatVar?: (key: string, value: string) => void }) => {
        if (data === '{{setvar::scratch::regex}}') {
            options?.setChatVar?.('scratch', 'regex')
            return 'written'
        }
        if(data === 'phase1-cbs-pattern'){
            mocks.state.cbsPatternCalls++
            if(mocks.state.cbsPatternCalls === 1){
                if(mocks.state.cbsFirstError){
                    throw mocks.state.cbsFirstError
                }
                return mocks.state.cbsFirstPattern
            }
            return 'x'
        }
        return data
    },
}))
vi.mock('src/ts/process/modules', () => moduleMocks)
vi.mock('src/ts/process/memory/hypamemory', () => ({
    HypaProcesser: class {
        get addText() {
            mocks.state.onHypaAddText?.()
            return undefined as never
        }
    },
}))
vi.mock('src/ts/process/scriptings', () => scriptingsMocks)
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginV2: mocks.pluginV2,
}))
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn() }))
vi.mock('./regexWorkerClient', async (importOriginal) => {
    const original = await importOriginal<typeof import('./regexWorkerClient')>()
    return {
        ...original,
        isRegexWorkerAvailable: () => mocks.state.workerAvailable,
        getSharedRegexWorkerClient: () => ({
            execute: async (_plan: unknown, input: string) => {
                mocks.state.workerCalls++
                mocks.state.workerInput = input
                if(mocks.state.workerFailure !== null){
                    throw mocks.state.workerFailure
                }
                return { data: mocks.state.workerData, errors: mocks.state.workerErrors }
            },
        }),
    }
})
vi.mock('./nativeRegexBatch', () => ({
    tryExecuteNativeRegexBatch: async (_plan: unknown, input: string) => {
        mocks.state.nativeCalls++
        mocks.state.nativeInput = input
        if(mocks.state.nativeFailure !== null){
            throw mocks.state.nativeFailure
        }
        return mocks.state.nativeData === undefined
            ? undefined
            : { data: mocks.state.nativeData, errors: [] }
    },
}))

const {
    createPromptScriptOperationScope,
    processScriptFull,
    resetScriptCache,
} = await import('./scripts')
const { RegexExecutionTimeoutError } = await import('./regexWorkerClient')
const { getCurrentCharacter, getCurrentChat } = await import('../storage/database.svelte')

function makeScript(input: string, output: string, flag = 'g'): customscript {
    return {
        comment: '',
        in: input,
        out: output,
        type: 'editoutput',
        flag,
        ableFlag: true,
    }
}

function makeCharacter(scripts: customscript[]): character {
    return {
        type: 'character',
        chaId: 'cache-character',
        customscript: scripts,
        emotionImages: [['happy', 'happy.png']],
    } as character
}

it('uses frozen capture script inputs without reading the live selected conversation or modules', async () => {
    const character = makeCharacter([])

    await processScriptFull(character, 'frozen', 'editdisplay', 0, { chatRole: 'char' }, {
        captureContext: {
            presetRegex: [],
            moduleRegexScripts: [],
            moduleAssets: [],
            dynamicAssets: false,
            dynamicAssetsEditDisplay: false,
            parserContext: {
                database: mocks.database as any,
                character,
                userName: 'Frozen User',
                personaPrompt: 'Frozen Persona',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
        },
    })

    expect(getCurrentCharacter).not.toHaveBeenCalled()
    expect(getCurrentChat).not.toHaveBeenCalled()
    expect(moduleMocks.getModuleAssets).not.toHaveBeenCalled()
    expect(moduleMocks.getModuleRegexScripts).not.toHaveBeenCalled()
})

const emptyResultWithAction = [
    makeScript('x', ''),
    makeScript('^$', '@@emo happy'),
]

describe('processScriptFull result caching', () => {
    beforeEach(() => {
        setRuntimePerformanceProfile('normal')
        resetScriptCache()
        mocks.state.emotions = {}
        mocks.state.cbsPatternCalls = 0
        mocks.state.cbsFirstPattern = 'x'
        mocks.state.cbsFirstError = null
        mocks.state.workerFailure = null
        mocks.state.workerCalls = 0
        mocks.state.workerAvailable = false
        mocks.state.workerData = 'worker-output'
        mocks.state.workerErrors = []
        mocks.state.workerInput = null
        mocks.state.nativeFailure = null
        mocks.state.nativeCalls = 0
        mocks.state.nativeData = undefined
        mocks.state.nativeInput = null
        mocks.state.currentChat = null
        mocks.state.session = null
        mocks.state.selectedCharIndex = 0
        mocks.state.onHypaAddText = null
        mocks.database.dynamicAssets = false
        mocks.database.characters = [] as never[]
        scriptingsMocks.runLuaEditTrigger.mockClear()
        for (const callbacks of Object.values(mocks.pluginV2)) callbacks.clear()
    })

    it('treats a cached empty string as a hit', async () => {
        const character = makeCharacter(emptyResultWithAction)

        expect(await processScriptFull(character, 'x', 'editoutput')).toEqual({
            data: '',
            emoChanged: true,
        })
        expect(await processScriptFull(character, 'x', 'editoutput')).toEqual({
            data: '',
            emoChanged: false,
        })
    })

    it('bypass neither reads nor writes the completed result cache', async () => {
        const character = makeCharacter(emptyResultWithAction)

        await processScriptFull(character, 'x', 'editoutput')
        expect((await processScriptFull(character, 'x', 'editoutput', -1, {}, { cache: 'bypass' })).emoChanged).toBe(true)

        resetScriptCache()
        expect((await processScriptFull(character, 'x', 'editoutput', -1, {}, { cache: 'bypass' })).emoChanged).toBe(true)
        expect((await processScriptFull(character, 'x', 'editoutput')).emoChanged).toBe(true)
        expect((await processScriptFull(character, 'x', 'editoutput')).emoChanged).toBe(false)
    })

    it('preserves the cache-key CBS parse before bypass execution', async () => {
        const character = makeCharacter([
            makeScript('phase1-cbs-pattern', 'b', 'g<cbs>'),
        ])
        mocks.state.cbsFirstPattern = '['

        const result = await processScriptFull(character, 'x', 'editoutput', -1, {}, { cache: 'bypass' })

        expect(result.data).toBe('b')
        expect(mocks.state.cbsPatternCalls).toBe(2)
    })

    it('preserves cache-key CBS parser errors when bypassing', async () => {
        const character = makeCharacter([
            makeScript('phase1-cbs-pattern', 'b', 'g<cbs>'),
        ])
        const parserError = new Error('cache-key CBS parser failure')
        mocks.state.cbsFirstError = parserError
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})

        await expect(processScriptFull(
            character,
            'x',
            'editoutput',
            -1,
            {},
            { cache: 'bypass' },
        )).rejects.toThrow(parserError)
        errorLog.mockRestore()
    })

    it('still applies the ruleset when the regex Worker is unusable', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        mocks.state.workerFailure = new Error('Worker is not defined')
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})

        const result = await processScriptFull(
            character,
            'a cat here',
            'editoutput',
            -1,
            {},
            { cache: 'bypass', regexWorker: true },
        )

        expect(mocks.state.workerCalls).toBe(1)
        expect(result.data).toBe('a dog here')
        errorLog.mockRestore()
    })

    it('does not fall back to the UI thread when the regex Worker times out', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        const timeout = new RegexExecutionTimeoutError(1)
        mocks.state.workerFailure = timeout

        await expect(processScriptFull(
            character,
            'a cat here',
            'editoutput',
            -1,
            {},
            { cache: 'bypass', regexWorker: true },
        )).rejects.toBe(timeout)
    })

    it('offloads eligible editoutput plans without a caller flag when a Worker is available', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        mocks.state.workerAvailable = true

        const first = await processScriptFull(character, 'a cat here', 'editoutput')
        const second = await processScriptFull(character, 'a cat here', 'editoutput')

        expect(mocks.state.workerCalls).toBe(1)
        expect(first.data).toBe('worker-output')
        expect(second.data).toBe('worker-output')
    })

    it('publishes a complete eligible native batch before consulting the Worker', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        mocks.state.workerAvailable = true
        mocks.state.nativeData = 'native-output'

        const result = await processScriptFull(
            character,
            'a cat here',
            'editoutput',
            -1,
            {},
            { cache: 'bypass' },
        )

        expect(mocks.state.nativeCalls).toBe(1)
        expect(mocks.state.workerCalls).toBe(0)
        expect(result.data).toBe('native-output')
    })

    it('falls back on the untouched input and preserves ordered Worker errors', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        const firstError = new Error('first JavaScript regex error')
        const secondError = new Error('second JavaScript regex error')
        mocks.state.workerAvailable = true
        mocks.state.nativeFailure = Object.assign(new Error('native transport failed'), {
            partialData: 'must-not-be-published',
        })
        mocks.state.workerData = 'javascript-authority-output'
        mocks.state.workerErrors = [
            { sourceIndex: 7, error: firstError },
            { sourceIndex: 3, error: secondError },
        ]
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})

        const result = await processScriptFull(
            character,
            'a cat here',
            'editoutput',
            -1,
            {},
            { cache: 'bypass' },
        )

        expect(mocks.state.nativeInput).toBe('a cat here')
        expect(mocks.state.workerInput).toBe('a cat here')
        expect(result.data).toBe('javascript-authority-output')
        expect(errorLog.mock.calls.slice(-2)).toEqual([[firstError], [secondError]])
        errorLog.mockRestore()
    })

    it('does not fall back after native cancellation', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        const controller = new AbortController()
        const cancelled = new Error('cancel native regex')
        controller.abort(cancelled)
        mocks.state.workerAvailable = true
        mocks.state.nativeFailure = cancelled

        await expect(processScriptFull(
            character,
            'a cat here',
            'editoutput',
            -1,
            {},
            { cache: 'bypass', signal: controller.signal },
        )).rejects.toBe(cancelled)
        expect(mocks.state.workerCalls).toBe(0)
    })

    it('keeps the UI thread when the caller opts out of the Worker', async () => {
        const character = makeCharacter([makeScript('cat', 'dog')])
        mocks.state.workerAvailable = true

        const result = await processScriptFull(
            character,
            'a cat here',
            'editoutput',
            -1,
            {},
            { cache: 'bypass', regexWorker: false },
        )

        expect(mocks.state.workerCalls).toBe(0)
        expect(mocks.state.nativeCalls).toBe(0)
        expect(result.data).toBe('a dog here')
    })

    it('does not offload non-editoutput modes', async () => {
        const character = makeCharacter([{ ...makeScript('cat', 'dog'), type: 'editinput' }])
        mocks.state.workerAvailable = true

        const result = await processScriptFull(
            character,
            'a cat here',
            'editinput',
            -1,
            {},
            { cache: 'bypass' },
        )

        expect(mocks.state.workerCalls).toBe(0)
        expect(result.data).toBe('a dog here')
    })

    it('replaces sticky-flag matches at position 0 on repeated executions', async () => {
        const character = makeCharacter([
            makeScript('foo', 'X', 'y<no_end_nl>'),
            makeScript('never-matches', '@@emo happy'),
        ])

        const first = await processScriptFull(character, 'foofoo', 'editoutput', -1, {}, { cache: 'bypass' })
        const second = await processScriptFull(character, 'foofoo', 'editoutput', -1, {}, { cache: 'bypass' })

        expect(first.data).toBe('Xfoo')
        expect(second.data).toBe('Xfoo')
    })

    it('keeps no more than 1,000 completed results', async () => {
        const character = makeCharacter([makeScript('^', '@@emo happy')])

        for (let index = 0; index <= 1_000; index++) {
            await processScriptFull(character, `result-${index}`, 'editoutput')
        }

        expect((await processScriptFull(character, 'result-0', 'editoutput')).emoChanged).toBe(true)
        expect((await processScriptFull(character, 'result-1000', 'editoutput')).emoChanged).toBe(false)
    })

    it('clears retained results when switching to the lower low-spec budget', async () => {
        const character = makeCharacter([makeScript('^', '@@emo happy')])

        await processScriptFull(character, 'retained-before-profile-change', 'editoutput')
        expect((await processScriptFull(character, 'retained-before-profile-change', 'editoutput')).emoChanged).toBe(false)

        setRuntimePerformanceProfile('low-spec')

        expect((await processScriptFull(character, 'retained-before-profile-change', 'editoutput')).emoChanged).toBe(true)
    })

    it('does not retain a completed result larger than the byte budget', async () => {
        const character = makeCharacter([makeScript('^', '@@emo happy')])
        const oversized = 'a'.repeat(2_100_000)

        expect((await processScriptFull(character, oversized, 'editoutput')).emoChanged).toBe(true)
        expect((await processScriptFull(character, oversized, 'editoutput')).emoChanged).toBe(true)
    })
})

describe('history-sensitive regex conversation operations', () => {
    beforeEach(() => {
        resetScriptCache()
        mocks.state.currentChat = null
        mocks.state.session = null
        mocks.state.selectedCharIndex = 0
        mocks.state.onHypaAddText = null
        mocks.database.dynamicAssets = false
        mocks.database.characters = [] as never[]
        scriptingsMocks.runLuaEditTrigger.mockClear()
        for (const callbacks of Object.values(mocks.pluginV2)) callbacks.clear()
    })

    it('runs a mutating regex after a plugin saves conversation metadata', async () => {
        const chat = {
            id: 'regex-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message' }],
        } as Chat
        const char = makeCharacter([makeScript('x', '@@inject')])
        char.chaId = 'regex-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id,
            conversation: chat,
            storeRevision: 21,
        })
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        mocks.pluginV2.editoutput.add(async (data) => {
            const next = structuredClone(chat)
            next.scriptstate = { $plugin: 'saved' }
            expect(session.adoptPersistedMetadata(next, 22)).toBe(true)
            return data
        })
        const result = await processScriptFull(
            char,
            'x',
            'editoutput',
            0,
            {},
            { cache: 'bypass', regexWorker: false },
        )
        expect(result.data).toBe('')
        expect(chat.message[0].data).toBe('x')
        expect(chat.scriptstate).toEqual({ $plugin: 'saved' })
        expect(session.activePinReasons).toEqual([])
    })

    it('applies @@inject through the active session batch instead of direct DB mutation', async () => {
        const chat = {
            id: 'regex-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('x', '@@inject')])
        char.chaId = 'regex-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: 'regex-chat',
            conversation: chat,
            storeRevision: 21,
        })
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session

        const result = await processScriptFull(char, 'x', 'editoutput', 0, {}, {
            cache: 'bypass',
            regexWorker: false,
        })

        expect(result.data).toBe('')
        expect(chat.message[0].data).toBe('x')
        expect(session.version).toBe(1)
        expect(session.activePinReasons).toEqual([])
    })

    it('does not clone history for a no-script or pure-regex cache-hit path', async () => {
        const messages = Array.from({ length: 256 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' : 'char',
            data: `message-${index}`,
            chatId: `message-${index}`,
        })) as Chat['message']
        const chat = { id: 'pure-chat', message: messages } as Chat
        const noScriptCharacter = makeCharacter([])
        noScriptCharacter.chaId = 'pure-character'
        noScriptCharacter.chats = [chat]
        noScriptCharacter.chatPage = 0
        const pureRegexCharacter = makeCharacter([makeScript('plain', 'result')])
        pureRegexCharacter.chaId = noScriptCharacter.chaId
        pureRegexCharacter.chats = [chat]
        pureRegexCharacter.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: noScriptCharacter.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 24,
        })
        mocks.database.characters = [noScriptCharacter] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        const structuredCloneSpy = vi.spyOn(globalThis, 'structuredClone')

        try {
            await processScriptFull(noScriptCharacter, 'plain', 'editoutput', -1, {}, {
                regexWorker: false,
            })
            await processScriptFull(pureRegexCharacter, 'plain', 'editoutput', -1, {}, {
                regexWorker: false,
            })
            structuredCloneSpy.mockClear()

            const cached = await processScriptFull(
                pureRegexCharacter,
                'plain',
                'editoutput',
                -1,
                {},
                { regexWorker: false },
            )

            expect(cached.data).toBe('result')
            expect(structuredCloneSpy).not.toHaveBeenCalled()
            expect(session.version).toBe(0)
            expect(session.activePinReasons).toEqual([])
        } finally {
            structuredCloneSpy.mockRestore()
        }
    })

    it.each(['rollp', 'rollpick'])(
        'guards the read-only history-sensitive %s CBS helper without cloning history',
        async (helper) => {
            const chat = {
                id: `history-${helper}-chat`,
                message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
            } as Chat
            const char = makeCharacter([])
            char.chaId = `history-${helper}-character`
            char.chats = [chat]
            char.chatPage = 0
            const session = new ActiveConversationSession({
                characterId: char.chaId,
                conversationId: chat.id!,
                conversation: chat,
                storeRevision: 27,
            })
            mocks.database.characters = [char] as never
            mocks.state.currentChat = chat
            mocks.state.session = session
            const acquirePin = vi.spyOn(session, 'acquirePin')
            const structuredCloneSpy = vi.spyOn(globalThis, 'structuredClone')

            try {
                await processScriptFull(
                    char,
                    `{{${helper}::6}}`,
                    'editoutput',
                    -1,
                    {},
                    { cache: 'bypass', regexWorker: false },
                )

                expect(structuredCloneSpy).not.toHaveBeenCalled()
                expect(acquirePin).toHaveBeenCalledWith('compatibility')
                expect(session.activePinReasons).toEqual([])
            } finally {
                structuredCloneSpy.mockRestore()
            }
        },
    )

    it('does not treat CBS-looking text in a static regex pattern as history-sensitive', async () => {
        const chat = {
            id: 'static-cbs-pattern-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('{{history}}', 'replacement')])
        char.chaId = 'static-cbs-pattern-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 28,
        })
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        const structuredCloneSpy = vi.spyOn(globalThis, 'structuredClone')

        try {
            await processScriptFull(char, 'plain', 'editoutput', -1, {}, {
                cache: 'bypass',
                regexWorker: false,
            })

            expect(structuredCloneSpy).not.toHaveBeenCalled()
            expect(session.version).toBe(0)
            expect(session.activePinReasons).toEqual([])
        } finally {
            structuredCloneSpy.mockRestore()
        }
    })

    it('classifies CBS helpers in dynamic regex patterns', async () => {
        const chat = {
            id: 'dynamic-cbs-pattern-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([
            makeScript('{{history}}', 'replacement', 'g<cbs>'),
        ])
        char.chaId = 'dynamic-cbs-pattern-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 29,
        })
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        const structuredCloneSpy = vi.spyOn(globalThis, 'structuredClone')

        try {
            await processScriptFull(char, 'plain', 'editoutput', -1, {}, {
                cache: 'bypass',
                regexWorker: false,
            })

            expect(structuredCloneSpy).not.toHaveBeenCalled()
            expect(session.activePinReasons).toEqual([])
        } finally {
            structuredCloneSpy.mockRestore()
        }
    })

    it('pins an unsupported plugin callback to the explicit full-array compatibility path', async () => {
        const chat = {
            id: 'plugin-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('input-plugin', '@@inject')])
        char.chaId = 'plugin-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: 'plugin-chat',
            conversation: chat,
            storeRevision: 22,
        })
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        let observedCompatibilityPins = 0
        mocks.pluginV2.editoutput.add(async (data) => {
            observedCompatibilityPins = session.pinCount('compatibility')
            return `${data}-plugin`
        })

        const result = await processScriptFull(char, 'input', 'editoutput', 0, {}, {
            cache: 'bypass',
            regexWorker: false,
        })

        expect(result.data).toBe('')
        expect(observedCompatibilityPins).toBe(1)
        expect(session.activePinReasons).toEqual([])
        expect(chat.message[0].data).toBe('input-plugin')
        expect(session.version).toBe(1)
    })

    it('stops plugin and history processing when cancellation occurs in a plugin callback', async () => {
        const chat = {
            id: 'cancelled-plugin-chat',
            message: [{ role: 'user', data: 'original', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('input-plugin', '@@inject')])
        char.chaId = 'cancelled-plugin-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 32,
        })
        const controller = new AbortController()
        let secondPluginCalls = 0
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        mocks.pluginV2.editoutput.add(async (data) => {
            controller.abort()
            return `${data}-plugin`
        })
        mocks.pluginV2.editoutput.add(async (data) => {
            secondPluginCalls++
            return data
        })

        await expect(
            processScriptFull(
                char,
                'input',
                'editoutput',
                0,
                {},
                {
                    cache: 'bypass',
                    regexWorker: false,
                    signal: controller.signal,
                },
            ),
        ).rejects.toThrow(/abort/i)

        expect(secondPluginCalls).toBe(0)
        expect(chat.message[0].data).toBe('original')
        expect(session.version).toBe(0)
        expect(session.activePinReasons).toEqual([])
    })

    it('does not invoke Lua or plugins for a pre-aborted call', async () => {
        const character = makeCharacter([])
        const plugin = vi.fn(async (data: string) => data)
        const controller = new AbortController()
        mocks.pluginV2.editoutput.add(plugin)
        controller.abort()

        await expect(
            processScriptFull(
                character,
                'input',
                'editoutput',
                -1,
                {},
                {
                    cache: 'bypass',
                    regexWorker: false,
                    signal: controller.signal,
                },
            ),
        ).rejects.toThrow(/abort/i)

        expect(scriptingsMocks.runLuaEditTrigger).not.toHaveBeenCalled()
        expect(plugin).not.toHaveBeenCalled()
    })

    it('keeps a mutating Plugin v2 prompt scope off the transaction path', async () => {
        const chat = {
            id: 'prompt-plugin-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('input-plugin', '@@inject')])
        char.chaId = 'prompt-plugin-character'
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 30,
        })
        const acquirePin = vi.spyOn(session, 'acquirePin')
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        mocks.pluginV2.editoutput.add(async (data) => `${data}-plugin`)
        const scope = createPromptScriptOperationScope(char, {
            pluginCompatibility: true,
        })

        try {
            const result = await processScriptFull(char, 'input', 'editoutput', 0, {}, {
                cache: 'bypass',
                regexWorker: false,
                promptOperationScope: scope,
            })
            scope.finish()

            expect(result.data).toBe('')
            expect(chat.message[0].data).toBe('input-plugin')
            expect(session.version).toBe(0)
            expect(acquirePin.mock.calls.filter(([reason]) => reason === 'compatibility'))
                .toHaveLength(1)
            expect(acquirePin.mock.calls.filter(([reason]) => reason === 'transaction'))
                .toHaveLength(0)
            expect(session.activePinReasons).toEqual([])
        } finally {
            scope.release()
        }
    })

    it('refuses to capture a prompt scope for a character that is no longer selected', () => {
        const originalChat = {
            id: 'prompt-original-chat',
            message: [{ role: 'user', data: 'original', chatId: 'original-message' }],
        } as Chat
        const replacementChat = {
            id: 'prompt-replacement-chat',
            message: [{ role: 'user', data: 'replacement', chatId: 'replacement-message' }],
        } as Chat
        const originalCharacter = makeCharacter([])
        originalCharacter.chaId = 'prompt-original-character'
        originalCharacter.chats = [originalChat]
        originalCharacter.chatPage = 0
        const replacementCharacter = makeCharacter([])
        replacementCharacter.chaId = 'prompt-replacement-character'
        replacementCharacter.chats = [replacementChat]
        replacementCharacter.chatPage = 0
        const replacementSession = new ActiveConversationSession({
            characterId: replacementCharacter.chaId,
            conversationId: replacementChat.id!,
            conversation: replacementChat,
            storeRevision: 31,
        })
        mocks.database.characters = [originalCharacter, replacementCharacter] as never
        mocks.state.selectedCharIndex = 1
        mocks.state.currentChat = replacementChat
        mocks.state.session = replacementSession

        expect(() => createPromptScriptOperationScope(originalCharacter)).toThrow(
            /inactive|ownership/i,
        )
        expect(replacementSession.activePinReasons).toEqual([])
    })

    it('does not retarget a post-plugin history action after navigation', async () => {
        const originalChat = {
            id: 'plugin-original-chat',
            message: [{ role: 'user', data: 'original', chatId: 'original-message' }],
        } as Chat
        const replacementChat = {
            id: 'plugin-replacement-chat',
            message: [{ role: 'user', data: 'replacement', chatId: 'replacement-message' }],
        } as Chat
        const originalCharacter = makeCharacter([makeScript('input-plugin', '@@inject')])
        originalCharacter.chaId = 'plugin-original-character'
        originalCharacter.chats = [originalChat]
        originalCharacter.chatPage = 0
        const replacementCharacter = makeCharacter([])
        replacementCharacter.chaId = 'plugin-replacement-character'
        replacementCharacter.chats = [replacementChat]
        replacementCharacter.chatPage = 0
        const originalSession = new ActiveConversationSession({
            characterId: originalCharacter.chaId,
            conversationId: originalChat.id!,
            conversation: originalChat,
            storeRevision: 25,
        })
        const replacementSession = new ActiveConversationSession({
            characterId: replacementCharacter.chaId,
            conversationId: replacementChat.id!,
            conversation: replacementChat,
            storeRevision: 26,
        })
        let releasePlugin: (() => void) | null = null
        mocks.pluginV2.editoutput.add(async (data) => {
            await new Promise<void>((resolve) => {
                releasePlugin = resolve
            })
            return `${data}-plugin`
        })
        mocks.database.characters = [originalCharacter, replacementCharacter] as never
        mocks.state.currentChat = originalChat
        mocks.state.session = originalSession

        const pending = processScriptFull(
            originalCharacter,
            'input',
            'editoutput',
            0,
            {},
            { cache: 'bypass', regexWorker: false },
        )
        await vi.waitFor(() => expect(releasePlugin).not.toBeNull())
        mocks.state.selectedCharIndex = 1
        mocks.state.currentChat = replacementChat
        mocks.state.session = replacementSession
        releasePlugin!()

        await expect(pending).rejects.toThrow(/inactive|ownership/i)
        expect(originalChat.message[0].data).toBe('original')
        expect(replacementChat.message[0].data).toBe('replacement')
        expect(originalSession.activePinReasons).toEqual([])
        expect(replacementSession.activePinReasons).toEqual([])
    })

    it('commits an eager inject mutation before a later processing error', async () => {
        const chat = {
            id: 'regex-partial-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('x', '@@inject')])
        char.chaId = 'regex-partial-character'
        char.chats = [chat]
        char.chatPage = 0
        char.additionalAssets = [['asset', 'source', '']]
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 23,
        })
        mocks.database.dynamicAssets = true
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session

        await expect(processScriptFull(char, 'x', 'editoutput', 0, {}, {
            cache: 'bypass',
            regexWorker: false,
        })).rejects.toThrow(/addText/)

        expect(chat.message[0].data).toBe('x')
        expect(session.version).toBe(1)
        expect(session.activePinReasons).toEqual([])
    })

    it('discards a staged inject mutation when later processing is cancelled', async () => {
        const chat = {
            id: 'regex-cancelled-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('x', '@@inject')])
        char.chaId = 'regex-cancelled-character'
        char.chats = [chat]
        char.chatPage = 0
        char.additionalAssets = [['asset', 'source', '']]
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 33,
        })
        const controller = new AbortController()
        mocks.database.dynamicAssets = true
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        mocks.state.onHypaAddText = () => controller.abort()

        await expect(
            processScriptFull(
                char,
                'x',
                'editoutput',
                0,
                {},
                {
                    cache: 'bypass',
                    regexWorker: false,
                    signal: controller.signal,
                },
            ),
        ).rejects.toThrow(/addText/)

        expect(chat.message[0].data).toBe('before')
        expect(session.version).toBe(0)
        expect(session.activePinReasons).toEqual([])
    })

    it('propagates the original processing error when the error-path commit is stale', async () => {
        const chat = {
            id: 'regex-masking-chat',
            message: [{ role: 'user', data: 'before', chatId: 'message-0' }],
        } as Chat
        const char = makeCharacter([makeScript('x', '@@inject')])
        char.chaId = 'regex-masking-character'
        char.chats = [chat]
        char.chatPage = 0
        char.additionalAssets = [['asset', 'source', '']]
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 24,
        })
        mocks.database.dynamicAssets = true
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        mocks.state.onHypaAddText = () => {
            session.append({ role: 'user', data: 'concurrent', chatId: 'message-1' })
        }
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})

        await expect(processScriptFull(char, 'x', 'editoutput', 0, {}, {
            cache: 'bypass',
            regexWorker: false,
        })).rejects.toThrow(/addText/)

        expect(errorLog.mock.calls.flat().some(
            (logged) => logged instanceof ConversationSessionStaleError,
        )).toBe(true)
        expect(session.activePinReasons).toEqual([])
        errorLog.mockRestore()
    })
})


describe('live display script ordering', () => {
    function fixture() {
        resetScriptCache()
        for (const callbacks of Object.values(mocks.pluginV2)) callbacks.clear()
        const chat = {
            id: 'display-order',
            message: [{ role: 'char', data: 'x', chatId: 'row' }],
        } as Chat
        const char = makeCharacter([
            { ...makeScript('x', '{{getvar::scratch}}'), type: 'editdisplay' },
        ])
        char.chats = [chat]
        char.chatPage = 0
        const session = new ActiveConversationSession({
            characterId: char.chaId,
            conversationId: chat.id!,
            conversation: chat,
            storeRevision: 1,
        })
        mocks.database.characters = [char] as never
        mocks.state.currentChat = chat
        mocks.state.session = session
        mocks.state.selectedCharIndex = 0
        mocks.database.dynamicAssets = false
        vi.mocked(getCurrentCharacter).mockReturnValue(char)
        scriptingsMocks.runLuaEditTrigger.mockImplementation(
            async (_char, _mode, value) => {
                session.applyOperation({
                    expectedVersion: session.version,
                    expectedMetadata: cloneConversationMetadata(chat),
                    metadata: {
                        ...cloneConversationMetadata(chat),
                        scriptstate: { $scratch: value },
                    },
                    origin: 'display',
                })
                return value
            },
        )
        let release!: () => void
        const gate = new Promise<void>((resolve) => {
            release = resolve
        })
        const plugin = vi.fn(async (value: string) => {
            if (value === 'first') await gate
            return value
        })
        mocks.pluginV2.editdisplay.add(plugin)
        const run = (value: string, signal?: AbortSignal) =>
            processScriptFull(
                char,
                value,
                'editdisplay',
                0,
                {},
                { cache: 'bypass', regexWorker: false, signal },
            )
        const cleanup = () => {
            release()
            scriptingsMocks.runLuaEditTrigger.mockImplementation(
                async (_char, _mode, value) => value,
            )
            mocks.pluginV2.editdisplay.clear()
        }
        return { chat, char, session, run, plugin, release, cleanup }
    }

    it('finishes plugins and regex before the next row changes Lua variables', async () => {
        const f = fixture()
        try {
            const first = f.run('first')
            await vi.waitFor(() => expect(f.plugin).toHaveBeenCalledOnce())
            const second = f.run('second')
            const results = Promise.allSettled([first, second])
            await new Promise((resolve) => setTimeout(resolve, 0))
            f.release()
            expect(await results).toEqual([
                {
                    status: 'fulfilled',
                    value: { data: 'first', emoChanged: false },
                },
                {
                    status: 'fulfilled',
                    value: { data: 'second', emoChanged: false },
                },
            ])
            expect(f.chat.scriptstate).toEqual({ $scratch: 'second' })
            expect(f.session.activePinReasons).toEqual([])
        } finally {
            f.cleanup()
        }
    })

    it('skips a cancelled waiting row before it runs Lua or plugins', async () => {
        const f = fixture()
        try {
            const first = f.run('first')
            await vi.waitFor(() => expect(f.plugin).toHaveBeenCalledOnce())
            const controller = new AbortController()
            const second = f.run('cancelled', controller.signal)
            const results = Promise.allSettled([first, second])
            controller.abort()
            f.release()
            const completed = await results
            expect(completed[0].status).toBe('fulfilled')
            expect(completed[1]).toMatchObject({
                status: 'rejected',
                reason: { name: 'AbortError' },
            })
            expect(f.chat.scriptstate).toEqual({ $scratch: 'first' })
            expect(f.plugin).toHaveBeenCalledOnce()
            expect(f.session.activePinReasons).toEqual([])
        } finally {
            f.cleanup()
        }
    })
    it('rejects waiting work after an external edit and releases the queue for a fresh render', async () => {
        const f = fixture()
        try {
            const first = f.run('first')
            await vi.waitFor(() => expect(f.plugin).toHaveBeenCalledOnce())
            const second = f.run('obsolete')
            const results = Promise.allSettled([first, second])
            f.session.edit(f.session.locate(0), {
                ...f.chat.message[0],
                data: 'external edit',
            })
            f.release()
            const completed = await results
            expect(completed).toEqual([
                {
                    status: 'rejected',
                    reason: expect.any(ConversationSessionStaleError),
                },
                {
                    status: 'rejected',
                    reason: expect.any(ConversationSessionStaleError),
                },
            ])
            expect(f.plugin).toHaveBeenCalledOnce()
            expect(f.chat.scriptstate).toEqual({ $scratch: 'first' })
            expect((await f.run('fresh')).data).toBe('fresh')
            expect(f.chat.message[0].data).toBe('external edit')
            expect(f.session.activePinReasons).toEqual([])
        } finally {
            f.cleanup()
        }
    })

    it('does not run waiting work after navigation', async () => {
        const f = fixture()
        try {
            const first = f.run('first')
            await vi.waitFor(() => expect(f.plugin).toHaveBeenCalledOnce())
            const second = f.run('wrong conversation')
            const results = Promise.allSettled([first, second])
            mocks.state.session = null
            mocks.state.currentChat = null
            f.release()
            const completed = await results
            expect(
                completed.every((result) => result.status === 'rejected'),
            ).toBe(true)
            expect(f.plugin).toHaveBeenCalledOnce()
            expect(f.chat.scriptstate).toEqual({ $scratch: 'first' })
            expect(f.session.activePinReasons).toEqual([])
        } finally {
            f.cleanup()
        }
    })

    it('keeps frozen parsing independent from a busy live display pipeline', async () => {
        const f = fixture()
        try {
            const first = f.run('first')
            await vi.waitFor(() => expect(f.plugin).toHaveBeenCalledOnce())
            const isolated = await processScriptFull(
                makeCharacter([]),
                'isolated',
                'editdisplay',
                0,
                {},
                {
                    cache: 'bypass',
                    regexWorker: false,
                    captureContext: {
                        presetRegex: [],
                        moduleRegexScripts: [],
                        moduleAssets: [],
                        dynamicAssets: false,
                        dynamicAssetsEditDisplay: false,
                        parserContext: {
                            database: mocks.database,
                            character: f.char,
                            selectedCharID: 0,
                            modules: [],
                            moduleLorebooks: [],
                            chatVariables: {},
                            globalChatVariables: {},
                        },
                    } as never,
                },
            )
            expect(isolated.data).toBe('isolated')
            expect(f.plugin).toHaveBeenCalledOnce()
            f.release()
            expect((await first).data).toBe('first')
        } finally {
            f.cleanup()
        }
    })
    it('persists regex display variables without invalidating the same display again', async () => {
        const f = fixture()
        try {
            f.char.customscript = [
                {
                    ...makeScript('^input$', '{{setvar::scratch::regex}}'),
                    type: 'editdisplay',
                },
            ]
            const events = vi.fn()
            f.session.subscribe(events)
            expect((await f.run('input')).data).toBe('written')
            expect(f.chat.scriptstate).toEqual({ $scratch: 'regex' })
            expect(events).toHaveBeenCalledTimes(2)
            expect(
                events.mock.calls.every(
                    ([event]) => event.displayVariableUpdate === true,
                ),
            ).toBe(true)
        } finally {
            f.cleanup()
        }
    })
})
