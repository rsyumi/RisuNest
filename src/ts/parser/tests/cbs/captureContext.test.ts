// @vitest-environment jsdom

import { writable } from 'svelte/store'
import { beforeEach, expect, test, vi } from 'vitest'
import type { Database, Message, character, customscript } from '../../../storage/database.svelte'
import type { ChatScreenshotRenderContext } from '../../../chatScreenshotRange'
import { planChatParserHistory } from '../../../chatParserHistory'
import { captureResidentCbsHistory, prepareCbsHistory, type CbsHistoryReader } from '../../../prepareCbsHistory'
import { createCbsHistoryAccess, CbsHistoryAccessError } from '../../../cbsHistoryAccess'
import { RevisionConflictError } from '../../../storage/persistentDataStore'
import type { PersistentDataRuntime } from '../../../storage/persistentDataRuntime'
import { RegexExecutor } from '../../../process/regexExecutor'
import { getRegexExecutionPlan } from '../../../process/regexExecutionPlan'
import { RegexWorkerClient } from '../../../process/regexWorkerClient'
import { InMemoryWorker } from '../../../../../tests/support/regexWorker'
import { authorNotePresetLinkPatch } from '../../../authorNotePresets'

vi.mock('../../../platform', async () => (await import('../../../platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))

const live = vi.hoisted(() => ({
    database: {} as Database,
    modules: [] as Array<{ id: string; name: string; namespace?: string }>,
    runtime: null as unknown as PersistentDataRuntime,
}))
const bergamotTranslate = vi.hoisted(() => vi.fn(async (
    _text: string,
    from: string,
    to: string,
) => `${from}->${to}`))

vi.mock('../../../storage/database.svelte', () => ({
    appVer: '1.0.0',
    getDatabase: () => live.database,
    getCurrentCharacter: () => live.database.characters[0],
    getCurrentChat: () => live.database.characters[0]?.chats[0],
}))
vi.mock('../../../stores.svelte', () => ({
    DBState: { get db() { return live.database } },
    selIdState: { selId: 0 },
    selectedCharID: writable(0),
    CharEmotion: writable({}),
    CurrentTriggerIdStore: writable(null),
}))
vi.mock('../../../globalApi.svelte', () => ({
    aiWatermarkingLawApplies: () => false,
    downloadFile: vi.fn(),
    globalFetch: vi.fn(),
    getFileSrc: async (source: string) => source,
}))
vi.mock('../../../process/modules', () => ({
    getModules: () => live.modules,
    getModuleLorebooks: () => [],
    getModuleAssets: () => [],
    getModuleRegexScripts: () => [],
    getModuleTriggers: () => [],
}))
vi.mock('../../../process/memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('../../../process/scriptings', () => ({ runLuaEditTrigger: vi.fn() }))
vi.mock('../../../plugins/plugins.svelte', () => ({
    pluginV2: { editinput: new Set(), editoutput: new Set(), editprocess: new Set(), editdisplay: new Set() },
}))
vi.mock('../../../process/triggers', () => ({ runTrigger: vi.fn() }))
vi.mock('../../../alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('../../../process/index.svelte', () => ({ doingChat: writable(false) }))
vi.mock('../../../storage/persistentDataRuntime.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('../../../storage/persistentDataRuntime.svelte')>(),
    getPersistentStorageAuthorityEpoch: () => 0,
    assertPersistentMutationAllowed: () => {},
    peekActiveConversationSession: () => null,
    getPersistentDataRuntime: () => live.runtime,
}))
vi.mock('../../../process/request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('../../../translator/bergamotTranslator', () => ({ bergamotTranslate }))

const { ParseMarkdown, risuChatParser, trimMarkdown } = await import('../../parser.svelte')
const { processScriptFull } = await import('../../../process/scripts')
const {
    createChatScreenshotDialogSnapshot,
    createChatScreenshotJobFromDialogSnapshot,
    snapshotChatScreenshotCharacter,
} = await import('../../../chatScreenshotRange')
const { clearLLMCache, setLLMCache, translate, translateHTML } = await import('../../../translator/translator')
const { runLuaEditTrigger } = await import('../../../process/scriptings')
const { parseRisuChat } = await import('../../../plugins/parseRisuChat')
const { pluginV2 } = await import('../../../plugins/plugins.svelte')
const { createMetadataOnlySelectedConversation } = await import('../../../storage/selectedConversationLifecycle')
const { acquireLiveDisplayParserLease, subscribeLiveDisplayParserSelection, captureLiveDisplayParserInputs } = await import('../../../liveDisplayParserLease')
const { EditableViewportFixture: PersistentConversationViewportSource } = await import('../../../conversationViewportSource.testSupport')
const { installDisplayRecovery } = await import('../../../displayRecovery.svelte')
const { manualTimers, recoveryStore } = await import('../../../displayRecovery.testSupport')

function makeCharacter(name: string, messages = [
    { role: 'user' as const, data: `${name} previous` },
    { role: 'char' as const, data: `${name} current` },
]): character {
    return {
        type: 'character',
        name,
        nickname: name,
        chaId: name.toLocaleLowerCase(),
        firstMessage: `${name} first`,
        alternateGreetings: [],
        chats: [{
            message: messages,
            note: '',
            name: '',
            localLore: [],
            fmIndex: -1,
            scriptstate: { $score: `${name} score` },
        }],
        chatPage: 0,
        customscript: [],
        globalLore: [],
        personality: '',
        desc: '',
        scenario: '',
        exampleMessage: '',
        defaultVariables: '',
    } as unknown as character
}

function makeDatabase(character: character, prefix: string): Database {
    return {
        characters: [character],
        username: `${prefix} user`,
        personaPrompt: `${prefix} persona`,
        mainPrompt: `${prefix} main`,
        globalChatVariables: { world: `${prefix} world` },
    } as unknown as Database
}

async function createProductScreenshotJob(input: {
    characterId: string
    chatId: string
    messages: Message[]
    start: number
    end: number
    renderContext: ChatScreenshotRenderContext
    bodyIndices?: number[]
}) {
    const parserContext = input.renderContext.parserContext
    const sourceCharacter = parserContext.character
    const sourceChat = sourceCharacter.chats[sourceCharacter.chatPage]
    const captureCharacter = snapshotChatScreenshotCharacter(sourceCharacter, sourceChat, parserContext.database.authorNotePresets)
    const renderContext: ChatScreenshotRenderContext = {
        ...input.renderContext,
        parserContext: {
            ...parserContext,
            character: captureCharacter,
            database: {
                ...parserContext.database,
                characters: parserContext.database.characters.map((candidate, index) =>
                    index === parserContext.selectedCharID ? captureCharacter : candidate,
                ),
            },
        },
    }
    const frozenMessages = structuredClone(input.messages)
    const reader = {
        characterId: input.characterId,
        chatId: input.chatId,
        revision: 1,
        totalTurns: frozenMessages.length,
        async readRange(startIndex: number, limit: number) {
            for (let index = startIndex; index < startIndex + limit; index++) input.bodyIndices?.push(index)
            return structuredClone(frozenMessages.slice(startIndex, startIndex + limit))
        },
    }
    const snapshot = createChatScreenshotDialogSnapshot({
        characterId: reader.characterId,
        chatId: reader.chatId,
        revision: reader.revision,
        sessionVersion: 1,
        totalTurns: reader.totalTurns,
        renderContext,
    })
    return createChatScreenshotJobFromDialogSnapshot(
        snapshot,
        reader,
        input.start,
        input.end,
        undefined,
        risuChatParser,
    )
}

beforeEach(() => {
    live.database = makeDatabase(makeCharacter('Live'), 'Live')
    live.modules = [{ id: 'live', name: 'Live', namespace: 'live-module' }]
    pluginV2.editprocess.clear()
    vi.mocked(runLuaEditTrigger).mockImplementation(async (_char, _mode, content) => content)
})

test('Plugin parsing keeps upstream defaults, condition precedence and variable effects', async () => {
    expect(await parseRisuChat('{{previoususerchat}}|{{lastmessageindex}}')).toBe('|1')
    expect(await parseRisuChat('{{role}}', { role: 'user', cbsConditions: { chatRole: 'char' } })).toBe('char')
    await parseRisuChat('{{setvar::score::written}}', { runVar: true })
    expect(live.database.characters[0].chats[0].scriptstate.$score).toBe('written')
    await expect(parseRisuChat('', { messageIndex: 2 })).rejects.toThrow('Invalid messageIndex: 2')
    await expect(parseRisuChat('', { messageIndex: 0.5 })).rejects.toThrow('Invalid messageIndex: 0.5')
})

test('Plugin editprocess prepares the second CBS pass, permits reentrant calls and retains its target', async () => {
    const original = live.database.characters[0]
    original.chats[0].scriptstate.$expanded = '{{lastmessage}}'
    let entered!: () => void
    const inside = new Promise<void>((resolve) => { entered = resolve })
    let resume!: () => void
    const wait = new Promise<void>((resolve) => { resume = resolve })
    pluginV2.editprocess.add(async (data) => {
        expect(await parseRisuChat('{{lastmessageindex}}')).toBe('1')
        entered()
        await wait
        return data
    })
    const pending = parseRisuChat('{{getvar::expanded}}', { processRegex: true })
    await inside
    live.database.characters[0] = makeCharacter('Navigated')
    resume()
    expect(await pending).toBe('Live current')
})

test.each(['navigate', 'stream', 'promote', 'unload'])('Plugin parsing retains its captured window and releases its lease after %s', async (action) => {
    const character = live.database.characters[0]
    const messages = character.chats[0].message
    character.chats[0].id = 'original-conversation'
    const completeChat = character.chats[0]
    character.chats[0] = createMetadataOnlySelectedConversation(character.chats[0])
    let finish!: () => void
    const waiting = new Promise<void>((resolve) => { finish = resolve })
    const release = vi.fn(async () => {})
    const controller = new AbortController()
    live.runtime = {
        captureSelectedConversationAuthority: () => ({ totalMessages: 2, storeRevision: 1 }),
        getActiveConversationViewportSource: () => ({ captureResidentRows: () => [{ absoluteIndex: 1, message: messages[1] }] }),
        store: { acquireRevision: async () => ({ revision: 1, release,
            readConversationWindow: async (query) => {
                await waiting
                return { revision: 1, value: {
                    characterId: character.chaId, conversationId: 'original-conversation',
                    startIndex: query.startIndex, endIndex: query.startIndex + query.limit,
                    totalMessages: 2, messages: messages.slice(query.startIndex, query.startIndex + query.limit),
                    hasMoreBefore: query.startIndex > 0, hasMoreAfter: false,
                } }
            },
        }) },
    } as unknown as PersistentDataRuntime
    const pending = parseRisuChat('{{previouschatlog::0}}|{{lastmessage}}', undefined, controller.signal)
    if (action === 'navigate') live.database.characters[0] = makeCharacter('Navigated')
    if (action === 'stream') messages[1].data = 'next stream token'
    let promoted: Promise<string> | undefined
    if (action === 'promote') {
        character.chats[0] = completeChat
        promoted = parseRisuChat('{{lastmessage}}')
        messages[1].data = 'next stream token'
    }
    if (action === 'unload') controller.abort()
    finish()
    if (action === 'unload') await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
    else expect(await pending).toBe('Live previous|Live current')
    if (promoted) expect(await promoted).toBe('Live current')
    expect(release).toHaveBeenCalledOnce()
    if (action === 'stream') expect(await parseRisuChat('{{lastmessage}}')).toBe('next stream token')
})

test.each(['acquire', 'read'])('Plugin parsing maps a conflicting %s to a target change', async (stage) => {
    const character = live.database.characters[0]
    character.chats[0].id = 'conflicting-conversation'
    character.chats[0] = createMetadataOnlySelectedConversation(character.chats[0])
    let released = false
    live.runtime = {
        captureSelectedConversationAuthority: () => ({ totalMessages: 2, storeRevision: 1 }),
        getActiveConversationViewportSource: () => null,
        store: { acquireRevision: async () => {
            if (stage === 'acquire') throw new RevisionConflictError(1, 2)
            return {
                revision: 1,
                readConversationWindow: async () => { throw new RevisionConflictError(1, 2) },
                release: async () => { released = true },
            }
        } },
    } as unknown as PersistentDataRuntime
    await expect(parseRisuChat('{{previouschatlog::0}}')).rejects.toMatchObject({ name: 'PluginParserTargetChangedError' })
    expect(released).toBe(stage === 'read')
})

test.each([new Error('primary read failed'), 0, undefined])('Plugin parsing preserves a read error %s when lease cleanup fails', async (primary) => {
    const character = live.database.characters[0]
    character.chats[0].id = 'cleanup-conversation'
    character.chats[0] = createMetadataOnlySelectedConversation(character.chats[0])
    let released = false
    live.runtime = {
        captureSelectedConversationAuthority: () => ({ totalMessages: 2, storeRevision: 1 }),
        getActiveConversationViewportSource: () => null,
        store: { acquireRevision: async () => ({
            revision: 1,
            readConversationWindow: async () => { throw primary },
            release: async () => { released = true; throw new Error('cleanup failed') },
        }) },
    } as unknown as PersistentDataRuntime
    await expect(parseRisuChat('{{previouschatlog::0}}')).rejects.toBe(primary)
    expect(released).toBe(true)
})

test('Plugin regex passes read only the history exposed by their actual output', async () => {
    const character = live.database.characters[0]
    const messages: Message[] = Array.from({ length: 80 }, (_, index) => ({ role: 'char', data: `row ${index}` }))
    character.chats[0].id = 'chat'
    character.chats[0] = createMetadataOnlySelectedConversation(character.chats[0])
    character.chats[0].scriptstate.$expanded = '{{previouschatlog::70}}'
    character.customscript = [{ type: 'editprocess', in: 'HP', out: '<b>HP</b>', flag: 'g', ableFlag: true }] as any
    const indices: number[] = []
    const release = vi.fn(async () => {})
    live.runtime = {
        captureSelectedConversationAuthority: () => ({ totalMessages: messages.length, storeRevision: 1 }),
        getActiveConversationViewportSource: () => undefined,
        store: { acquireRevision: async () => ({ revision: 1, release,
            readConversationWindow: async (query) => {
                const start = query.startIndex!, end = start + query.limit!
                for (let index = start; index < end; index++) indices.push(index)
                return { revision: 1, value: { characterId: character.chaId, conversationId: 'chat',
                    startIndex: start, endIndex: end, totalMessages: messages.length,
                    messages: messages.slice(start, end), hasMoreBefore: start > 0, hasMoreAfter: end < messages.length } }
            },
        }) },
    } as unknown as PersistentDataRuntime
    expect(await parseRisuChat('{{#when::1::is::1}}HP{{/when}}|{{lastmessageindex}}', { processRegex: true })).toBe('<b>HP</b>\n|79')
    expect(indices).toEqual([])
    expect(await parseRisuChat('{{getvar::expanded}}', { processRegex: true })).toBe('row 70')
    expect(indices).toEqual([70])
    indices.length = 0
    character.customscript = [{ type: 'editprocess', in: 'H([0-9]+)', out: '{{previouschatlog::$1}}', flag: 'g', ableFlag: true }] as any
    expect(await parseRisuChat('H2', { processRegex: true })).toBe('row 2')
    expect(indices).toEqual([2])
    messages[2].data = 'updated row 2'
    expect(await parseRisuChat('H2', { processRegex: true })).toBe('updated row 2')
    expect(indices).toEqual([2, 2])
    expect(release).toHaveBeenCalledTimes(4)
})

test('prepared CBS reads match complete history for composed expressions and sparse rows', async () => {
    const messages: Message[] = Array.from({ length: 80 }, (_, index) => ({
        role: index % 3 ? 'char' : 'user', data: `turn ${index}`, time: 1_000_000 + index * 1000,
    }))
    const character = makeCharacter('Prepared', messages)
    character.chats[0].id = 'conversation'
    const db = makeDatabase(character, 'Prepared')
    const readIndices: number[] = []
    const reader: CbsHistoryReader = {
        revision: 1,
        async readConversationWindow(query) {
            const start = query.startIndex!
            const end = start + query.limit!
            for (let i = start; i < end; i++) readIndices.push(i)
            return { revision: 1, value: {
                characterId: character.chaId, conversationId: 'conversation',
                messages: messages.slice(start, end), startIndex: start, endIndex: end,
                totalMessages: messages.length, hasMoreBefore: start > 0, hasMoreAfter: end < messages.length,
            } }
        },
    }
    const expressions = [
        'plain text', '{{char}}', '{{lastmessageindex}}', '{{pick::a::b::c}}',
        '{{previouschatlog::2}}|{{previous_chat_log::70}}', '{{lastmessage}}',
        '{{previouscharchat}}', '{{previoususerchat}}', '{{messageidleduration}}',
        '{{messagetime}}|{{messagedate}}|{{role}}', '{{idleduration}}',
        '{#lastmessage}}', '{#previouscharchat}}', '{#history}}',
        '{{history}}', '{{userhistory}}', '{{charhistory}}', '{{messageunixtimearray}}',
        '{{{{getvar::macro}}}}', '{{#if 1}}{{lastmessage}}{{/if}}',
        '{{#pure}}{{lastmessage}}{{/pure}}', '{{#each {{array::a::b}}}}{{previouscharchat}}{{/each}}',
        '{{#when::1::is::1}}{{previouschatlog::2}}{{/when}}',
        '{{#escape}}{{history}}{{/escape}}', '{{#pure}}{{#if 1}}{{history}}{{/if}}{{/pure}}',
        '{{#func::f}}{{lastmessage}}{{/func}}{{call::f}}', '<user>|<char>',
        '{{#if {{getvar::flag}}}}{{lastmessage}}{{/if}}', '{{#when {{getvar::flag}}}}{{previouscharchat}}{{/when}}',
        '{{? {{getvar::hp}}+{{lastmessageindex}}}}', '{{last message{{getvar::empty}}}}',
        '<risu-style>7b7b6c6173746d657373616765696e6465787d7d</risu-style>',
    ]
    for (const chatID of [-1, 0, 16, 79]) {
        for (const [index, expression] of expressions.entries()) {
            const source = `${expression}|${expressions[(index + 3) % expressions.length]}`
            const context = { db, chara: character, chatID, selectedCharID: 0,
                chatVariables: { macro: 'lastmessage', flag: '1', hp: '3', empty: '' }, currentTime: 2_000_000 }
            const expected = risuChatParser(source, { ...context })
            const historyAccess = await prepareCbsHistory({ characterId: character.chaId,
                conversationId: 'conversation', totalMessages: messages.length, chatID,
                plan: planChatParserHistory({ source }), reader, pageSize: 4 })
            const actual = risuChatParser(source, { ...context, historyAccess })
            historyAccess.assertComplete()
            expect(actual, `${chatID}: ${source}`).toBe(expected)
        }
    }
    readIndices.length = 0
    const source = '{{previouschatlog::2}}|{{previouschatlog::70}}'
    const historyAccess = await prepareCbsHistory({ characterId: character.chaId,
        conversationId: 'conversation', totalMessages: messages.length, chatID: -1,
        plan: planChatParserHistory({ source }), reader })
    expect(risuChatParser(source, { db, chara: character, historyAccess })).toBe('turn 2|turn 70')
    expect(readIndices).toEqual([2, 70])
    readIndices.length = 0
    await prepareCbsHistory({ characterId: character.chaId, conversationId: 'conversation',
        totalMessages: messages.length, chatID: 16,
        plan: planChatParserHistory({ source: '{{lastmessageindex}}|{{pick::a::b}}' }), reader })
    expect(readIndices).toEqual([])
    for (const [source, index, affected] of [
        ['plain', 79, false], ['{{lastmessageindex}}', 79, false],
        ['{{previouschatlog::2}}', 79, false], ['{{previouschatlog::2}}', 2, true],
        ['{{lastmessage}}', 78, false], ['{{lastmessage}}', 79, true],
        ['{{previouscharchat}}', 13, false], ['{{previouscharchat}}', 15, true],
        ['{{messageidleduration}}', 11, false], ['{{messageidleduration}}', 12, true],
    ] as const) {
        const access = await prepareCbsHistory({ characterId: character.chaId, conversationId: 'conversation',
            totalMessages: messages.length, chatID: 16, plan: planChatParserHistory({ source }), reader })
        expect(access.dependencies.affectedBy([{ start: index, end: index + 1, countChanged: false }]), source).toBe(affected)
    }
    const count = captureResidentCbsHistory(messages, planChatParserHistory({ source: '{{lastmessageindex}}' }), -1)
    expect(count.dependencies.affectedBy([{ start: 80, end: 81, countChanged: true }])).toBe(true)
})

test.each(['retained', 'first pass'])('fully loaded live displays survive a commit during %s preparation', async (timing) => {
    const character = live.database.characters[0]
    character.chats[0].id = 'chat'
    let revision = 1
    const source = new PersistentConversationViewportSource({ characterId: character.chaId,
        conversationId: 'chat', revision, totalMessages: 2, rowBudget: 8,
        reader: { readConversationWindow: async () => { throw new Error('Unexpected persisted history read') } } })
    live.runtime = {
        captureSelectedConversationTarget: () => ({ characterId: character.chaId, conversationId: 'chat', storeRevision: revision, navigationGeneration: 0 }),
        captureSelectedConversationAuthority: () => null,
        getActiveConversationViewportSource: () => source,
        acquireCompleteConversation: () => { throw new Error('Unexpected complete promotion') },
    } as unknown as PersistentDataRuntime
    const onStale = vi.fn()
    const pending = acquireLiveDisplayParserLease({ source: '{{previouschatlog::0}}', character,
        signal: new AbortController().signal, parseFirst: true, chatID: -1, onStale })
    const retained = timing === 'retained' ? await pending : undefined
    revision = 2
    source.advanceUnchangedRevision(revision)
    const lease = retained ?? await pending
    try {
        expect(lease?.projection?.parsedSource).toBe('Live previous')
        const access = await lease!.projection!.context.parserContext.prepareHistory!('{{lastmessage}}', -1, false)
        expect(access.row(1)?.data).toBe('Live current')
        expect(onStale).not.toHaveBeenCalled()
    } finally {
        lease?.release()
        source.dispose()
    }
})

test.each([false, true])('initial live display preparation retries a revision-only advance unless aborted (%s)', async (abort) => {
    const character = live.database.characters[0]
    const messages = character.chats[0].message
    character.chats[0].id = 'chat'
    character.chats[0] = createMetadataOnlySelectedConversation(character.chats[0])
    let revision = 1
    let started!: () => void
    const reading = new Promise<void>((resolve) => { started = resolve })
    let finish!: () => void
    const pendingRead = new Promise<void>((resolve) => { finish = resolve })
    const readConversationWindow: CbsHistoryReader['readConversationWindow'] = async (query) => {
        const readRevision = revision
        if (readRevision === 1) {
            started()
            await pendingRead
        }
        if (readRevision !== revision) throw new RevisionConflictError(readRevision, revision)
        const start = query.startIndex!, end = start + query.limit!
        return { revision, value: { characterId: character.chaId, conversationId: 'chat',
            startIndex: start, endIndex: end, totalMessages: messages.length, messages: messages.slice(start, end),
            hasMoreBefore: start > 0, hasMoreAfter: end < messages.length } }
    }
    const source = new PersistentConversationViewportSource({ characterId: character.chaId,
        conversationId: 'chat', revision, totalMessages: messages.length, rowBudget: 8, reader: { readConversationWindow } })
    live.runtime = {
        captureSelectedConversationTarget: () => ({ characterId: character.chaId, conversationId: 'chat', storeRevision: revision, navigationGeneration: 0 }),
        captureSelectedConversationAuthority: () => ({ totalMessages: source.snapshot().totalMessages, storeRevision: revision }),
        getActiveConversationViewportSource: () => source,
        store: { acquireRevision: async (expected: number) => {
            if (expected !== revision) throw new RevisionConflictError(expected, revision)
            return { revision, readConversationWindow, release: async () => {} }
        } },
        acquireCompleteConversation: () => { throw new Error('Unexpected complete promotion') },
    } as unknown as PersistentDataRuntime
    const controller = new AbortController()
    const onStale = vi.fn()
    try {
        const pending = acquireLiveDisplayParserLease({ source: '{{previouschatlog::0}}', character,
            signal: controller.signal, parseFirst: true, chatID: -1, onStale })
        const rejected = expect(pending).rejects.toThrow(RevisionConflictError)
        await reading
        revision = 2
        source.advanceUnchangedRevision(revision)
        if (abort) controller.abort()
        finish()
        await rejected
        expect(onStale).toHaveBeenCalledTimes(abort ? 0 : 1)

        const fresh = await acquireLiveDisplayParserLease({ source: '{{previouschatlog::0}}', character,
            signal: new AbortController().signal, parseFirst: true, chatID: -1, onStale })
        try {
            expect(fresh?.projection?.parsedSource).toBe('Live previous')
            expect(fresh?.projection?.revision).toBe(2)
            expect(onStale).toHaveBeenCalledTimes(abort ? 0 : 1)
        } finally { fresh?.release() }
    } finally {
        controller.abort()
        finish()
        source.dispose()
    }
})

test('background and bookmark contexts read precise history and refresh on unpersisted tail edits', async () => {
    const character = live.database.characters[0]
    const messages = character.chats[0].message
    character.chats[0].id = 'chat'
    character.chats[0] = createMetadataOnlySelectedConversation(character.chats[0])
    const bodyIndices: number[] = []
    const readConversationWindow: CbsHistoryReader['readConversationWindow'] = async (query) => {
        const start = query.startIndex!, end = start + query.limit!
        for (let index = start; index < end; index++) bodyIndices.push(index)
        return { revision, value: { characterId: character.chaId, conversationId: 'chat',
            startIndex: start, endIndex: end, totalMessages: 2, messages: messages.slice(start, end),
            hasMoreBefore: start > 0, hasMoreAfter: end < 2 } }
    }
    const source = new PersistentConversationViewportSource({ characterId: character.chaId,
        conversationId: 'chat', revision: 1, totalMessages: 2, rowBudget: 8, reader: { readConversationWindow } })
    let revision = 1
    const release = vi.fn(async () => {})
    const acquireRevision = vi.fn(async (expected) => {
        if (expected !== revision) throw new Error('RevisionConflict')
        return { revision, readConversationWindow, release }
    })
    live.runtime = {
        captureSelectedConversationTarget: () => ({ characterId: character.chaId, conversationId: 'chat', storeRevision: revision, navigationGeneration: 0 }),
        captureSelectedConversationAuthority: () => ({ totalMessages: source.snapshot().totalMessages, storeRevision: revision }),
        getActiveConversationViewportSource: () => source,
        subscribeActiveConversationViewportSource: (listener) => { listener(source); return () => {} },
        store: { acquireRevision },
        acquireCompleteConversation: () => { throw new Error('Unexpected complete promotion') },
    } as unknown as PersistentDataRuntime
    const observed = vi.fn()
    const unsubscribe = subscribeLiveDisplayParserSelection(observed)
    const prepare = (text: string, parseFirst = true) => acquireLiveDisplayParserLease({
        source: text, character, signal: new AbortController().signal, parseFirst, chatID: parseFirst ? -1 : 1,
    })
    const revisionStale = vi.fn()
    const retained = await acquireLiveDisplayParserLease({ source: 'plain', character,
        signal: new AbortController().signal, parseFirst: true, chatID: -1, onStale: revisionStale })
    const silent = vi.fn()
    const stopSilent = source.subscribe(silent)
    silent.mockClear()
    revision = 2
    source.advanceUnchangedRevision(revision)
    expect(silent).not.toHaveBeenCalled()
    expect(revisionStale).not.toHaveBeenCalled()
    await expect(retained!.projection!.context.parserContext.prepareHistory!('{{previouschatlog::0}}', -1, false)).rejects.toThrow()
    expect(revisionStale).toHaveBeenCalledOnce()
    retained?.release()
    const fresh = await prepare('{{previouschatlog::0}}')
    expect(fresh?.projection?.parsedSource).toBe('Live previous')
    fresh?.release()
    stopSilent()
    bodyIndices.length = 0
    acquireRevision.mockClear()
    character.customscript = [{ type: 'editdisplay', in: 'HP', out: '<b>HP</b>', flag: 'g', ableFlag: true }] as any
    const plain = await prepare('{{#when::1::is::1}}HP{{/when}}')
    expect(await ParseMarkdown(plain!.projection!.parsedSource!, character, 'back', -1, {}, {
        scriptContext: plain!.projection!.context,
    })).toBe('<b>HP</b>\n')
    expect(bodyIndices).toEqual([])
    // Each regex pass prepares its output, but none of them needs a stored row.
    expect(acquireRevision).not.toHaveBeenCalled()
    plain?.release()
    await expect(plain!.projection!.context.parserContext.prepareHistory!('{{lastmessage}}', -1, false)).rejects.toThrow('Live display lease is closed')
    for (const name of ['lastmessage', 'previouscharchat']) {
        const legacy = await prepare(`{#${name}}}`)
        expect(legacy?.projection?.parsedSource).toBe('Live current')
        legacy?.release()
    }
    const background = await prepare('{{previouscharchat}}|{{lastmessageindex}}')
    expect(background?.projection?.parsedSource).toBe('Live current|1')
    const inputs = captureLiveDisplayParserInputs('{{getvar::live}}', character)
    character.chats[0].scriptstate.$live = 'new variable value'
    expect(captureLiveDisplayParserInputs('{{getvar::live}}', character)).not.toEqual(inputs)
    expect((await prepare('{{getvar::live}}'))?.projection?.parsedSource).toBe('new variable value')
    const before = observed.mock.lastCall?.[1]
    const fixed = await prepare('{{previouschatlog::0}}', false)
    const fixedUpdates = vi.fn()
    const unsubscribeFixed = subscribeLiveDisplayParserSelection(fixedUpdates, () => fixed?.projection?.context.parserContext.historyAccess?.dependencies)
    fixedUpdates.mockClear()
    observed.mockClear()
    source.applyOptimisticRange(1, 1, [{ role: 'char', data: 'streamed tail' }])
    expect(fixedUpdates).not.toHaveBeenCalled()
    expect(observed).toHaveBeenCalledOnce()
    expect(observed.mock.lastCall?.[1]).not.toBe(before)
    expect((await prepare('{{lastmessage}}'))?.projection?.parsedSource).toBe('streamed tail')
    source.applyOptimisticRange(2, 0, [{ role: 'user', data: 'appended' }])
    expect(fixedUpdates).not.toHaveBeenCalled()
    expect((await prepare('{{lastmessage}}|{{lastmessageindex}}'))?.projection?.parsedSource).toBe('appended|2')
    const bookmark = await prepare('{{previouschatlog::0}}', false)
    const parser = bookmark!.projection!.context.parserContext
    expect(risuChatParser('{{previouschatlog::0}}', { ...parser, db: parser.database, chara: parser.character, chatID: 1 })).toBe('Live previous')
    source.applyOptimisticRange(0, 1, [{ role: 'user', data: 'edited dependency' }])
    expect(fixedUpdates).toHaveBeenCalledOnce()
    unsubscribeFixed()
    fixed?.release()
    unsubscribe()
    observed.mockClear()
    source.applyOptimisticRange(2, 1, [{ role: 'user', data: 'later' }])
    expect(observed).not.toHaveBeenCalled()
    expect(bodyIndices.every((index) => index === 0 || index === 1)).toBe(true)
    expect(release).toHaveBeenCalled()
    // A display regex can expose a row only after an edit that the lease's own reads skipped.
    character.customscript = [{ type: 'editdisplay', in: 'HP', out: '{{lastmessage}}', flag: 'g', ableFlag: true }] as any
    const onStale = vi.fn()
    const render = async (lease: Awaited<ReturnType<typeof prepare>>) => ParseMarkdown(lease!.projection!.parsedSource!, character, 'back', -1, {}, {
        scriptContext: lease!.projection!.context,
    })
    const late = await acquireLiveDisplayParserLease({ source: 'HP', character, signal: new AbortController().signal, parseFirst: true, chatID: -1, onStale })
    const lateUpdates = vi.fn()
    const unsubscribeLate = subscribeLiveDisplayParserSelection(lateUpdates, () => late?.projection?.context.parserContext.historyAccess?.dependencies)
    lateUpdates.mockClear()
    source.applyOptimisticRange(3, 0, [{ role: 'char', data: 'late tail' }])
    expect(lateUpdates).not.toHaveBeenCalled()
    await expect(render(late)).rejects.toThrow('Live display conversation changed during preparation')
    expect(onStale).toHaveBeenCalledOnce()
    // The owner hears about it after the parse call stack, which can be a Svelte derived.
    const repeated = late!.projection!.context.parserContext.prepareHistory!('{{lastmessage}}', -1, false)
    expect(onStale).toHaveBeenCalledOnce()
    await expect(repeated).rejects.toThrow('Live display conversation changed during preparation')
    expect(onStale).toHaveBeenCalledTimes(2)
    unsubscribeLate()
    late?.release()
    await expect(late!.projection!.context.parserContext.prepareHistory!('{{lastmessage}}', -1, false)).rejects.toThrow('Live display lease is closed')
    expect(onStale).toHaveBeenCalledTimes(2)
    expect(await render(await prepare('HP'))).toBe('late tail')
    source.dispose()
})

test.each([false, true])('prepares newly exposed screenshot CBS and subsequent style parsing (style %s)', async (style) => {
    const character = makeCharacter('Stages', [
        { role: 'user', data: style ? '{{previouschatlog::2}}' : '{{lastmessage}}' },
        { role: 'char', data: '{{previouschatlog::0}}' },
        ...(style ? [{ role: 'char' as const, data: `<risu-style>${Buffer.from('.sample { content: "{{lastmessage}}"; }').toString('hex')}</risu-style>` }] : []),
        { role: 'char', data: 'captured tail' },
    ])
    const database = makeDatabase(character, 'Stages')
    const context = {
        character: null, characterName: character.name, moduleAssets: [], presetRegex: [], moduleRegexScripts: [],
        parserContext: { database, character, userName: 'User', personaPrompt: '', modules: [],
            moduleLorebooks: [], selectedCharID: 0, chatVariables: {}, globalChatVariables: {}, currentTime: 1 },
        settings: { autoTranslate: false, dynamicAssets: false, dynamicAssetsEditDisplay: false },
    } as unknown as ChatScreenshotRenderContext
    const job = await createProductScreenshotJob({ characterId: character.chaId, chatId: 'chat',
        messages: character.chats[0].message, start: 2, end: 2, renderContext: context })
    const captured = job.renderContext
    expect(captured.preparedDisplayText?.[1]).toEqual({ source: '{{previouschatlog::0}}', text: style ? '{{previouschatlog::2}}' : '{{lastmessage}}' })
    const output = await ParseMarkdown(captured.preparedDisplayText![1].text, character, 'back', 1, {}, {
        scriptContext: { ...captured, dynamicAssets: false, dynamicAssetsEditDisplay: false } as any,
    })
    expect(output).toContain('captured tail')
    expect(() => captured.parserContext.historyAccess!.assertComplete()).not.toThrow()
    if (style) {
        const messages = character.chats[0].message
        const source = captured.preparedDisplayText![1].text
        const liveContext = { ...context, dynamicAssets: false, dynamicAssetsEditDisplay: false,
            parserContext: { ...context.parserContext,
                historyAccess: captureResidentCbsHistory(messages, planChatParserHistory({ source }), 1),
                prepareHistory: async (text: string, index: number) =>
                    captureResidentCbsHistory(messages, planChatParserHistory({ source: text }), index),
            } }
        expect(await ParseMarkdown(source, character, 'back', 1, {}, { scriptContext: liveContext })).toContain('captured tail')
    }
})

test.each([false, true])('screenshots defer regex-created CBS reads until rendering (translation %s)', async (translate) => {
    const messages: Message[] = Array.from({ length: 80 }, (_, index) => ({ role: 'char', data: index === 6 ? 'H2' : `row ${index}` }))
    const character = makeCharacter('Screenshot', messages)
    character.customscript = [{ type: 'editdisplay', in: 'H([0-9]+)', out: '{{previouschatlog::$1}}', flag: 'g', ableFlag: true }] as any
    const database = { ...makeDatabase(character, 'Screenshot'), translatorType: 'bergamot', htmlTranslation: true,
        translator: 'ko', aiModel: 'gpt' } as Database
    const context = { character: null, characterName: character.name, moduleAssets: [], presetRegex: [], moduleRegexScripts: [],
        parserContext: { database, character, userName: 'User', personaPrompt: '', modules: [], moduleLorebooks: [],
            selectedCharID: 0, chatVariables: {}, globalChatVariables: {}, currentTime: 1 },
        settings: { autoTranslate: translate, dynamicAssets: false, dynamicAssetsEditDisplay: false },
    } as unknown as ChatScreenshotRenderContext
    const bodyIndices: number[] = []
    const job = await createProductScreenshotJob({ characterId: character.chaId, chatId: 'chat', messages,
        start: 7, end: 7, renderContext: context, bodyIndices })
    expect(bodyIndices).toEqual([6])
    const scriptContext = { ...job.renderContext, dynamicAssets: false, dynamicAssetsEditDisplay: false } as any
    let text = job.messages[0].data
    if (translate) {
        bergamotTranslate.mockResolvedValueOnce('H2')
        text = await translateHTML(text, false, character, 6, false, { scriptContext })
    }
    expect(await ParseMarkdown(text, character, 'back', 6, {}, { scriptContext })).toBe('row 2')
    expect(bodyIndices).toEqual([6, 2])
})

test.each([false, true])('backward CBS scans use backend-aware bounded reads (projected %s)', async (projected) => {
    const messages: Message[] = Array.from({ length: 12 }, (_, index) => ({
        role: index === 2 ? 'user' : 'char', data: `row ${index}`,
    }))
    const bodyIndices: number[] = []
    const roleIndices: number[] = []
    const reader: CbsHistoryReader = { revision: 1, async readConversationWindow(query) {
        const start = query.startIndex!, end = start + query.limit!
        for (let index = start; index < end; index++) bodyIndices.push(index)
        return { revision: 1, value: { characterId: 'c', conversationId: 'chat', startIndex: start, endIndex: end,
            totalMessages: 12, messages: messages.slice(start, end), hasMoreBefore: start > 0, hasMoreAfter: end < 12 } }
    } }
    if (projected) reader.readRoleWindow = async (query) => {
        const start = query.startIndex!, end = start + query.limit!
        for (let index = start; index < end; index++) roleIndices.push(index)
        return { revision: 1, value: { characterId: 'c', conversationId: 'chat', startIndex: start, endIndex: end,
            totalMessages: 12, messages: messages.slice(start, end).map(({ role }) => ({ role, parserInert: true })),
            hasMoreBefore: start > 0, hasMoreAfter: end < 12 } }
    }
    const access = await prepareCbsHistory({ characterId: 'c', conversationId: 'chat', totalMessages: 12,
        chatID: 11, pageSize: 4, plan: planChatParserHistory({ source: '{{previoususerchat}}' }), reader })
    expect(access.previous('user', 11, 1).map(({ data }) => data)).toEqual(['row 2'])
    expect(new Set(bodyIndices).size).toBe(bodyIndices.length)
    expect(bodyIndices.length).toBe(projected ? 1 : 11)
    expect(roleIndices.length).toBe(projected ? 11 : 0)
    expect(Array.from({ length: 12 }, (_, index) => access.peek(index)).filter(Boolean)).toHaveLength(1)
})

test('a missing prepared row is observable and does not replay preceding variable writes', () => {
    const historyAccess = createCbsHistoryAccess({ length: 2, rows: new Map() })
    const writes: string[] = []
    expect(() => risuChatParser('{{setvar::score::changed}}{{lastmessage}}', {
        db: live.database, historyAccess, runVar: true,
        setChatVar: (key, value) => writes.push(`${key}:${value}`),
    })).toThrow(CbsHistoryAccessError)
    expect(writes).toEqual(['score:changed'])
    expect(() => historyAccess.assertComplete()).toThrow(CbsHistoryAccessError)
})

test.each([false, true])('Worker continuations preserve nested CBS, capture substitution, and ordered variable reads and writes (plain bypass %s)', async (skipPlainCbs) => {
    const character = makeCharacter('CBS worker')
    const variables: Record<string, string> = { score: '2', next: '{{getvar::score}}' }
    const reads: string[] = [], writes: string[] = []
    const plan = getRegexExecutionPlan([
        { in: '\\[board:(\\w+)\\]', out: '{{#if {{greater_equal::{{chat_index}}::{{? {{lastmessageid}}-3}}}}}}<section>' +
            '{{#if {{and::{{? {{chat_index}}={{lastmessageid}}}}::{{greater_equal::{{getvar::score}}::2}}}}}}' +
            Array.from({ length: 24 }, () => '{{getvar::score}}').join(',') + '{{/if}}:$1</section>{{/if}}' },
        { in: 'item', out: '{{setvar::score::9}}item' },
        { in: 'item', out: '{{getvar::score}}/{{getvar::next}}' },
        { in: 'absent', out: 'unused' },
    ].map((rule) => ({ ...rule, comment: '', type: 'editdisplay', flag: 'g', ableFlag: true })), 'editdisplay')
    const parse = (value: string) => risuChatParser(value, {
        db: makeDatabase(character, 'CBS worker'), chatID: 1, runVar: true,
        getChatVar: (key) => { reads.push(`${key}:${variables[key]}`); return variables[key] },
        setChatVar: (key, value) => { writes.push(`${key}:${value}`); variables[key] = value },
    })
    const padding = '문장😀 '.repeat(1000)
    const executor = new RegexExecutor({ skipPlainCbs, worker: true, client: new RegexWorkerClient(() => new InMemoryWorker()) })
    const result = await executor.execute(plan, padding + '[board:item]', parse, async () => { throw new Error('Unexpected host rule') })
    expect(result.data).toBe(padding + `<section>${Array(24).fill('2').join(',')}:9/9</section>`)
    expect(writes).toEqual(['score:9'])
    expect(reads.slice(-3)).toEqual(['score:9', 'next:{{getvar::score}}', 'score:9'])
    expect(result.errors).toEqual([])
})

test('does not rerun live display Lua after an obsolete translation finishes', async () => {
    live.database = {
        ...makeDatabase(makeCharacter('Translation owner'), 'Translation owner'),
        translatorType: 'bergamot',
        translator: 'ko',
        aiModel: 'gpt',
        htmlTranslation: false,
        combineTranslation: true,
    } as Database
    let finishTranslation!: (value: string) => void
    bergamotTranslate.mockImplementationOnce(
        () =>
            new Promise((resolve) => {
                finishTranslation = resolve
            }),
    )
    vi.mocked(runLuaEditTrigger).mockClear()
    const controller = new AbortController()
    const pending = translateHTML(
        '<p>synthetic delayed translation fixture</p>',
        false,
        live.database.characters[0],
        0,
        false,
        undefined,
        controller.signal,
    )
    await vi.waitFor(() => expect(finishTranslation).toBeTypeOf('function'))
    finishTranslation('synthetic translated fixture')
    live.database = makeDatabase(makeCharacter('Replacement owner'), 'Replacement owner')
    controller.abort()
    await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
    expect(runLuaEditTrigger).not.toHaveBeenCalled()
})

test('stores the display recovery record of the selected chat before a translation runs its display step again', async () => {
    const owner = makeCharacter('Translation owner')
    owner.chats[0].id = 'translated-chat'
    live.database = {
        ...makeDatabase(owner, 'Translation owner'),
        translatorType: 'bergamot',
        translator: 'ko',
        aiModel: 'gpt',
        htmlTranslation: false,
        combineTranslation: true,
    } as Database
    bergamotTranslate.mockImplementationOnce(async () => 'synthetic translated fixture')
    vi.mocked(runLuaEditTrigger).mockClear()
    const device = recoveryStore()
    await installDisplayRecovery(device.store, { timers: manualTimers().timers })
    try {
        const pending = translateHTML('<p>synthetic translation fixture</p>', false, owner, 0, false)
        await vi.waitFor(() => expect(device.records).toEqual([{
            working: [{ characterId: owner.chaId, conversationId: 'translated-chat' }],
            marked: [],
        }]))
        // A turn of the event loop in which a display step that did not wait would start.
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(runLuaEditTrigger).not.toHaveBeenCalled()

        device.commitAll()
        await pending
        expect(runLuaEditTrigger).toHaveBeenCalledOnce()
    } finally {
        await installDisplayRecovery(null)
    }
})

test('uses frozen CBS values after the live database, persona, variables, and modules change', () => {
    const frozenCharacter = makeCharacter('Frozen')
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const parserContext = {
        db: frozenDatabase,
        chara: frozenCharacter,
        chatID: 1,
        userName: 'Frozen user',
        personaPrompt: 'Frozen persona',
        modules: [{ id: 'frozen', name: 'Frozen', description: '', namespace: 'frozen-module' }],
        moduleLorebooks: [],
        selectedCharID: 0,
        chatVariables: { score: 'Frozen score' },
        globalChatVariables: { world: 'Frozen world' },
        currentTime: Date.UTC(2020, 0, 2, 3, 4, 5),
    }

    live.database = makeDatabase(makeCharacter('Changed'), 'Changed')
    live.modules = [{ id: 'changed', name: 'Changed', namespace: 'changed-module' }]

    expect(risuChatParser(
        '{{user}}|{{char}}|{{persona}}|{{previoususerchat}}|{{getvar::score}}|{{getglobalvar::world}}|{{moduleenabled::frozen-module}}|{{mainprompt}}|{{date::YYYY-MM-DD}}',
        parserContext,
    )).toBe('Frozen user|Frozen|Frozen persona|Frozen previous|Frozen score|Frozen world|1|Frozen main|2020-01-02')
})

test('uses frozen variable snapshots for #when operators', () => {
    const frozenCharacter = makeCharacter('Frozen')
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const parserContext = {
        db: frozenDatabase,
        chara: frozenCharacter,
        chatID: 1,
        chatVariables: { enabled: '1', score: 'Frozen score' },
        globalChatVariables: { toggle_enabled: '1' },
    }

    live.database.characters[0].chats[0].scriptstate = {
        $enabled: '0',
        $score: 'Live score',
    }
    live.database.globalChatVariables = { toggle_enabled: '0' }

    expect(risuChatParser(
        [
            '{{#when::var::enabled}}var{{/when}}',
            '{{#when::score::vis::Frozen score}}vis{{/when}}',
            '{{#when::toggle::enabled}}toggle{{/when}}',
            '{{#when::enabled::tis::1}}tis{{/when}}',
        ].join('|'),
        parserContext,
    )).toBe('var|vis|toggle|tis')
})

test('uses frozen CBS values for the initial message and dynamic regex pattern', async () => {
    const frozenCharacter = makeCharacter('Frozen')
    const script: customscript = {
        comment: '',
        in: '^{{user}}$',
        out: '{{user}} matched',
        type: 'editdisplay',
        flag: 'g<cbs>',
        ableFlag: true,
    }
    frozenCharacter.customscript = [script]
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')

    const result = await processScriptFull(
        frozenCharacter,
        '{{user}}',
        'editdisplay',
        1,
        { chatRole: 'char' },
        {
            cache: 'bypass',
            captureContext: {
                presetRegex: [],
                moduleRegexScripts: [],
                moduleAssets: [],
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext: {
                    database: frozenDatabase,
                    character: frozenCharacter,
                    userName: 'Frozen user',
                    personaPrompt: 'Frozen persona',
                    modules: [],
                    moduleLorebooks: [],
                    selectedCharID: 0,
                    chatVariables: {},
                    globalChatVariables: {},
                    currentTime: 1,
                },
            },
        },
    )

    expect(result.data).toBe('Frozen user matched')
})

test('keeps real ParseMarkdown CBS output frozen when live state changes mid-capture', async () => {
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'user', data: 'previous' },
        { role: 'char', data: '{{user}}|{{persona}}|{{previoususerchat}}|{{moduleenabled::frozen-module}}' },
    ])
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 2,
        end: 2,
        renderContext: {
            character: null,
            characterName: frozenCharacter.name,
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: 'Frozen persona',
                modules: [{ id: 'frozen', name: 'Frozen', description: '', namespace: 'frozen-module' }] as any,
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'google',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })

    live.database = makeDatabase(makeCharacter('Changed'), 'Changed')
    live.modules = [{ id: 'changed', name: 'Changed', namespace: 'changed-module' }]
    const capture = job.renderContext
    const output = await ParseMarkdown(
        job.messages[0].data,
        capture.parserContext.character as any,
        'back',
        job.start - 1,
        { chatRole: 'char' },
        {
            moduleAssets: capture.moduleAssets,
            hideAllImages: capture.settings.hideAllImages,
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext,
            } as any,
            projectedChatID: capture.firstParserMessageIndex,
        },
    )

    expect(output).toContain('Frozen user|Frozen persona|previous|1')
    expect(output).not.toContain('Changed')
})

