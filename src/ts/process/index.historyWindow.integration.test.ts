// @vitest-environment node

import '../storage/tests/selectedConversationEvictionNodeDom.setup'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import { checkOutputProcessing, checkStreamingWrite, finalText, installOutputTrigger, liveMessage, setterCases, whileStreaming, type SetterCase } from './tests/streamingPluginWrites'
import { createProductionPluginDatabaseAccess } from '../plugins/pluginDatabaseAccess'
import { alertStore } from '../alertQueue'
import type { Chat, Database, Message, character } from '../storage/database.svelte'
import { DBState, selectedCharID, settingsOpen, ReloadChatPointer } from '../stores.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import { createConversationSummaryStubFromChat } from '../storage/conversationResidency'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
    publishPersistentConversationReplacementToWorkingSet,
    type PersistentDataRuntimeStateAdapter,
} from '../storage/persistentDataRuntime'

// The send runs against the production runtime, its windowed store and the real
// trigger, regex and Lua code. Only the provider, the tokenizer and the
// surfaces a send does not exercise here are stand-ins.
const current = vi.hoisted(() => ({
    runtime: null as unknown as ReturnType<typeof createPersistentDataRuntime>,
    store: null as unknown as IndexedDbPersistentDataStore,
    alertError: null as unknown as ReturnType<typeof vi.fn>,
    snapshots: ['answer'],
}))

