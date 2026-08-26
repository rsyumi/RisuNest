import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Chat, character, customscript } from '../storage/database.svelte'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { ActiveConversationSession } from '../storage/activeConversationSession'

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
        currentChat: null as Chat | null,
        session: null as ActiveConversationSession | null,
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

vi.mock('svelte/store', () => ({
    get: (store: unknown) => store === mocks.charEmotionStore ? mocks.state.emotions : 0,
}))
vi.mock('src/ts/stores.svelte', () => ({
    CharEmotion: mocks.charEmotionStore,
    selectedCharID: mocks.selectedCharStore,
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => mocks.database,
    getCurrentCharacter: vi.fn(),
    getCurrentChat: () => mocks.state.currentChat,
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
    risuChatParser: (data: string) => {
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
vi.mock('src/ts/process/memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('src/ts/process/scriptings', () => ({
    runLuaEditTrigger: async (_char: unknown, _mode: unknown, data: string) => data,
}))
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
            execute: async () => {
                mocks.state.workerCalls++
                if(mocks.state.workerFailure !== null){
                    throw mocks.state.workerFailure
                }
                return { data: mocks.state.workerData, errors: [] }
            },
        }),
    }
})

const { processScriptFull, resetScriptCache } = await import('./scripts')
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
        mocks.state.currentChat = null
        mocks.state.session = null
        mocks.database.dynamicAssets = false
        mocks.database.characters = [] as never[]
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
        mocks.database.dynamicAssets = false
        mocks.database.characters = [] as never[]
        for (const callbacks of Object.values(mocks.pluginV2)) callbacks.clear()
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
})
