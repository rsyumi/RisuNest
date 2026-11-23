import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest'

const fixture = vi.hoisted(() => ({
    store: null as any,
    replacers: new Set<(formated: unknown[], model: string) => Promise<unknown[]>>(),
    modelRequests: 0,
    historyLimit: false,
    outputs: new Set<(event: any) => Promise<void>>(),
}))

// The synthetic database already supplies the generation fields; import migration is outside this fixture.
vi.mock('src/ts/storage/databasePreparation', async (importOriginal) => (await import('../process/tests/sendChatTestHarness')).copyingDatabasePreparationModule(importOriginal))
vi.mock('src/ts/storage/deviceSettings', async (importOriginal) => (await import('../process/tests/sendChatTestHarness')).historyWindowDeviceSettingsModule(importOriginal, () => fixture.historyLimit))
vi.mock('src/ts/storage/persistentDataStoreFactory', async () => (await import('../process/tests/sendChatTestHarness')).persistentDataStoreFactoryModule(() => fixture.store))
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('src/ts/tokenizer', async () => (await import('../process/tests/sendChatTestHarness')).tokenizerModule({
    tokenize: vi.fn(async () => 10),
    encodeWithTokenizer: vi.fn(async () => new Array(10).fill(0)),
}))
vi.mock('src/ts/alert', async () => (await import('../process/tests/sendChatTestHarness')).alertModule())
vi.mock('src/ts/parser/chatML', async () => (await import('../process/tests/sendChatTestHarness')).chatMLModule())
vi.mock('src/ts/process/lorebook.svelte', async () => (await import('../process/tests/sendChatTestHarness')).lorebookModule())
// The provider request: `beforeRequest` replacers run where `requestChatData` runs them, then the reply.
vi.mock('src/ts/process/request/request', async () => (await import('../process/tests/sendChatTestHarness')).requestModule(vi.fn(async (request: { formated: unknown[] }, purpose: string) => {
    if (purpose === 'emotion') return '|igp'
    fixture.modelRequests += 1
    for (const replacer of fixture.replacers) request.formated = await replacer(request.formated, purpose)
    return {
        type: 'streaming',
        result: new ReadableStream<Record<string, string>>({
            start(controller) {
                controller.enqueue({ response: 'answer' })
                controller.close()
            },
        }),
    }
})))
vi.mock('src/ts/process/stableDiff', async () => (await import('../process/tests/sendChatTestHarness')).stableDiffModule())
vi.mock('src/ts/process/scripts', async () => (await import('../process/tests/sendChatTestHarness')).scriptsModule())
vi.mock('src/ts/process/templates/templates', async () => (await import('../process/tests/sendChatTestHarness')).templatesModule())
vi.mock('src/ts/process/exampleMessages', async () => (await import('../process/tests/sendChatTestHarness')).exampleMessagesModule())
vi.mock('src/ts/process/tts', async () => (await import('../process/tests/sendChatTestHarness')).ttsModule())
vi.mock('src/ts/process/memory/supaMemory', async () => (await import('../process/tests/sendChatTestHarness')).supaMemoryModule())
// A character without trigger scripts: the real `runTrigger` returns null for every mode.
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn(async () => null) }))
vi.mock('src/ts/process/memory/hypamemory', async () => (await import('../process/tests/sendChatTestHarness')).hypamemoryModule())
vi.mock('src/ts/process/embedding/addinfo', async () => (await import('../process/tests/sendChatTestHarness')).addinfoModule())
vi.mock('src/ts/process/files/inlays', async () => (await import('../process/tests/sendChatTestHarness')).inlaysModule())
vi.mock('src/ts/process/models/modelString', async () => (await import('../process/tests/sendChatTestHarness')).modelStringModule())
vi.mock('src/ts/process/inlayScreen', async () => (await import('../process/tests/sendChatTestHarness')).inlayScreenModule())
vi.mock('src/ts/process/transformers', async () => (await import('../process/tests/sendChatTestHarness')).transformersModule())
vi.mock('src/ts/process/memory/hanuraiMemory', async () => (await import('../process/tests/sendChatTestHarness')).hanuraiMemoryModule())
vi.mock('src/ts/process/memory/hypav2', async () => (await import('../process/tests/sendChatTestHarness')).hypav2Module())
vi.mock('src/ts/process/memory/hypav3', async () => (await import('../process/tests/sendChatTestHarness')).hypav3Module())
vi.mock('src/ts/process/scriptings', async () => (await import('../process/tests/sendChatTestHarness')).scriptingsModule())
vi.mock('src/ts/model/modellist', async (importOriginal) => ({
    ...await importOriginal<typeof import('../model/modellist')>(),
    ...(await import('../process/tests/sendChatTestHarness')).modellistModule(),
}))
vi.mock('src/ts/process/modules', async () => (await import('../process/tests/sendChatTestHarness')).modulesModule())
vi.mock('src/ts/globalApi.svelte', async () => (await import('../process/tests/sendChatTestHarness')).globalApiModule({ forageStorage: {} }))
vi.mock('src/ts/plugins/plugins.svelte', async () => (await import('../process/tests/sendChatTestHarness')).pluginsModule({ chatOutput: fixture.outputs, replacerbeforeRequest: fixture.replacers }))
vi.mock('src/ts/plugins/pluginDatabaseAccess', async (importOriginal) =>
    (await import('../process/tests/sendChatTestHarness')).pluginDatabaseAccessModule(importOriginal))