test('keeps absolute chatindex while previouscharchat scans the projected frozen history', async () => {
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'char', data: 'earlier character turn' },
        { role: 'user', data: 'intervening user turn' },
        { role: 'char', data: '{{chatindex}}|{{previouscharchat}}' },
    ])
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 3,
        end: 3,
        renderContext: {
            character: null,
            characterName: frozenCharacter.name,
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: 'Frozen persona',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'google',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })
    const capture = job.renderContext
    const output = await ParseMarkdown(
        job.messages[0].data,
        capture.parserContext.character as any,
        'back',
        job.start - 1,
        { chatRole: 'char' },
        {
            projectedChatID: capture.firstParserMessageIndex,
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext,
            },
        } as any,
    )

    expect(output).toContain('2|earlier character turn')

    const css = '.capture::after{content:"{{chatindex}}|{{previouscharchat}}"}'
    const styled = trimMarkdown(
        `<risu-style>${Buffer.from(css).toString('hex')}</risu-style>`,
        {
            parserContext: capture.parserContext as any,
            chatID: job.start - 1,
            projectedChatID: capture.firstParserMessageIndex,
            cbsConditions: { chatRole: 'char' },
        },
    )
    expect(styled).toContain('content:"2|earlier character turn"')
})

test('keeps the original final message and absolute id for a middle capture range', async () => {
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'char', data: 'opening character turn' },
        { role: 'user', data: 'first user turn' },
        { role: 'char', data: '{{lastmessage}}|{{lastmessageid}}' },
        { role: 'user', data: 'later user turn' },
        { role: 'char', data: 'original final turn' },
    ])
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 3,
        end: 3,
        renderContext: {
            character: null,
            characterName: 'Frozen',
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: '',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'google',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })
    const capture = job.renderContext

    const output = await ParseMarkdown(
        job.messages[0].data,
        capture.parserContext.character as any,
        'back',
        job.start - 1,
        { chatRole: 'char' },
        {
            projectedChatID: capture.firstParserMessageIndex,
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext,
            },
        } as any,
    )

    expect(output).toContain('original final turn|4')
    expect(capture.parserContext.historyAccess?.length).toBe(5)
    expect(capture.parserContext.historyAccess?.row(4)?.data).toBe('original final turn')
})

