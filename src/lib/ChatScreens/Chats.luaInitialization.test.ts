// @vitest-environment happy-dom

import { beforeAll, afterAll, expect, test, vi } from 'vitest'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { mount, tick, unmount } from 'svelte'
import type { character, Message } from 'src/ts/storage/database.svelte'
import { ActiveConversationSession } from 'src/ts/storage/activeConversationSession'
import { SynchronousSessionConversationViewportSource } from 'src/ts/conversationViewportSource'
import type { LiveChatParserProjectionResolver } from 'src/ts/selectedConversationLiveParserProjection'
import ChatsHarness from './ChatsHarness.test.svelte'
import { consumeStreamingDisplayStream } from 'src/ts/process/streamingDisplayStream'
const scriptingState = vi.hoisted(() => ({
    database: { characters: [] as character[], templateDefaultVariables: '' },
    parses: 0,
    parseBudget: Infinity,
    metadataReads: 0,
    session: null as ActiveConversationSession | null,
}))
const luaCode = `listenEdit('editDisplay', function(id, value)
    local previous = getState(id, 'synthetic_initialized')
    if previous == nil then setState(id, 'synthetic_initialized', 1) end
    return value .. '\\nSYNTHETIC_LUA_OK ' .. tostring(#getFullChat(id))
end)`
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    peekActiveConversationSession: () => scriptingState.session,
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => scriptingState.database,
    getCurrentCharacter: () => scriptingState.database.characters[0],
    getCurrentChat: () => scriptingState.database.characters[0]?.chats[0],
    setDatabase: vi.fn(),
}))
vi.mock('src/ts/parser/parser.svelte', () => ({
    hasher: vi.fn(),
    risuChatParser: (value: string) => value,
    ParseMarkdown: async (value: string, character: character) => {
        const { runLuaEditTrigger } = await import('src/ts/process/scriptings')
        scriptingState.parses++
        if (scriptingState.parses > scriptingState.parseBudget) return 'SYNTHETIC_PARSE_BUDGET_EXCEEDED'
        return runLuaEditTrigger(character, 'editdisplay', value)
    },
    trimMarkdown: (value: string) => value,
    addMetadataToElement: (value: string) => value,
    postTranslationParse: (value: string) => value,
    getDistance: () => 0,
}))
vi.mock('src/ts/alert', () => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertInput: vi.fn(),
    alertNormal: vi.fn(),
    alertSelect: vi.fn(),
}))
vi.mock('src/ts/platform', () => ({ isTauriMobile: true }))
vi.mock('src/ts/tokenizer', () => ({ tokenize: vi.fn() }))
vi.mock('src/ts/util', () => ({
    asBuffer: vi.fn(),
    parseKeyValue: () => [],
    getPersonaPrompt: vi.fn(),
    getUserIcon: vi.fn(),
    getUserName: vi.fn(),
    sleep: () => Promise.resolve(),
}))
vi.mock('src/ts/process/modules', () => ({
    getModuleLorebooks: () => [],
    getModuleTriggers: () => [],
    getModuleAssets: () => [],
}))
vi.mock('src/ts/process/files/inlays', () => ({
    getInlayAsset: vi.fn(),
    writeInlayImage: vi.fn(),
}))
vi.mock('src/ts/process/lorebook.svelte', () => ({
    loadLoreBookV3PromptFromCompatibilitySnapshot: vi.fn(),
}))
vi.mock('src/ts/process/memory/hypamemory', () => ({ HypaProcesser: vi.fn() }))
vi.mock('src/ts/process/request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('src/ts/process/stableDiff', () => ({ generateAIImage: vi.fn() }))
vi.mock('src/ts/translator/translator', () => ({
    getLLMCache: vi.fn(),
    translateHTML: vi.fn(),
}))

beforeAll(async () => {
    Object.defineProperty(document, 'currentScript', {
        configurable: true,
        value: {
            src: pathToFileURL(
                resolve(process.cwd(), 'node_modules/wasmoon/dist/index.js'),
            ).href,
        },
    })
    const luaJson = await readFile(
        resolve(process.cwd(), 'public/lua/json.lua'),
        'utf8',
    )
    const wasm = await readFile(
        resolve(process.cwd(), 'node_modules/wasmoon/dist/glue.wasm'),
    )
    vi.stubGlobal(
        'fetch',
        vi.fn(
            async (url) =>
                new Response(
                    String(url).endsWith('glue.wasm')
                        ? new Uint8Array(wasm)
                        : luaJson,
                    { status: 200 },
                ),
        ),
    )
})
vi.mock('src/ts/characters', () => ({
    getCharImage: async (source: string) => source,
}))
vi.mock('src/ts/globalApi.svelte', () => ({
    chatFoldedStateMessageIndex: { index: -1 },
    fetchNative: vi.fn(),
    readImage: vi.fn(),
    getFileSrc: vi.fn(),
}))
vi.mock('src/ts/ui/yieldToUi', () => ({
    yieldToMainThread: () => Promise.resolve(),
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: {
            get db() {
                return scriptingState.database
            },
        },
        selIdState: { selId: 0 },
        selectedCharID: writable(0),
        ReloadChatPointer: writable({}),
        ReloadGUIPointer: writable(0),
        createSimpleCharacter: (char: character) => ({
            type: 'simple',
            chaId: char.chaId,
            virtualscript: char.virtualscript,
            customscript: char.customscript,
            additionalAssets: char.additionalAssets,
            emotionImages: char.emotionImages,
            triggerscript: char.triggerscript,
        }),
    }
})
vi.mock('./Chat.svelte', async () => ({
    default: (await import('./ChatLuaBodyProbe.test.svelte')).default,
}))
vi.mock('./CreatorQuote.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))

function makeCharacter(messages: Message[]): character {
    return {
        type: 'character',
        name: 'Character',
        image: 'character.png',
        chaId: 'character-id',
        chatPage: 0,
        chats: [
            {
                id: 'chat-room-id',
                message: messages,
                isStreaming: false,
                activeStreamingDisplayOptimizationMode: 'balanced',
            },
        ],
        firstMessage: 'first greeting',
        alternateGreetings: [],
        creatorNotes: '',
        removedQuotes: false,
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [
            {
                type: 'start',
                conditions: [],
                effect: [{ type: 'triggerlua', code: luaCode }],
            },
        ],
    } as unknown as character
}

function makeMetadataOnlyCharacter(): character {
    const conversation = {
        id: 'chat-room-id',
        isStreaming: false,
        activeStreamingDisplayOptimizationMode: 'balanced',
    } as character['chats'][number]
    Object.defineProperty(conversation, 'message', {
        get() {
            scriptingState.metadataReads++
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    return { ...makeCharacter([]), chats: [conversation] } as character
}

import ChatConversationStart from './ChatConversationStart.svelte'

afterAll(() => {
    vi.unstubAllGlobals()
    delete (document as unknown as { currentScript?: unknown }).currentScript
})

test.each([false, true])(
    'refreshes existing Lua bodies in place when their complete history changes (per-row writes: %s)',
    async (perRowWrites) => {
        const character = makeCharacter(
            Array.from({ length: 3 }, (_, index) => ({
                role: 'char',
                data: `synthetic ${index}`,
                chatId: `history-${index}`,
            })),
        )
        if (perRowWrites) {
            character.triggerscript[0].effect = [
                {
                    type: 'triggerlua',
                    code: luaCode.replace(
                        "    local previous = getState(id, 'synthetic_initialized')",
                        "    setState(id, 'synthetic_rendered_row', value)\n    local previous = getState(id, 'synthetic_initialized')",
                    ),
                },
            ]
        }
        scriptingState.parses = 0
        scriptingState.parseBudget = 40
        const conversation = character.chats[0]
        const session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: conversation.id!,
            conversation,
            storeRevision: 1,
        })
        scriptingState.database = {
            characters: [character],
            templateDefaultVariables: '',
        }
        scriptingState.session = session
        const source = new SynchronousSessionConversationViewportSource({
            session,
            captureCurrent: () => ({ character, conversation }),
        })
        const resolver: LiveChatParserProjectionResolver = {
            resolve: async ({ row, totalMessages }) => ({
                kind: 'complete',
                characterId: character.chaId,
                conversationId: conversation.id!,
                revision: session.version,
                totalMessages,
                chatID: row.absoluteIndex,
                projectedChatID: row.absoluteIndex,
                historyOffset: 0,
                reasons: ['projection-budget'],
                release: () => {},
            }),
        }
        const target = document.createElement('div')
        document.body.appendChild(target)
        const mounted = mount(ChatsHarness, {
            target,
            props: {
                initialCharacter: character,
                initialViewportSource: source,
                parserProjectionResolver: resolver,
            },
        })
        try {
            const bodies = () =>
                [...target.querySelectorAll('[data-lua-body]')].filter(
                    (node) => Number(node.getAttribute('data-index')) >= 0,
                )
            await vi.waitFor(() => {
                expect(bodies()).toHaveLength(3)
                expect(
                    bodies().every((node) =>
                        node.textContent?.includes('SYNTHETIC_LUA_OK 3'),
                    ),
                ).toBe(true)
            })
            const initialParses = scriptingState.parses
            await new Promise((resolve) => setTimeout(resolve, 50))
            expect(scriptingState.parses).toBe(initialParses)
            const previousBodies = bodies()
            session.append({
                role: 'char',
                data: 'new tail',
                chatId: 'history-3',
            })
            await vi.waitFor(() => {
                expect(bodies()).toHaveLength(4)
                expect(
                    bodies().every((node) =>
                        node.textContent?.includes('SYNTHETIC_LUA_OK 4'),
                    ),
                ).toBe(true)
            })
            expect(
                previousBodies.every((node) => bodies().includes(node)),
            ).toBe(true)
        } finally {
            await unmount(mounted)
            source.dispose()
            scriptingState.parseBudget = Infinity
            target.remove()
            scriptingState.session = null
        }
    },
)

test('renders balanced stream snapshots through the real ChatBody and Lua before EOF without replacing the body', async () => {
    const character = makeCharacter([{ role: 'char', data: '', chatId: 'stream-output' }])
    character.chats[0].isStreaming = true
    const conversation = character.chats[0]
    const session = new ActiveConversationSession({
        characterId: character.chaId,
        conversationId: conversation.id!,
        conversation,
        storeRevision: 1,
    })
    scriptingState.database = { characters: [character], templateDefaultVariables: '' }
    scriptingState.session = session
    scriptingState.parses = 0
    scriptingState.parseBudget = 20
    const source = new SynchronousSessionConversationViewportSource({
        session,
        captureCurrent: () => ({ character, conversation }),
    })
    const resolver: LiveChatParserProjectionResolver = {
        resolve: async ({ row, totalMessages }) => ({
            kind: 'complete',
            characterId: character.chaId,
            conversationId: conversation.id!,
            revision: session.version,
            totalMessages,
            chatID: row.absoluteIndex,
            projectedChatID: row.absoluteIndex,
            historyOffset: 0,
            reasons: ['projection-budget'],
            release: () => {},
        }),
    }
    const target = document.createElement('div')
    document.body.appendChild(target)
    const mounted = mount(ChatsHarness, {
        target,
        props: {
            initialCharacter: character,
            initialViewportSource: source,
            parserProjectionResolver: resolver,
        },
    })
    const abort = new AbortController()
    let streamController!: ReadableStreamDefaultController<string>
    const stream = new ReadableStream<string>({
        start(controller) {
            streamController = controller
        },
    })
    const consume = consumeStreamingDisplayStream({
        mode: 'balanced',
        reader: stream.getReader(),
        abortSignal: abort.signal,
        getSnapshot: (value) => value,
        isOwned: () => session.isActive,
        processSemantic: async ({ value }, context) => {
            if (context.canCommit())
                session.edit(session.locate(0), {
                    role: 'char',
                    data: value,
                    chatId: 'stream-output',
                })
        },
        processPreview: async () => {
            throw new Error('balanced must use semantic processing')
        },
    })
    try {
        const body = () => target.querySelector('[data-lua-body][data-index="0"]')
        await vi.waitFor(() => expect(body()?.textContent).toContain('SYNTHETIC_LUA_OK 1'))
        const initialBody = body()
        for (const value of ['first streamed words', 'first streamed words plus more']) {
            streamController.enqueue(value)
            await vi.waitFor(() => expect(body()?.textContent).toBe(`${value}\nSYNTHETIC_LUA_OK 1`))
            expect(body()).toBe(initialBody)
        }
        streamController.close()
        expect((await consume).completed).toBe(true)
        expect(scriptingState.parses).toBeLessThanOrEqual(6)
    } finally {
        abort.abort()
        await consume
        await unmount(mounted)
        source.dispose()
        scriptingState.parseBudget = Infinity
        scriptingState.session = null
        target.remove()
    }
})

test.each([0, 12])(
    'waits for a complete lease before a Lua greeting (%i history rows)',
    async (totalMessages) => {
        const metadataCharacter = makeMetadataOnlyCharacter()
        const completeCharacter = makeCharacter(
            Array.from({ length: totalMessages }, (_, index) => ({
                role: 'user',
                data: 'synthetic message',
                chatId: `synthetic-message-${index}`,
            })),
        )
        scriptingState.database = {
            characters: [metadataCharacter],
            templateDefaultVariables: '',
        }
        scriptingState.session = null
        scriptingState.metadataReads = 0
        scriptingState.parses = 0
        let finishPreparation!: (lease: { release(): void }) => void
        const release = vi.fn()
        const acquireConversationStartParserLease = vi.fn(
            () =>
                new Promise<{ release(): void }>((resolve) => {
                    finishPreparation = resolve
                }),
        )
        const target = document.createElement('div')
        document.body.appendChild(target)
        const mounted = mount(ChatConversationStart, {
            target,
            props: {
                currentCharacter: metadataCharacter,
                resolvedImage: '',
                showAiWarning: false,
                totalMessages,
                onReroll: () => {},
                unReroll: () => {},
                onRemoveCreatorQuote: () => {},
                acquireConversationStartParserLease,
            },
        })
        try {
            await vi.waitFor(() =>
                expect(
                    acquireConversationStartParserLease,
                ).toHaveBeenCalledOnce(),
            )
            await tick()
            expect(scriptingState.parses).toBe(0)
            expect(scriptingState.metadataReads).toBe(0)
            expect(metadataCharacter.chats[0].scriptstate).toBeUndefined()
            scriptingState.database.characters = [completeCharacter]
            scriptingState.session = new ActiveConversationSession({
                characterId: completeCharacter.chaId,
                conversationId: completeCharacter.chats[0].id!,
                conversation: completeCharacter.chats[0],
                storeRevision: 1,
            })
            finishPreparation({ release })
            await vi.waitFor(() =>
                expect(target.textContent).toContain(
                    `SYNTHETIC_LUA_OK ${totalMessages}`,
                ),
            )
            expect(scriptingState.metadataReads).toBe(0)
            expect(metadataCharacter.chats[0].scriptstate).toBeUndefined()
            expect(completeCharacter.chats[0].scriptstate).toEqual({
                $__synthetic_initialized: '1',
            })
            expect(release).not.toHaveBeenCalled()
        } finally {
            await unmount(mounted)
            target.remove()
            scriptingState.session = null
        }
        expect(release).toHaveBeenCalledOnce()
    },
)
test('runs the same greeting with complete session history', async () => {
    const character = makeCharacter([
        {
            role: 'user',
            data: 'synthetic message',
            chatId: 'synthetic-message',
        },
    ])
    scriptingState.database = {
        characters: [character],
        templateDefaultVariables: '',
    }
    scriptingState.session = new ActiveConversationSession({
        characterId: character.chaId,
        conversationId: character.chats[0].id!,
        conversation: character.chats[0],
        storeRevision: 1,
    })
    const target = document.createElement('div')
    document.body.appendChild(target)
    const mounted = mount(ChatConversationStart, {
        target,
        props: {
            currentCharacter: character,
            resolvedImage: '',
            showAiWarning: false,
            totalMessages: 1,
            onReroll: () => {},
            unReroll: () => {},
            onRemoveCreatorQuote: () => {},
        },
    })
    try {
        await vi.waitFor(() =>
            expect(target.textContent).toContain('SYNTHETIC_LUA_OK 1'),
        )
        expect(character.chats[0].scriptstate).toEqual({
            $__synthetic_initialized: '1',
        })
        expect(scriptingState.session.version).toBe(1)
    } finally {
        await unmount(mounted)
        target.remove()
        scriptingState.session = null
    }
})

test('releases a late greeting lease after the component is destroyed', async () => {
    const character = makeMetadataOnlyCharacter()
    scriptingState.database = {
        characters: [character],
        templateDefaultVariables: '',
    }
    scriptingState.session = null
    scriptingState.parses = 0
    let finishPreparation!: (lease: { release(): void }) => void
    const release = vi.fn()
    const acquireConversationStartParserLease = vi.fn(
        () =>
            new Promise<{ release(): void }>((resolve) => {
                finishPreparation = resolve
            }),
    )
    const target = document.createElement('div')
    document.body.appendChild(target)
    const mounted = mount(ChatConversationStart, {
        target,
        props: {
            currentCharacter: character,
            resolvedImage: '',
            showAiWarning: false,
            totalMessages: 0,
            onReroll: () => {},
            unReroll: () => {},
            onRemoveCreatorQuote: () => {},
            acquireConversationStartParserLease,
        },
    })
    await vi.waitFor(() =>
        expect(acquireConversationStartParserLease).toHaveBeenCalledOnce(),
    )
    await unmount(mounted)
    finishPreparation({ release })
    await vi.waitFor(() => expect(release).toHaveBeenCalledOnce())
    expect(scriptingState.parses).toBe(0)
    target.remove()
})

test('retries a failed greeting lease without touching metadata-only state', async () => {
    const metadataCharacter = makeMetadataOnlyCharacter()
    const completeCharacter = makeCharacter([
        {
            role: 'user',
            data: 'synthetic retry message',
            chatId: 'retry-message',
        },
    ])
    scriptingState.database = {
        characters: [metadataCharacter],
        templateDefaultVariables: '',
    }
    scriptingState.session = null
    scriptingState.parses = 0
    scriptingState.metadataReads = 0
    const release = vi.fn()
    const acquireConversationStartParserLease = vi
        .fn<() => Promise<{ release(): void }>>()
        .mockRejectedValueOnce(new Error('synthetic lease preparation failed'))
        .mockImplementationOnce(async () => {
            scriptingState.database.characters = [completeCharacter]
            scriptingState.session = new ActiveConversationSession({
                characterId: completeCharacter.chaId,
                conversationId: completeCharacter.chats[0].id!,
                conversation: completeCharacter.chats[0],
                storeRevision: 1,
            })
            return { release }
        })
    const target = document.createElement('div')
    document.body.appendChild(target)
    const mounted = mount(ChatConversationStart, {
        target,
        props: {
            currentCharacter: metadataCharacter,
            resolvedImage: '',
            showAiWarning: false,
            totalMessages: 1,
            onReroll: () => {},
            unReroll: () => {},
            onRemoveCreatorQuote: () => {},
            acquireConversationStartParserLease,
        },
    })
    try {
        await vi.waitFor(() =>
            expect(
                target.querySelector('[data-chat-greeting-load-error]'),
            ).not.toBeNull(),
        )
        expect(acquireConversationStartParserLease).toHaveBeenCalledOnce()
        expect(scriptingState.parses).toBe(0)
        expect(scriptingState.metadataReads).toBe(0)
        expect(metadataCharacter.chats[0].scriptstate).toBeUndefined()
        target
            .querySelector<HTMLButtonElement>(
                '[data-chat-greeting-load-error] button',
            )!
            .click()
        await vi.waitFor(() =>
            expect(target.textContent).toContain('SYNTHETIC_LUA_OK 1'),
        )
        expect(acquireConversationStartParserLease).toHaveBeenCalledTimes(2)
        expect(
            target.querySelector('[data-chat-greeting-load-error]'),
        ).toBeNull()
        expect(scriptingState.metadataReads).toBe(0)
        expect(metadataCharacter.chats[0].scriptstate).toBeUndefined()
        expect(completeCharacter.chats[0].scriptstate).toEqual({
            $__synthetic_initialized: '1',
        })
        expect(release).not.toHaveBeenCalled()
    } finally {
        await unmount(mounted)
        target.remove()
        scriptingState.session = null
    }
    expect(release).toHaveBeenCalledOnce()
})