vi.mock('../storage/persistentDataRuntime.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('../storage/persistentDataRuntime.svelte')>(),
    acknowledgeGenerationCompletion: (epoch?: number) => current.runtime.acknowledgeGenerationCompletion(epoch),
    drainDeferredLwwReceives: async () => undefined,
    assertPersistentMutationAllowed: (epoch?: number) => current.runtime.assertPersistentMutationAllowed(epoch),
    getPersistentStorageAuthorityEpoch: () => current.runtime.getStorageAuthorityEpoch(),
    getPersistentNavigationGeneration: () => current.runtime.getNavigationGeneration(),
    acquireCompleteConversation: (reason: string, target: never) => current.runtime.acquireCompleteConversation(reason, target),
    captureSelectedConversationAuthority: () => current.runtime.captureSelectedConversationAuthority(),
    recordSelectedCharacterLastInteraction: (authority: never, before: number | undefined, after: number) =>
        current.runtime.recordSelectedCharacterLastInteraction(authority, before, after),
    captureWindowedConversationMutationController: (target: never, chat: Chat, start: number) =>
        current.runtime.captureWindowedConversationMutationController(target, chat, start),
    captureSelectedConversationTarget: () => current.runtime.captureSelectedConversationTarget(),
    flushPendingData: (reason: string) => current.runtime.flushPendingData(reason),
    flushPendingDataLocally: (reason: string) => current.runtime.flushPendingDataLocally(reason),
    getActiveConversationSession: () => current.runtime.getActiveConversationSession(),
    getActiveConversationViewportSource: () => current.runtime.getActiveConversationViewportSource(),
    peekActiveConversationSession: () => current.runtime?.getActiveConversationSession() ?? null,
    invalidateActiveConversationSession: () => current.runtime.invalidateActiveConversationSession(),
    getPersistentDataRuntime: () => current.runtime,
    getPersistentRevision: () => current.runtime.revision,
    withLiveConversationWrite: <T>(reason: string, operation: () => Promise<T>) => current.runtime.withLiveConversationWrite(reason, operation),
    commitPreparedUnitIntent: (...args: Parameters<typeof current.runtime.commitPreparedUnitIntent>) => current.runtime.commitPreparedUnitIntent(...args),
}))
vi.mock('../storage/persistentDataStoreFactory', async () => (await import('./tests/sendChatTestHarness')).persistentDataStoreFactoryModule(() => current.store))
vi.mock('../storage/deviceSettings', async (importOriginal) => (await import('./tests/sendChatTestHarness')).historyWindowDeviceSettingsModule(importOriginal, () => true))
vi.mock('../iosNative', async () => (await import('./tests/sendChatTestHarness')).iosNativeModule())
vi.mock('../tokenizer', async () => (await import('./tests/sendChatTestHarness')).tokenizerModule({
    // Every message costs ten tokens in the window walk.
    tokenize: vi.fn(async () => 10),
    encodeWithTokenizer: vi.fn(async () => new Array(10).fill(0)),
}))
vi.mock('../alert', async () => {
    const harness = await import('./tests/sendChatTestHarness')
    current.alertError = vi.fn()
    return harness.alertModule({ alertError: current.alertError, alertInput: vi.fn(), alertSelect: vi.fn(), alertConfirm: vi.fn() })
})
vi.mock('./request/request', async () => (await import('./tests/sendChatTestHarness')).requestModule(vi.fn(async () => ({
    type: 'streaming',
    result: new ReadableStream<Record<string, string>>({
        start(controller) {
            for (const response of current.snapshots) controller.enqueue({ response })
            controller.close()
        },
    }),
}))))
vi.mock('../parser/chatML', async () => (await import('./tests/sendChatTestHarness')).chatMLModule())
vi.mock('../parser/parser.svelte', async (importOriginal) => ({
    ...(await import('src/ts/parser/parser.testSupport')).parserStubModule(),
    risuChatParser: (await importOriginal<typeof import('../parser/parser.svelte')>()).risuChatParser,
}))
vi.mock('./lorebook.svelte', async () => (await import('./tests/sendChatTestHarness')).lorebookModule())
vi.mock('./stableDiff', async () => (await import('./tests/sendChatTestHarness')).stableDiffModule({ generateAIImage: vi.fn() }))
vi.mock('./templates/templates', async () => (await import('./tests/sendChatTestHarness')).templatesModule())
vi.mock('./exampleMessages', async () => (await import('./tests/sendChatTestHarness')).exampleMessagesModule())
vi.mock('./tts', async () => (await import('./tests/sendChatTestHarness')).ttsModule())
vi.mock('./memory/supaMemory', async () => (await import('./tests/sendChatTestHarness')).supaMemoryModule())
vi.mock('./memory/hypamemory', async () => (await import('./tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('./embedding/addinfo', async () => (await import('./tests/sendChatTestHarness')).addinfoModule())
vi.mock('./files/inlays', async () => (await import('./tests/sendChatTestHarness')).inlaysModule({ writeInlayImage: vi.fn() }))
vi.mock('./models/modelString', async () => (await import('./tests/sendChatTestHarness')).modelStringModule())
vi.mock('./inlayScreen', async () => (await import('./tests/sendChatTestHarness')).inlayScreenModule())
vi.mock('./transformers', async () => (await import('./tests/sendChatTestHarness')).transformersModule())
vi.mock('./memory/hanuraiMemory', async () => (await import('./tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('./memory/hypav2', async () => (await import('./tests/sendChatTestHarness')).hypav2Module())
vi.mock('./memory/hypav3', async () => (await import('./tests/sendChatTestHarness')).hypav3Module())
vi.mock('../model/modellist', async () => (await import('./tests/sendChatTestHarness')).modellistModule())
vi.mock('./modules', async (importOriginal) => ({
    ...await importOriginal<typeof import('./modules')>(),
    ...(await import('./tests/sendChatTestHarness')).modulesModule(),
}))
vi.mock('../globalApi.svelte', async () => (await import('./tests/sendChatTestHarness')).globalApiModule({
    fetchNative: vi.fn(),
    downloadFile: vi.fn(),
}))
vi.mock('../plugins/plugins.svelte', async () => (await import('./tests/sendChatTestHarness')).pluginsModule())
vi.mock('./presetChain', async () => (await import('./tests/sendChatTestHarness')).presetChainModule())
vi.mock('./command', async () => (await import('./tests/sendChatTestHarness')).commandModule())

const MESSAGES = 1_000
const WINDOW_START = 900

function storedMessages(): Message[] {
    return Array.from({ length: MESSAGES }, (_, index): Message => ({
        role: index % 2 ? 'char' : 'user',
        data: index === 100 ? 'WRITE:outside' : index === 950 ? 'WRITE:inside' : `m${index}`,
        chatId: `id-${index}`,
        saying: index % 2 ? 'character-a' : undefined,
        time: 1_800_000_000_000 + index,
    }))
}

function syntheticDatabase(messages: Message[]): Database {
    const owner = {
        type: 'character',
        chaId: 'character-a',
        name: 'Synthetic character',
        image: '',
        firstMessage: 'greeting',
        alternateGreetings: [],
        desc: '',
        personality: '',
        scenario: '',
        notes: '',
        bias: [],
        additionalAssets: [],
        emotionImages: [],
        globalLore: [],
        chatFolders: [],
        reloadKeys: 0,
        viewScreen: 'none',
        inlayViewScreen: false,
        supaMemory: false,
        utilityBot: false,
        defaultVariables: '',
        chatPage: 0,
        customscript: [{
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
        }],
        triggerscript: [{
            comment: '',
            type: 'start',
            conditions: [],
            effect: [{
                type: 'triggerlua',
                code: `
                    function onStart(id)
                        insertChat(id, 960, 'char', 'lua inserted')
                    end
                `,
            }],
        }],
        chats: [{ id: 'chat-a', name: 'Synthetic chat', note: '', localLore: [], fmIndex: -1, message: messages }],
    } as unknown as character
    return {
        username: 'Synthetic user',
        characters: [owner],
        botPresets: [],
        botPresetsId: 0,
        plugins: [],
        pluginCustomStorage: {},
        statics: { messages: 0 },
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
        outputImageModal: false,
        rememberToolUsage: false,
    } as unknown as Database
}

async function bootWindowedConversation(
    messages: Message[],
    configure?: (seed: Database) => void,
    configureWorkingSet?: (db: Database) => void,
) {
    const { resetScriptCache } = await import('./scripts')
    resetScriptCache()
    const seed = syntheticDatabase(messages)
    configure?.(seed)
    const name = `history-window-send-${crypto.randomUUID()}`
    const factory = new IDBFactory()
    const store = new IndexedDbPersistentDataStore(
        name,
        factory,
        IDBKeyRange,
    )
    await store.open()
    await store.replaceFromDatabase(structuredClone(seed))
    configureWorkingSet?.(seed)
    // The working set lives in DBState, as in the app.
    DBState.db = seed
    selectedCharID.set(0)
    const selected = () => DBState.db.characters[0]
    const state: PersistentDataRuntimeStateAdapter = {
        captureWorkingSetDatabase: () => DBState.db,
        captureRoot: () => capturePersistentRoot(DBState.db),
        captureSelectedCharacter: () => selected() ?? null,
        captureCharacter: (id) => DBState.db.characters.find((candidate) => candidate.chaId === id) ?? null,
        getSelectedCharacterId: () => selected()?.chaId,
        getSelectedConversationId: () => selected()?.chats[selected().chatPage ?? 0]?.id,
        replaceDatabase: (next) => { DBState.db = next },
        publishCharacter: (next) => { DBState.db.characters[0] = next },
        publishConversation: (_characterId, conversation, nextCharacter) => {
            if (nextCharacter) DBState.db.characters[0] = nextCharacter
            else DBState.db.characters[0].chats[DBState.db.characters[0].chatPage ?? 0] = conversation
        },
        publishConversationReplacement: (result) => {
            publishPersistentConversationReplacementToWorkingSet(DBState.db, result)
        },
        canUseWindowedSelectedConversation: () => true,
        isConversationOperationActive: () => false,
        conversationViewportRowBudget: 64,
    }
    const backgroundErrors: unknown[] = []
    current.store = store
    current.runtime = createPersistentDataRuntime({
        store,
        state,
        onBackgroundError: (error) => { backgroundErrors.push(error) },
        prepareDatabase: async (candidate) => candidate,
    })
    await current.runtime.initializeActiveWorkingSet(DBState.db)
    await vi.waitFor(
        () => expect(current.runtime.getSelectedConversationMode()).toBe('windowed'),
        { timeout: 5_000 },
    )
    async function connect() {
        const reopened = new IndexedDbPersistentDataStore(name, factory, IDBKeyRange)
        await reopened.open()
        return reopened
    }
    return { store, backgroundErrors, connect, async reopen() {
        return (await (await connect()).readConversation('character-a', 'chat-a'))!.value
    } }
}

let restoreFetch: (() => void) | null = null

beforeAll(async () => {
    const jsonLuaSource = await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8')
    const fetch = globalThis.fetch
    globalThis.fetch = vi.fn(async () => new Response(jsonLuaSource, { status: 200 })) as typeof globalThis.fetch
    restoreFetch = () => { globalThis.fetch = fetch }
    // The Lua runtime loads its WebAssembly from disk only when no browser globals are present,
    // so its factory is created once here before the send runs with the DOM in place.
    const { runScripted } = await import('./scriptings')
    DBState.db = syntheticDatabase([])
    selectedCharID.set(0)
    const browserGlobals = new Map(
        ['window', 'document', 'navigator', 'location'].map((key) =>
            [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const),
    )
    try {
        for (const key of browserGlobals.keys()) Reflect.deleteProperty(globalThis, key)
        await runScripted(`listenEdit('editInput', function(id, value, meta) return value end)`, {
            char: { chaId: 'lua-warm-up' } as never,
            chat: { message: [] } as never,
            data: 'input',
            mode: 'editInput',
        })
    } finally {
        for (const [key, descriptor] of browserGlobals) {
            if (descriptor) Object.defineProperty(globalThis, key, descriptor)
        }
    }
})

afterAll(() => {
    restoreFetch?.()
})

describe('a windowed send through the production runtime', () => {
    it.each(['direct', 'expanded'])('uses a shared complete lease for %s full-history background CBS', async (kind) => {
        await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
            seed.characters[0].chats[0].scriptstate = { $expanded: '{{history}}' }
        })
        const { acquireLiveDisplayParserLease } = await import('../liveDisplayParserLease')
        const { risuChatParser } = await import('../parser/parser.svelte')
        const shared = vi.spyOn(current.runtime, 'acquireCompleteConversation')
        const controller = new AbortController()
        const lease = await acquireLiveDisplayParserLease({
            source: kind === 'direct' ? '{{history}}' : '{{getvar::expanded}}',
            character: DBState.db.characters[0], signal: controller.signal, parseFirst: true,
        })
        try {
            expect(shared).toHaveBeenCalledOnce()
            if (kind === 'expanded') {
                expect(lease?.projection?.parsedSource).toBe('{{history}}')
                const parser = lease!.projection!.context.parserContext
                const result = risuChatParser(lease!.projection!.parsedSource!, {
                    ...parser, db: parser.database, chara: parser.character,
                })
                expect(result).toContain('m0')
                expect(parser.historyAccess!.row(999)?.data).toBe('m999')
                expect(result).toBe(risuChatParser('{{history}}', { db: DBState.db, chara: DBState.db.characters[0] }))
                parser.historyAccess!.assertComplete()
            }
        } finally { lease?.release(); controller.abort(); shared.mockRestore() }
    })
    it('orders concurrent parser variable increments before reentrant script callbacks', async () => {
        const { reopen, backgroundErrors } = await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
            seed.characters[0].chats[0].scriptstate = { $counter: '0' }
        })
        const { parseRisuChat } = await import('../plugins/parseRisuChat')
        const { pluginV2 } = await import('../plugins/plugins.svelte')
        const increment = '{{setvar::counter::{{calc::{{getvar::counter}}+1}}}}{{getvar::counter}}'
        expect(await Promise.all([
            parseRisuChat(increment, { runVar: true }),
            parseRisuChat(increment, { runVar: true }),
            parseRisuChat('{{getvar::counter}}'),
        ])).toEqual(['1', '2', '2'])
        const hook = async (data: string) => `${data}:${await parseRisuChat(increment, { runVar: true })}`
        pluginV2.editprocess.add(hook)
        try {
            expect(await parseRisuChat(increment, { runVar: true, processRegex: true })).toBe('3:4')
        } finally { pluginV2.editprocess.delete(hook) }
        expect((await reopen()).scriptstate.$counter).toBe('4')
        expect(backgroundErrors).toEqual([])
    })
    it.each([false, true])('aborts a queued parser call promptly without letting a later write overtake its predecessor (history: %s)', async (needsHistory) => {
        const { store, reopen, backgroundErrors } = await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
            seed.characters[0].chats[0].scriptstate = { $counter: '0' }
        })
        const { parseRisuChat } = await import('../plugins/parseRisuChat')
        let entered!: () => void
        const reading = new Promise<void>((resolve) => { entered = resolve })
        let release!: () => void
        const held = new Promise<void>((resolve) => { release = resolve })
        const acquire = store.acquireRevision.bind(store)
        const spy = vi.spyOn(store, 'acquireRevision').mockImplementationOnce(async (revision) => {
            const lease = await acquire(revision)
            const read = lease.readConversationWindow.bind(lease)
            lease.readConversationWindow = async (query) => {
                entered()
                await held
                return read(query)
            }
            return lease
        })
        const increment = '{{setvar::counter::{{calc::{{getvar::counter}}+1}}}}{{getvar::counter}}'
        if (needsHistory) spy.mockImplementationOnce(async (revision) => {
            await held
            return acquire(revision)
        })
        const first = parseRisuChat('{{previouschatlog::0}}|' + increment, { runVar: true })
        const controller = new AbortController()
        let queued: Promise<string> | undefined
        let later: Promise<string> | undefined
        let laterSettled = false
        try {
            await reading
            queued = parseRisuChat((needsHistory ? '{{previouschatlog::0}}|' : '') + increment, { runVar: true }, controller.signal)
            let rejection: unknown
            void queued.catch((error) => { rejection = error })
            controller.abort()
            await vi.waitFor(() => expect(rejection).toMatchObject({ name: 'AbortError' }))
            later = parseRisuChat(increment, { runVar: true })
            void later.then(() => { laterSettled = true })
            // Drain queued continuations without releasing the first read.
            await new Promise<void>((resolve) => setImmediate(resolve))
            expect(laterSettled).toBe(false)
            release()
            expect(await first).toBe('m0|1')
            expect(await later).toBe('2')
            expect((await reopen()).scriptstate.$counter).toBe('2')
            expect(backgroundErrors).toEqual([])
        } finally {
            release()
            await Promise.allSettled([first, ...(queued ? [queued] : []), ...(later ? [later] : [])])
            spy.mockRestore()
        }
    })
    it.each(['variable', 'inject'])('persists a delayed parser %s write to the invocation conversation after navigation', async (effect) => {
        const { store, reopen, backgroundErrors } = await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = effect === 'inject'
                ? [{ type: 'editprocess', in: 'm999', out: '@@inject', flag: 'g', ableFlag: true }] as any : []
            seed.characters[0].triggerscript = []
            seed.characters[0].chats.push({ ...seed.characters[0].chats[0], id: 'chat-b',
                message: [{ role: 'user', data: 'other conversation' }], scriptstate: {} })
        })
        const { parseRisuChat } = await import('../plugins/parseRisuChat')
        let entered!: () => void
        const reading = new Promise<void>((resolve) => { entered = resolve })
        let release!: () => void
        const wait = new Promise<void>((resolve) => { release = resolve })
        const acquire = store.acquireRevision.bind(store)
        let delayed = false
        const spy = vi.spyOn(store, 'acquireRevision').mockImplementation(async (revision) => {
            const lease = await acquire(revision)
            const read = lease.readConversationWindow.bind(lease)
            lease.readConversationWindow = async (query) => {
                if (!delayed && query.conversationId === 'chat-a') {
                    delayed = true
                    entered()
                    await wait
                }
                return read(query)
            }
            return lease
        })
        const pending = parseRisuChat('{{lastmessage}}|{{setvar::captured::yes}}{{getvar::captured}}', {
            runVar: true, processRegex: effect === 'inject', messageIndex: 0,
        })
        try {
            await reading
            await current.runtime.activateConversation('chat-b')
            release()
            expect(await pending).toBe(effect === 'inject' ? '|yes' : 'm999|yes')
            await current.runtime.flushPendingData('parser-navigation')
            expect((await reopen()).scriptstate.$captured).toBe('yes')
            expect((await reopen()).message[0].data).toBe(effect === 'inject' ? 'm999|yes' : 'm0')
            expect((await store.readConversation('character-a', 'chat-b'))?.value.scriptstate?.$captured).toBeUndefined()
            expect((await store.readConversation('character-a', 'chat-b'))?.value.message[0].data).toBe('other conversation')
            expect(backgroundErrors).toEqual([])
        } finally { release(); spy.mockRestore() }
    })

    it.each([
        { target: 'an earlier message', index: 100 },
        { target: 'the streaming reply', index: MESSAGES },
    ])('keeps a streaming reply when a parser inject stores $target of the windowed conversation', async ({ index }) => {
        const { store, connect, reopen, backgroundErrors } = await bootWindowedConversation(storedMessages(), seed => {
            seed.characters[0].triggerscript = []
        })
        const reader = await connect()
        const { parseRisuChat } = await import('../plugins/parseRisuChat')
        const runtime = current.runtime
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        const reply = await whileStreaming({ runtime, length: MESSAGES, windowed: true }, async ({ chunk }) => {
            await runtime.flushPendingData('partial-reply')
            const source = runtime.getActiveConversationViewportSource()!
            await source.ensureRange({ startIndex: index, limit: 1, reason: 'viewport' })
            const version = source.snapshot().historyVersion!
            const commit = store.commit.bind(store)
            let grown = false
            const spy = vi.spyOn(store, 'commit').mockImplementation(async request => {
                const result = await commit(request)
                if (!grown && JSON.stringify(request).includes('STORED:plugin note')) {
                    grown = true
                    const committed = await reader.readConversationWindow({
                        characterId: 'character-a', conversationId: 'chat-a', startIndex: index, limit: 1,
                    })
                    expect(committed?.value.messages[0].data).toBe('STORED:plugin note')
                    chunk('next streamed chunk')
                    await vi.waitFor(() => expect(source.snapshot().rowAt(MESSAGES)?.message.data).toBe('next streamed chunk'))
                }
                return result
            })
            try {
                expect(await parseRisuChat('WRITE:plugin note', { processRegex: true, messageIndex: index })).toBe('plugin note')
                await runtime.flushPendingData('parser-inject')
                expect(grown).toBe(true)
                expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
                expect(source.snapshot().historyChangesSince?.(version)?.some(change => change.start <= index && index < change.end)).toBe(true)
                if (index !== MESSAGES) expect((await liveMessage(runtime, index))?.data).toBe('STORED:plugin note')
                await runtime.mutateConversationBinding('character-a', 'chat-a', { bindedPersona: 'persona-id' },
                    patch => Object.assign(DBState.db.characters[0].chats[0], patch))
                await parseRisuChat('{{setvar::during_stream::kept}}', { runVar: true })
            } finally { spy.mockRestore() }
        })
        await runtime.flushPendingData('streamed-reply')
        const stored = await reopen()
        expect(stored.message).toHaveLength(MESSAGES + 1)
        expect(stored.message[100].data).toBe(index === 100 ? 'STORED:plugin note' : 'WRITE:outside')
        expect(stored.message.at(-1)).toMatchObject({ chatId: reply.chatId, data: finalText })
        expect(stored).toMatchObject({ bindedPersona: 'persona-id', scriptstate: { $during_stream: 'kept' } })
        expect(backgroundErrors).toEqual([])
    })

    async function pluginFixture(edit?: SetterCase) {
        const messages = storedMessages()
        if (edit === 'legacy row') delete messages[0].chatId
        if (edit === 'unread copy predating user message') messages[MESSAGES - 1] = { role: 'user', data: 'latest prompt', chatId: 'latest-user' }
        const { connect, backgroundErrors } = await bootWindowedConversation(messages, seed => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
            if (!edit) installOutputTrigger(seed)
        })
        const runtime = current.runtime
        const access = createProductionPluginDatabaseAccess({
            owner: 'streaming-plugin',
            getLiveRuntime: () => runtime,
            getPersistentRevision: () => runtime.revision,
            commitPersistentUnitIntent: runtime.commitPersistentUnitIntent,
            flushPendingData: runtime.flushPendingDataLocally,
            assertPersistentMutationAllowed: runtime.assertPersistentMutationAllowed,
            getStorageAuthorityEpoch: runtime.getStorageAuthorityEpoch,
            getCompatibilityDatabase: () => DBState.db,
            getSelectedCharacterId: () => 'character-a',
            captureSelectedConversationTarget: runtime.captureSelectedConversationTarget,
            acquireCompleteConversation: runtime.acquireCompleteConversation,
            refreshSelectedConversationAfterReplacement: runtime.refreshSelectedConversationAfterReplacement,
            invalidateActiveConversationSession: runtime.invalidateActiveConversationSession,
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: runtime.getNavigationGeneration,
            readPluginStorageSnapshot: async () => ({}),
            snapshot: <T>(value: T) => JSON.parse(JSON.stringify(value)) as T,
        })
        return { runtime, access, connect, length: MESSAGES, windowed: true, backgroundErrors }
    }

    it.each(setterCases)('keeps a plugin write to a windowed streaming conversation (%s)', async edit => {
        const fixture = await pluginFixture(edit)
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        try {
            await checkStreamingWrite(fixture, edit)
            expect(fixture.backgroundErrors).toEqual([])
        } finally { fixture.access.closeReadBaselines() }
    })

    it('waits for an output trigger before applying a windowed setter and accepts the patch retry', async () => {
        const fixture = await pluginFixture()
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        try {
            await checkOutputProcessing(fixture)
            expect(fixture.backgroundErrors).toEqual([])
        } finally { fixture.access.closeReadBaselines() }
    })

    it('allows parseRisuChat and its variable writes inside the provider callback without waiting for the send', async () => {
        const { reopen, backgroundErrors } = await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
            seed.mainPrompt = '{{#when::1::is::1}}system{{/when}}{{#if 1}}prompt{{/if}}{{#pure}}plain{{/pure}}'
        })
        const complete = vi.spyOn(current.runtime, 'acquireCompleteConversation')
        const { sendChat } = await import('./index.svelte')
        const { requestChatData } = await import('./request/request')
        const { parseRisuChat } = await import('../plugins/parseRisuChat')
        const results: string[] = []
        const { registerDisplayRow, DisplayReadRecord } = await import('./scriptVariableReads')
        const row = registerDisplayRow(() => 0)
        const reads = new DisplayReadRecord()
        reads.note('api_write')
        row.track('body', reads)
        vi.mocked(requestChatData).mockImplementationOnce(async () => {
            results.push(await parseRisuChat('{{lastmessageindex}}|{{previouschatlog::0}}'))
            const previousReload = get(ReloadChatPointer)[0] ?? 0
            results.push(await parseRisuChat('{{setvar::api_write::saved}}{{getvar::api_write}}', { runVar: true }))
            expect(get(ReloadChatPointer)[0] ?? 0).toBeGreaterThan(previousReload)
            results.push(await parseRisuChat('{{previouschatlog::0}}', { processRegex: true }))
            return { type: 'success', result: 'api answer' }
        })
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        try { await expect(sendChat({ historyLimit: true })).resolves.toBe(true) }
        finally { row.dispose() }
        expect(results).toEqual(['999|m0', 'saved', 'm0'])
        const stored = await reopen()
        expect(stored.message.at(-1)?.data).toBe('api answer')
        expect(stored.scriptstate.$api_write).toBe('saved')
        expect(backgroundErrors).toEqual([])
        expect(complete).not.toHaveBeenCalled()
        complete.mockRestore()
    })

    it('keeps parser reads and variable commits available inside successive streaming output callbacks', async () => {
        const { reopen, backgroundErrors } = await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
        })
        const { sendChat } = await import('./index.svelte')
        const { pluginV2 } = await import('../plugins/plugins.svelte')
        const { parseRisuChat } = await import('../plugins/parseRisuChat')
        const counts: string[] = []
        const output = async (data: string) => {
            counts.push(await parseRisuChat('{{lastmessageindex}}', { processRegex: true }))
            await parseRisuChat(`{{setvar::stream_calls::${counts.length}}}`, { runVar: true })
            return data
        }
        pluginV2.editoutput.add(output)
        current.snapshots = ['first token', 'second token']
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        try {
            await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
            expect(counts.length).toBeGreaterThanOrEqual(2)
            expect(counts.every((count) => count === '1000')).toBe(true)
            const stored = await reopen()
            expect(stored.message.at(-1)?.data).toBe('second token')
            expect(stored.scriptstate.$stream_calls).toBe(String(counts.length))
            expect(backgroundErrors).toEqual([])
        } finally { pluginV2.editoutput.delete(output); current.snapshots = ['answer'] }
    })

    it.each(['success', 'streaming'] as const)('persists a delayed %s response while settings remain open', async (type) => {
        const { reopen, backgroundErrors } = await bootWindowedConversation(storedMessages())
        const { sendChat } = await import('./index.svelte')
        const { requestChatData } = await import('./request/request')
        const { reserveGeneration } = await import('./generationState')
        const { beginGenerationEndRun, subscribeGenerationEnd } = await import('./generationEnd')
        const controller = new AbortController()
        const reservation = reserveGeneration(() => controller.abort())!
        const target = current.runtime.captureSelectedConversationTarget()!
        const navigation = current.runtime.getNavigationGeneration()
        const finish = vi.fn()
        const unsubscribe = subscribeGenerationEnd(finish)
        const run = beginGenerationEndRun(target)
        let entered!: () => void
        const requested = new Promise<void>(resolve => { entered = resolve })
        let release!: () => void
        const responseReady = new Promise<void>(resolve => { release = resolve })
        vi.mocked(requestChatData).mockImplementationOnce(async () => {
            entered()
            await responseReady
            return type === 'success'
                ? { type: 'success', result: 'hidden answer' }
                : { type: 'streaming', result: new ReadableStream({ start(stream) {
                    stream.enqueue({ response: 'hidden' })
                    stream.enqueue({ response: 'hidden answer' })
                    stream.close()
                } }) }
        })
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        const sending = run.capture(() => sendChat({ historyLimit: true, signal: controller.signal }, reservation))
        try {
            await requested
            settingsOpen.set(true)
            expect(current.runtime.getNavigationGeneration()).toBe(navigation)
            expect(current.runtime.captureSelectedConversationTarget()?.conversationId).toBe(target.conversationId)
            release()
            await expect(sending).resolves.toBe(true)
            reservation.release()
            run.finish('completed')
            const stored = await reopen()
            expect(stored.message.filter(message => message.data === 'hidden answer')).toHaveLength(1)
            expect(finish).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({
                characterId: target.characterId, conversationId: target.conversationId, status: 'completed',
            }))
            expect(backgroundErrors).toEqual([])
        } finally {
            release()
            await sending
            reservation.release()
            unsubscribe()
            settingsOpen.set(false)
        }
    })

    const tagListener = `
        listenEdit('editOutput', function(id, value)
            local tag = value:match('<s>(.-)</s>')
            return value .. tag
        end)
    `
    it.each([
        {
            send: 'a start handler that fails after stopping the send',
            code: 'function onStart(id) stopChat(id); local ctor = nil; return ctor.value end',
            snapshots: ['answer'], sent: false, dialog: /^Lua \(start\)\n.*attempt to index a nil value \(local 'ctor'\)/,
        },
        { send: 'a start handler that stops the send', code: 'function onStart(id) stopChat(id) end', snapshots: ['answer'], sent: false, dialog: null },
        { send: 'an output listener that fails only on a partial response', code: tagListener, snapshots: ['<s>hal', '<s>half</s> done'], sent: true, dialog: null },
        {
            send: 'an output listener that fails on the complete response',
            code: tagListener, snapshots: ['<s>half</s>', 'done without the tag'], sent: true,
            dialog: /^Lua \(editOutput\)\n.*attempt to concatenate a nil value \(local 'tag'\)/,
        },
    ])('shows a Lua error dialog for $send only when the handler fails on its final input', async ({ code, snapshots, sent, dialog }) => {
        await bootWindowedConversation(storedMessages(), seed => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = [{ type: 'start', comment: '', conditions: [], effect: [{ type: 'triggerlua', code }] }] as never
        })
        const { sendChat } = await import('./index.svelte')
        current.snapshots = snapshots
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        const errors = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        try {
            await expect(sendChat({ historyLimit: true })).resolves.toBe(sent)
            if (dialog) expect(get(alertStore)).toMatchObject({ type: 'error', msg: expect.stringMatching(dialog) })
            else expect(alertStore.hasDialogs()).toBe(false)
        } finally {
            alertStore.set({ type: 'none', msg: '' })
            current.snapshots = ['answer']
            errors.mockRestore()
        }
    })

    it.each(['insert', 'remove'] as const)('persists the response after Lua %s moves its output target', async (action) => {
        const original = storedMessages()
        const { reopen, backgroundErrors } = await bootWindowedConversation(structuredClone(original), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = [{ type: 'start', conditions: [], effect: [{ type: 'triggerlua', code: `
                local moved = false
                listenEdit('editOutput', function(id, value, meta)
                    if not moved then
                        setChatVar(id, 'first_index', tostring(meta.index))
                        ${action === 'insert' ? "insertChat(id, 960, 'user', 'owned insert')" : 'removeChat(id, 960)'}
                        moved = true
                    else
                        setChatVar(id, 'later_index', tostring(meta.index))
                    end
                    return value
                end)
            ` }] }] as never
        })
        const { sendChat } = await import('./index.svelte')
        const { pluginV2 } = await import('../plugins/plugins.svelte')
        const output = vi.fn(async () => undefined)
        pluginV2.chatOutput.add(output)
        current.snapshots = ['preview', 'answer']
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        try {
            await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
            const stored = await reopen()
            const index = action === 'insert' ? 1001 : 999
            expect(stored.message[index]).toMatchObject({ role: 'char', data: 'answer' })
            expect(stored.scriptstate).toMatchObject({ $first_index: '1000', $later_index: String(index) })
            expect(JSON.stringify(stored.message.slice(0, WINDOW_START))).toBe(JSON.stringify(original.slice(0, WINDOW_START)))
            expect(output).toHaveBeenCalledOnce()
            expect(backgroundErrors).toEqual([])
        } finally {
            pluginV2.chatOutput.delete(output)
            current.snapshots = ['answer']
        }
    })

    it('gives output listeners the whole conversation and the absolute output index', async () => {
        const original = storedMessages()
        const { reopen, backgroundErrors } = await bootWindowedConversation(structuredClone(original), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
        })
        const { sendChat } = await import('./index.svelte')
        const { pluginV2 } = await import('../plugins/plugins.svelte')
        const events: { char: character; chat: Chat; messageIndex: number }[] = []
        const output = vi.fn(async (event: { char: character; chat: Chat; messageIndex: number }) => { events.push(event) })
        pluginV2.chatOutput.add(output)
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        try {
            await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
            expect(current.runtime.getSelectedConversationMode()).toBe('windowed')
            const stored = await reopen()
            const [{ char, chat, messageIndex }] = events
            expect(chat.message).toHaveLength(stored.message.length)
            expect(JSON.stringify(chat.message.slice(0, WINDOW_START))).toBe(JSON.stringify(original.slice(0, WINDOW_START)))
            expect(messageIndex).toBe(stored.message.length - 1)
            expect(chat.message[messageIndex]).toMatchObject({ role: 'char', data: 'answer' })
            expect(char.chats[0]).toEqual(chat)
            expect(char.chats[0]).not.toBe(chat)
            expect(backgroundErrors).toEqual([])
        } finally {
            pluginV2.chatOutput.delete(output)
        }
    })

    it("omits other conversations' messages for listeners that ask for the current conversation or when only the current chat is sent", async () => {
        const others = ['chat-b', 'chat-c'].map((id) => ({
            id, name: `Other ${id}`, note: '', localLore: [], fmIndex: -1,
            message: [{ role: 'user', data: `${id} message`, chatId: `${id}-0` }],
        }) as Chat)
        await bootWindowedConversation(storedMessages(), (seed) => {
            seed.characters[0].customscript = []
            seed.characters[0].triggerscript = []
            seed.characters[0].chats.push(...structuredClone(others))
        }, (db) => {
            // Only chat-b stays loaded; chat-c is read from the store.
            db.characters[0].chats[2] = createConversationSummaryStubFromChat('character-a', db.characters[0].chats[2], 2)
        })
        const { sendChat } = await import('./index.svelte')
        const { pluginV2 } = await import('../plugins/plugins.svelte')
        const { registerChatOutputListener, removeChatOutputListener } = await import('../plugins/pluginChatOutputListeners')
        const { updateDeviceSettings } = await import('../storage/deviceSettings')
        type Event = { char: character; chat: Chat }
        const full: Event[] = []
        const reduced: Event[] = []
        const fullListener = vi.fn(async (event: Event) => { full.push(event) })
        const currentListener = vi.fn(async (event: Event) => { reduced.push(event) })
        registerChatOutputListener(pluginV2.chatOutput, fullListener as never)
        registerChatOutputListener(pluginV2.chatOutput, currentListener as never, { history: 'current' })
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        const withoutMessages = ({ message: _message, ...metadata }: Chat) => metadata
        try {
            await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
            expect(full[0].char.chats.map(chat => chat.message?.length)).toEqual([full[0].chat.message.length, 1, 1])
            expect(reduced[0].char.chats).toEqual(full[0].char.chats.map(withoutMessages))
            expect(reduced[0].char.chats.some(chat => 'message' in chat)).toBe(false)
            expect(reduced[0].chat).toEqual(full[0].chat)

            updateDeviceSettings({ pluginOutputCurrentChatOnly: true })
            await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
            expect(full[1].char.chats.some(chat => 'message' in chat)).toBe(false)
            expect(full[1].char.chats.slice(1)).toEqual(others.map(withoutMessages))
            expect(full[1].chat.message.length).toBe(full[0].chat.message.length + 1)
        } finally {
            updateDeviceSettings({ pluginOutputCurrentChatOnly: false })
            removeChatOutputListener(pluginV2.chatOutput, fullListener as never)
            removeChatOutputListener(pluginV2.chatOutput, currentListener as never)
        }
    })

    it('stores trigger, Lua, @@inject, parser and response writes inside the window and leaves earlier messages byte-identical', async () => {
        const original = storedMessages()
        Object.assign(original[910], { data: '{{setvar::window_var::first}}x', extra: { nested: ['한글', '🙂'] } })
        Object.assign(original[920], { extra: { unchanged: true } })
        original[930].data = 'y{{getvar::window_var}}'
        original[940].data = '{{getvar::window_var}}z'
        const { store, backgroundErrors, reopen } = await bootWindowedConversation(structuredClone(original))
        const { sendChat } = await import('./index.svelte')
        vi.spyOn(console, 'log').mockImplementation(() => undefined)

        await expect(sendChat({ historyLimit: true })).resolves.toBe(true)
        await current.runtime.flushPendingData('history-window-send-test')

        expect(current.runtime.getSelectedConversationMode()).toBe('windowed')
        const persisted = (await store.readConversation('character-a', 'chat-a'))!.value.message
        expect(JSON.stringify(persisted.slice(0, WINDOW_START))).toBe(JSON.stringify(original.slice(0, WINDOW_START)))
        expect((await reopen()).scriptstate.$window_var).toBe('first')
        const inserted = persisted[960]
        expect(persisted.slice(WINDOW_START)).toEqual([
            ...original.slice(WINDOW_START, 910),
            { ...original[910], data: 'x' },
            ...original.slice(911, 930),
            { ...original[930], data: 'yfirst' },
            ...original.slice(931, 940),
            { ...original[940], data: 'firstz' },
            ...original.slice(941, 950),
            { ...original[950], data: 'STORED:inside' },
            ...original.slice(951, 960),
            { role: 'char', data: 'lua inserted', chatId: inserted.chatId },
            ...original.slice(960),
            expect.objectContaining({ role: 'char', data: 'answer' }),
        ])
        expect(inserted.chatId).toEqual(expect.any(String))
        expect(persisted).toHaveLength(MESSAGES + 2)
        expect(current.alertError).not.toHaveBeenCalled()
        expect(backgroundErrors).toEqual([])
    })

    it('continues the current stored tail after a competing reorder races a legacy ID assignment', async () => {
        const original = storedMessages()
        const tail = original.at(-1)!
        delete tail.chatId
        const neighbour = original.at(-2)!
        neighbour.role = 'char'
        neighbour.saying = 'character-a'
        const { store, connect, reopen, backgroundErrors } = await bootWindowedConversation(structuredClone(original), seed => {
            seed.characters[0].triggerscript = []
            seed.characters[0].customscript = []
        })
        const other = await connect()
        const read = async () => (await other.readConversation('character-a', 'chat-a'))!
        let moved = false
        const commit = store.commit.bind(store)
        vi.spyOn(store, 'commit').mockImplementation(async request => {
            if (!moved && request.conversations?.some(range => range.type === 'replace-range'
                && range.messages.some(message => message.data === tail.data && message.chatId))) {
                moved = true
                await other.commit({ expectedRevision: (await read()).revision, conversations: [
                    { type: 'replace-range', characterId: 'character-a', conversationId: 'chat-a',
                        start: MESSAGES - 2, deleteCount: 2, messages: [tail, neighbour] },
                ] })
            }
            return commit(request)
        })
        const { requestChatData } = await import('./request/request')
        const { sendChat } = await import('./index.svelte')
        let atRequest: Message[] | undefined
        vi.mocked(requestChatData).mockImplementationOnce(async () => {
            atRequest = (await read()).value.message.slice(-2)
            return { type: 'streaming', result: new ReadableStream({ start(stream) {
                stream.enqueue({ response: ' continued' })
                stream.close()
            } }) }
        })
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        expect(await sendChat({ historyLimit: true, continue: true })).toBe(true)
        await current.runtime.flushPendingData('continued-tail')
        expect(moved).toBe(true)
        expect(atRequest).toEqual([{ ...tail, chatId: expect.stringMatching(/\S/) }, neighbour])
        const stored = (await reopen()).message
        expect(stored.slice(0, -1)).toEqual([...original.slice(0, -2), atRequest![0]])
        expect(stored.at(-1)).toMatchObject({ chatId: neighbour.chatId, data: neighbour.data + ' continued' })
        expect(backgroundErrors).toEqual([])
    })

    it('finishes a provider stream on a reply moved by another connection during a settings commit', async () => {
        const original = storedMessages()
        const { store, connect, reopen, backgroundErrors } = await bootWindowedConversation(structuredClone(original), seed => {
            seed.characters[0].triggerscript = []
            seed.characters[0].customscript = []
        })
        const { runtime } = current
        const remote = await connect()
        vi.spyOn(console, 'log').mockImplementation(() => undefined)
        const reply = await whileStreaming({ runtime, length: MESSAGES, windowed: true }, async ({ reply, chunk }) => {
            await runtime.flushPendingDataLocally('first-chunk')
            const commit = store.commit.bind(store)
            vi.spyOn(store, 'commit').mockImplementationOnce(async request => {
                const chat = (await remote.readConversation('character-a', 'chat-a'))!
                await remote.commit({ expectedRevision: chat.revision, conversations: [
                    { type: 'replace-range', characterId: 'character-a', conversationId: 'chat-a',
                        start: MESSAGES - 1, deleteCount: 2, messages: [chat.value.message.at(-1)!, original.at(-1)!] },
                ] })
                chunk('chunk during setting')
                await vi.waitFor(async () => expect((await liveMessage(runtime, MESSAGES))?.data).toBe('chunk during setting'))
                return commit(request)
            })
            const { updateChatBinding } = await import('../chatBindings.svelte')
            await updateChatBinding(DBState.db.characters[0].chats[0], { note: 'changed' })
            expect((await liveMessage(runtime, MESSAGES - 1))?.chatId).toBe(reply.chatId)
        })
        await runtime.flushPendingDataLocally('completed-stream')
        const chat = await reopen()
        expect(chat.note).toBe('changed')
        expect(chat.message).toEqual([...original.slice(0, -1), expect.objectContaining({ chatId: reply.chatId, data: finalText }), original.at(-1)!])
        expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
        expect(backgroundErrors).toEqual([])
    })
})