test('uses frozen parser and regex inputs through real cached auto-translation', async () => {
    await clearLLMCache()
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'char', data: 'frozen previous character' },
        { role: 'user', data: 'intervening user' },
        { role: 'char', data: 'source text' },
    ])
    const frozenDatabase = {
        ...makeDatabase(frozenCharacter, 'Frozen'),
        translatorType: 'llm',
        translator: 'ko',
        translatorInputLanguage: 'en',
    } as Database
    const presetScript: customscript = {
        comment: '',
        in: '{{previouscharchat}}',
        out: 'frozen preset',
        type: 'edittrans',
        flag: 'g<cbs>',
        ableFlag: true,
    }
    const moduleScript: customscript = {
        comment: '',
        in: 'frozen preset',
        out: 'frozen module',
        type: 'edittrans',
    }
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 3,
        end: 3,
        renderContext: {
            character: frozenCharacter as any,
            characterName: frozenCharacter.name,
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [presetScript],
            moduleRegexScripts: [moduleScript],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: 'Frozen persona',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: true,
                autoTranslateCachedOnly: true,
                translatorType: 'llm',
                translateBeforeHTMLFormatting: true,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })
    await setLLMCache('source text', 'frozen previous character')

    const changedCharacter = makeCharacter('Changed', [
        { role: 'char', data: 'changed previous character' },
        { role: 'user', data: 'changed user' },
        { role: 'char', data: 'changed current' },
    ])
    live.database = {
        ...makeDatabase(changedCharacter, 'Changed'),
        translatorType: 'llm',
        translator: 'ja',
        translatorInputLanguage: 'en',
        presetRegex: [{ ...presetScript, in: 'changed previous character', out: 'changed preset' }],
    } as Database
    live.modules = [{ id: 'changed', name: 'Changed', namespace: 'changed-module' }]

    const capture = job.renderContext
    const result = await translateHTML(
        'source text',
        false,
        capture.character as any,
        job.start - 1,
        false,
        {
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext as any,
            },
            projectedChatID: capture.firstParserMessageIndex,
            chara: capture.characterName,
            cbsConditions: { chatRole: 'char' },
        },
    )

    expect(result).toBe('frozen module')
    expect(result).not.toContain('Changed')
    await clearLLMCache()
})