vi.mock('src/ts/process/presetChain', async () => (await import('../process/tests/sendChatTestHarness')).presetChainModule())

import type { Database, Message } from '../storage/database.svelte'
import { getDatabase, setDatabaseLite } from '../storage/database.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import {
    acquireCompleteConversation,
    captureSelectedConversationTarget,
    captureSelectedConversationAuthority,
    replacePersistentDatabase,
    flushPendingDataLocally,
    getActiveConversationSession,
    initializeActiveWorkingSet,
} from '../storage/persistentDataRuntime.svelte'
import { selectedCharID } from '../stores.svelte'
import { doingChat, sendChat } from '../process/index.svelte'
import { createRisunestPrivateApi } from './apiV3/risunestPrivateApi'
import { conversationPatchAccess } from './conversationPatchHost'
import { normalizeConversationPatchInput } from './conversationPatch'
import { activeRerollConversations } from '../durableReroll'

const target = { characterId: 'character-a', conversationId: 'chat-a' }
const factory = new IDBFactory()
const storeName = `patch-generation-${crypto.randomUUID()}`

function database(): Database {
    const chat = {
        id: target.conversationId,
        name: 'Chat A',
        note: '',
        localLore: [],
        fmIndex: -1,
        message: [{ role: 'user', data: 'hello', chatId: 'user-message' }] as Message[],
        scriptstate: {},
    }
    return {
        characters: [{
            type: 'character', chaId: target.characterId, name: 'Character A', chatPage: 0, chats: [chat],
            firstMessage: '', alternateGreetings: [''], desc: '', personality: '', scenario: '', bias: [],
            additionalAssets: [], emotionImages: [], triggerscript: [], defaultVariables: '', reloadKeys: 0,
            viewScreen: 'none', inlayViewScreen: false, supaMemory: false,
        }],
        statics: { messages: 0 },
        botPresets: [],
        botPresetsId: 0,
        aiModel: 'test-model',
        maxContext: 8_192,
        maxResponse: 128,
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
        plugins: [],
    } as unknown as Database
}

const createPatches = () => createRisunestPrivateApi({
    databaseAccess: { readConversationContext: vi.fn() } as any,
    patchAccess: conversationPatchAccess,
    hostTools: { listTools: vi.fn(), callTool: vi.fn() },
    chatView: { register: vi.fn(), unregister: vi.fn(), dispose: vi.fn() } as any,
    generationEnd: { register: vi.fn(), unregister: vi.fn(), dispose: vi.fn() } as any,
    hasDatabasePermission: async () => true,
    hasDatabaseWritePermission: async () => true,
    hasHostToolPermission: async () => true,
    lifetimeSignal: new AbortController().signal,
})

// The selected conversation keeps its messages in the active session, and the chat object carries
// its metadata. A generation holds the conversation complete; once idle it may be windowed again,
// so reads after a generation promote it the way the chat screen does.
function generatingConversation() {
    const session = getActiveConversationSession()
    expect(session?.isActive).toBe(true)
    return { message: session!.materializeCompatibilityArray(), scriptstate: getDatabase().characters[0].chats[0].scriptstate }
}