test('does not reuse a live text-only translation cache entry during capture', async () => {
    const text = 'capture cache identity regression'
    const liveCharacter = makeCharacter('Live')
    live.database = {
        ...makeDatabase(liveCharacter, 'Live'),
        translatorType: 'bergamot',
        translator: 'live-target',
        translatorInputLanguage: 'en',
        aiModel: '',
        htmlTranslation: false,
        combineTranslation: false,
    } as Database
    expect(await translate(text, false)).toBe('en->live-target')

    const frozenCharacter = makeCharacter('Frozen')
    const frozenDatabase = {
        ...makeDatabase(frozenCharacter, 'Frozen'),
        translatorType: 'bergamot',
        translator: 'frozen-target',
        translatorInputLanguage: 'en',
        aiModel: '',
        htmlTranslation: false,
        combineTranslation: false,
    } as Database
    const result = await translateHTML(
        `<p>${text}</p>`,
        false,
        frozenCharacter,
        1,
        false,
        {
            scriptContext: {
                presetRegex: [],
                moduleRegexScripts: [],
                moduleAssets: [],
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext: {
                    database: frozenDatabase,
                    character: frozenCharacter,
                    userName: 'Frozen user',
                    personaPrompt: '',
                    modules: [],
                    moduleLorebooks: [],
                    selectedCharID: 0,
                    chatVariables: {},
                    globalChatVariables: {},
                    currentTime: 1,
                },
            },
            projectedChatID: 1,
            chara: frozenCharacter,
            cbsConditions: { chatRole: 'char' },
        },
    )

    expect(result).toContain('frozen-target')
    expect(result).not.toContain('live-target')
})

test('keeps chardisplayasset command and exclusions in the minimal frozen character', () => {
    const source = makeCharacter('Frozen')
    source.additionalAssets = [
        ['Visible display asset', 'visible.png', 'png'],
        ['Excluded display asset', 'excluded.png', 'png'],
    ]
    source.prebuiltAssetCommand = true
    source.prebuiltAssetExclude = ['excluded.png']
    const projected = snapshotChatScreenshotCharacter(source, source.chats[0], undefined)
    const database = { characters: [projected] } as Database

    live.database = makeDatabase(makeCharacter('Changed'), 'Changed')
    const output = risuChatParser('{{chardisplayasset}}', {
        db: database,
        chara: projected,
        selectedCharID: 0,
    })

    expect(output).toContain('Visible display asset')
    expect(output).not.toContain('Excluded display asset')
})

test('keeps the author note of a preset-linked chat in the minimal frozen character', () => {
    const source = makeCharacter('Frozen')
    const preset = { id: 'preset', name: 'Preset', text: 'Frozen preset note' }
    Object.assign(source.chats[0], authorNotePresetLinkPatch(preset, 'Stale copy'))
    const projected = snapshotChatScreenshotCharacter(source, source.chats[0], [preset])
    preset.text = 'Edited after capture'

    expect(risuChatParser('{{authornote}}', {
        db: { characters: [projected] } as Database,
        chara: projected,
        selectedCharID: 0,
    })).toBe('Frozen preset note')
})