async function live() {
    const lease = await acquireCompleteConversation('patch-generation-test', captureSelectedConversationTarget())
    try {
        return { message: lease.session.materializeCompatibilityArray(), scriptstate: getDatabase().characters[0].chats[0].scriptstate }
    } finally {
        lease.release()
    }
}

async function resetDatabase(initial: Database) {
    const lease = await acquireCompleteConversation('patch-generation-reset', captureSelectedConversationTarget())
    try { await replacePersistentDatabase(initial, 'patch-generation-reset') }
    finally { lease.release() }
    setDatabaseLite(initial)
    selectedCharID.set(0)
    await initializeActiveWorkingSet(getDatabase())
}

async function reloadedConversation() {
    const reopened = new IndexedDbPersistentDataStore(storeName, factory, IDBKeyRange)
    await reopened.open()
    return (await reopened.readConversation(target.characterId, target.conversationId))!.value
}

describe.each([false, true])('plugin conversation patch from a beforeRequest replacer (history limit %s)', (historyLimit) => {
    let patches: ReturnType<typeof createPatches>
    let log: { mockRestore(): void } | undefined
    beforeAll(async () => {
        log = vi.spyOn(console, 'log').mockImplementation(() => undefined)
        fixture.historyLimit = historyLimit
        fixture.modelRequests = 0
        patches = createPatches()
        const initial = database()
        if (!fixture.store) {
            fixture.store = new IndexedDbPersistentDataStore(storeName, factory, IDBKeyRange)
            await fixture.store.open()
            await fixture.store.replaceFromDatabase(initial)
            setDatabaseLite(initial)
            selectedCharID.set(0)
            await initializeActiveWorkingSet(getDatabase())
        } else {
            selectedCharID.set(0)
            await resetDatabase(initial)
        }
        const metadata = await fixture.store.readConversationMetadata(target.characterId, target.conversationId)
        const authority = captureSelectedConversationAuthority()
        if (authority) expect(authority.totalMessages).toBe(metadata.value.totalMessages)
    })
    afterAll(() => {
        fixture.replacers.clear()
        fixture.outputs.clear()
        doingChat.set(false)
        selectedCharID.set(-1)
        log?.mockRestore()
    })

    it('applies a plugin field and chat variable during the request, keeps the reply, and refuses message data', async () => {
        const outcomes: unknown[] = []
        const output = vi.fn(async (_event: unknown) => {})
        fixture.outputs.add(output)
        fixture.replacers.add(async (formated) => {
            outcomes.push(await patches.patchConversation({
                ...target,
                mutationId: 'data-during-request',
                messages: [{ index: 0, messageId: 'user-message', expected: { data: 'hello' }, set: { data: 'rewritten' } }],
            }))
            outcomes.push(await patches.patchConversation({
                ...target,
                mutationId: 'metadata-during-request',
                messages: [{ index: 0, messageId: 'user-message', expected: { data: 'hello' }, set: { __translation: 'record' } }],
                chatVariables: [{ key: '$bridge', expected: null, value: 'on' }],
            }))
            return formated
        })

        await expect(sendChat({ historyLimit })).resolves.toBe(true)

        fixture.outputs.delete(output)
        expect(output).toHaveBeenCalledOnce()
        expect(output).toHaveBeenCalledWith(expect.objectContaining({
            characterIndex: 0, chatIndex: 0, messageIndex: 1,
            char: expect.objectContaining({ chaId: target.characterId }),
            chat: expect.objectContaining({ id: target.conversationId }),
        }))
        expect(fixture.modelRequests).toBe(1)
        expect(outcomes).toEqual([
            { status: 'busy', revision: expect.any(Number) },
            { status: 'applied', revision: expect.any(Number) },
        ])
        const applied = await live()
        expect(applied.message.map((message) => message.data)).toEqual(['hello', 'answer'])
        expect(applied.message[0]).toMatchObject({ data: 'hello', __translation: 'record' })
        expect(applied.scriptstate).toEqual({ $bridge: 'on' })

        await flushPendingDataLocally('patch-generation-test')
        const stored = await reloadedConversation()
        expect(stored.message.map((message) => message.data)).toEqual(['hello', 'answer'])
        expect(stored.message[0]).toMatchObject({ data: 'hello', __translation: 'record' })
        expect(stored.scriptstate).toEqual({ $bridge: 'on' })
    })

    it('lands a patch the replacer does not wait for before the reply is applied', async () => {
        fixture.replacers.clear()
        let pending: Promise<unknown> | undefined
        fixture.replacers.add(async (formated) => {
            pending = patches.patchConversation({
                ...target,
                mutationId: 'unawaited-during-request',
                messages: [{ index: 0, messageId: 'user-message', expected: { data: 'hello' }, set: { __summary: 'late' } }],
                chatVariables: [{ key: '$late', expected: null, value: 'yes' }],
            })
            return formated
        })

        await expect(sendChat({ historyLimit })).resolves.toBe(true)

        await expect(pending).resolves.toEqual({ status: 'applied', revision: expect.any(Number) })
        const applied = await live()
        expect(applied.message.map((message) => message.data)).toEqual(['hello', 'answer', 'answer'])
        expect(applied.message[0]).toMatchObject({ __translation: 'record', __summary: 'late' })
        expect(applied.scriptstate).toEqual({ $bridge: 'on', $late: 'yes' })

        await flushPendingDataLocally('patch-generation-test')
        const stored = await reloadedConversation()
        expect(stored.message.map((message) => message.data)).toEqual(['hello', 'answer', 'answer'])
        expect(stored.message[0]).toMatchObject({ __translation: 'record', __summary: 'late' })
        expect(stored.scriptstate).toEqual({ $bridge: 'on', $late: 'yes' })
    })

    it('applies the reply only after a patch admitted during the request has landed', async () => {
        fixture.replacers.clear()
        let releaseCommit!: () => void
        const commitHeld = new Promise<void>((resolve) => { releaseCommit = resolve })
        let held = false
        const commit = fixture.store.commit.bind(fixture.store)
        // Holds the store commit that carries the patch, so the patch is admitted but has not landed.
        const commits = vi.spyOn(fixture.store, 'commit').mockImplementation(async (input: unknown) => {
            if (!held && JSON.stringify(input).includes('__held')) {
                held = true
                await commitHeld
            }
            return commit(input)
        })
        let pending: Promise<unknown> | undefined
        fixture.replacers.add(async (formated) => {
            pending = patches.patchConversation({
                ...target,
                mutationId: 'held-during-request',
                messages: [{ index: 0, messageId: 'user-message', expected: { data: 'hello' }, set: { __held: 'landed' } }],
                chatVariables: [{ key: '$held', expected: null, value: 'landed' }],
            })
            return formated
        })
        try {
            const sending = sendChat({ historyLimit })
            await vi.waitFor(() => expect(held).toBe(true))
            await vi.waitFor(() => expect(fixture.modelRequests).toBe(3))
            if (historyLimit) {
                expect(getActiveConversationSession()).toBeNull()
                expect(captureSelectedConversationAuthority()?.totalMessages).toBe(3)
            } else expect(generatingConversation().message).toHaveLength(3)
            releaseCommit()

            await expect(sending).resolves.toBe(true)
            await expect(pending).resolves.toEqual({ status: 'applied', revision: expect.any(Number) })
            const applied = await live()
            expect(applied.message.map((message) => message.data)).toEqual(['hello', 'answer', 'answer', 'answer'])
            expect(applied.message[0]).toMatchObject({ __held: 'landed' })
            expect(applied.scriptstate).toMatchObject({ $held: 'landed' })

            await flushPendingDataLocally('patch-generation-test')
            const stored = await reloadedConversation()
            expect(stored.message.map((message) => message.data)).toEqual(['hello', 'answer', 'answer', 'answer'])
            expect(stored.message[0]).toMatchObject({ __held: 'landed' })
            expect(stored.scriptstate).toMatchObject({ $held: 'landed' })
        } finally {
            releaseCommit()
            commits.mockRestore()
        }
    })
    it('refuses conflicting expected values, cancellation and reroll patches outside the request phase', async () => {
        fixture.replacers.clear()
        fixture.replacers.add(async (formated) => {
            await expect(patches.patchConversation({
                ...target, mutationId: 'conflicting-metadata',
                messages: [{ index: 0, messageId: 'user-message', expected: { data: 'wrong' }, set: { __conflict: true } }],
            })).resolves.toMatchObject({ status: 'conflict' })
            const abort = new AbortController()
            abort.abort()
            await expect(conversationPatchAccess.patchConversation(normalizeConversationPatchInput({
                ...target, mutationId: 'cancelled-metadata',
                messages: [{ index: 0, messageId: 'user-message', set: { __cancelled: true } }],
            }), abort.signal)).rejects.toThrow()
            return formated
        })
        await expect(sendChat({ historyLimit })).resolves.toBe(true)
        activeRerollConversations.set([target.conversationId])
        try {
            await expect(patches.patchConversation({
                ...target, mutationId: 'reroll-metadata',
                messages: [{ index: 0, messageId: 'user-message', set: { __reroll: true } }],
            })).resolves.toMatchObject({ status: 'busy' })
        } finally { activeRerollConversations.set([]) }
        const stored = await reloadedConversation()
        expect(stored.message[0]).not.toHaveProperty('__conflict')
        expect(stored.message[0]).not.toHaveProperty('__cancelled')
        expect(stored.message[0]).not.toHaveProperty('__reroll')
        expect(stored.message.at(-1)?.data).toBe('answer')
    })

    it('keeps patches outside the loaded history and on its tail without promoting the conversation', async () => {
        if (!historyLimit) return
        fixture.replacers.clear()
        const initial = database()
        initial.maxContext = 64
        initial.maxResponse = 8
        initial.characters[0].chats[0].message = Array.from({ length: 40 }, (_, index) => ({
            role: 'user', data: `history ${index}`, chatId: `history-${index}`,
        }))
        await resetDatabase(initial)
        const fullReads = vi.spyOn(fixture.store, 'readConversation')
        const windowReads: Array<{ startIndex?: number; limit?: number }> = []
        const leasedFullReads = vi.fn()
        const acquire = fixture.store.acquireRevision.bind(fixture.store)
        const leases = vi.spyOn(fixture.store, 'acquireRevision').mockImplementation(async (revision: number) => {
            const lease = await acquire(revision)
            const readWindow = lease.readConversationWindow.bind(lease)
            const readConversation = lease.readConversation.bind(lease)
            lease.readConversationWindow = (query: { startIndex?: number; limit?: number }) => {
                windowReads.push(query)
                return readWindow(query)
            }
            lease.readConversation = (...args: unknown[]) => {
                leasedFullReads()
                return readConversation(...args)
            }
            return lease
        })
        fixture.replacers.add(async (formated) => {
            expect(getActiveConversationSession()).toBeNull()
            await expect(patches.patchConversation({
                ...target, mutationId: 'outside-window',
                messages: [
                    { index: 0, messageId: 'history-0', set: { __outside: 'retained' } },
                    { index: 39, messageId: 'history-39', set: { __tail: 'retained' } },
                ],
                chatVariables: [{ key: '$window', expected: null, value: 'retained' }],
            })).resolves.toMatchObject({ status: 'applied' })
            expect(getActiveConversationSession()).toBeNull()
            return formated
        })
        try {
            await expect(sendChat({ historyLimit })).resolves.toBe(true)
            await flushPendingDataLocally('patch-window-test')
            expect(fullReads).not.toHaveBeenCalled()
            expect(leasedFullReads).not.toHaveBeenCalled()
            expect(windowReads.some((query) => query.startIndex === 27 && query.limit === 13)).toBe(true)
            const stored = await reloadedConversation()
            expect(stored.message).toHaveLength(41)
            expect(stored.message[0]).toMatchObject({ __outside: 'retained' })
            expect(stored.message[39]).toMatchObject({ __tail: 'retained' })
            expect(stored.message[40].data).toBe('answer')
            expect(stored.scriptstate).toMatchObject({ $window: 'retained' })
        } finally {
            fullReads.mockRestore()
            leases.mockRestore()
        }
    })

})
