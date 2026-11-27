import { beforeEach, expect, test, vi } from 'vitest'
import { get } from 'svelte/store'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { Chat, character } from '../storage/database.svelte'
import { CurrentTriggerIdStore, DBState, selectedCharID } from '../stores.svelte'
import { processMultiCommand } from './command'
import { createConversationOperationContext } from './conversationOperationContext'
import { createChatParserDependencyStamp } from '../chatRenderIdentity'
import { DisplayReadRecord, recordAmbientChatVariableReads, registerDisplayRow } from './scriptVariableReads'
import { StallingWorker } from '../../../tests/support/regexWorker'
import { authorNoteFingerprint, effectiveAuthorNote } from '../authorNotePresets'

const runtime = vi.hoisted(() => ({
    unexpectedNativeRuntimeAccess: () => {
        throw new Error('Unexpected native runtime access in this test')
    },
    session: null as ActiveConversationSession | null,
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: runtime.unexpectedNativeRuntimeAccess,
    capturePersistentMutationToken: runtime.unexpectedNativeRuntimeAccess,
    getPersistentDataRuntime: runtime.unexpectedNativeRuntimeAccess,
    peekActiveConversationSession: () => runtime.session,
}))
vi.mock('./modules', async (importOriginal) => ({
    ...await importOriginal<typeof import('./modules')>(),
    getModuleTriggers: () => [],
}))
vi.mock('../tokenizer', () => ({ tokenize: vi.fn(async () => 0) }))
vi.mock('../parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('./command', () => ({ processMultiCommand: vi.fn() }))
vi.mock('./request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('./stableDiff', () => ({ generateAIImage: vi.fn() }))
vi.mock('./files/inlays', () => ({ writeInlayImage: vi.fn() }))
vi.mock('./memory/hypamemory', async () => (await import('./tests/scriptingsTestHarness')).hypamemoryModule())

const { runTrigger } = await import('./triggers')
const { requestChatData } = await import('./request/request')
const { generateAIImage } = await import('./stableDiff')
const { HypaProcesser } = await import('./memory/hypamemory')

function fixture() {
    const chat = {
        id: 'conversation-1',
        message: [
            { role: 'user', data: 'zero', chatId: 'message-0' },
            { role: 'char', data: 'one', chatId: 'message-1' },
        ],
        scriptstate: {},
    } as Chat
    const char = {
        type: 'character',
        chaId: 'character-1',
        name: 'Character',
        chatPage: 0,
        chats: [chat],
        triggerscript: [{
            comment: 'ordered',
            type: 'manual',
            conditions: [],
            effect: [
                { type: 'modifychat', index: '0', value: '' },
                { type: 'impersonate', role: 'char', value: 'tail' },
            ],
        }],
        customscript: [],
        defaultVariables: '',
        firstMessage: 'first',
        alternateGreetings: [],
        lowLevelAccess: false,
    } as unknown as character
    const session = new ActiveConversationSession({
        characterId: char.chaId,
        conversationId: chat.id!,
        conversation: chat,
        storeRevision: 19,
    })
    return { chat, char, session }
}

beforeEach(() => {
    runtime.session = null
    selectedCharID.set(0)
})

test('skips a stalled regex condition and replacement and continues the trigger', async () => {
    vi.useFakeTimers()
    const originalWorker = globalThis.Worker
    vi.stubGlobal('Worker', StallingWorker)
    try {
        const { chat, char } = fixture()
        char.triggerscript = [{
            comment: 'condition', type: 'start',
            conditions: [{ type: 'exists', type2: 'regex', value: 'SLOW-condition|zero', depth: 2 }],
            effect: [{ type: 'modifychat', index: '0', value: 'must not run' }],
        }, {
            comment: 'replacement', type: 'start', conditions: [], effect: [{
                type: 'v2ReplaceString', source: 'source', sourceType: 'var',
                regex: 'pattern', regexType: 'var', flags: 'gs', flagsType: 'value',
                result: '$0', resultType: 'value', replacement: 'bad', replacementType: 'value',
                outputVar: 'replacement', indent: 0,
            }, {
                type: 'v2RegexTest', value: 'finished', valueType: 'value', regex: '^finished$', regexType: 'value',
                flags: '', flagsType: 'value', outputVar: 'continued', indent: 0,
            }],
        }] as character['triggerscript']
        DBState.db = { characters: [char], templateDefaultVariables: '' } as never
        chat.scriptstate = { $source: '<unchanged>', $pattern: 'SLOW-replacement|unchanged' }
        const regexGeneration = {}
        const result = runTrigger(char, 'start', { chat, regexGeneration })
        await vi.advanceTimersByTimeAsync(20_000)
        expect((await result)?.chat.scriptstate).toMatchObject({ $replacement: '<unchanged>', $continued: '1' })
        expect((await result)?.chat.message[0].data).toBe('zero')
        const nextChat = (await result)!.chat
        nextChat.scriptstate.$source = '<updated>'
        nextChat.scriptstate.$pattern = 'updated'
        const repeated = await runTrigger(char, 'start', { chat: nextChat, regexGeneration })
        expect(repeated?.chat.scriptstate.$replacement).toBe('<updated>')
        nextChat.scriptstate.$source = '<next>'
        nextChat.scriptstate.$pattern = 'next'
        const nextGeneration = await runTrigger(char, 'start', { chat: nextChat, regexGeneration: {} })
        expect(nextGeneration?.chat.scriptstate.$replacement).toBe('<bad>')
    } finally { vi.stubGlobal('Worker', originalWorker); vi.useRealTimers() }
})

test('skips a display trigger regex that timed out on other text in a later display run', async () => {
    vi.useFakeTimers()
    const originalWorker = globalThis.Worker
    vi.stubGlobal('Worker', StallingWorker)
    try {
        const { chat, char, session } = fixture()
        char.triggerscript = [{
            comment: 'display test', type: 'display', conditions: [], effect: [{
                type: 'v2RegexTest', value: 'source', valueType: 'var', regex: 'SLOW-display-trigger|text', regexType: 'value',
                flags: '', flagsType: 'value', outputVar: 'matched', indent: 0,
            }],
        }] as character['triggerscript']
        runtime.session = session
        DBState.db = { characters: [char], templateDefaultVariables: '' } as never
        chat.scriptstate = { $source: '<text>' }
        const first = runTrigger(char, 'display', { chat, displayMode: true, displayData: 'row' })
        await vi.advanceTimersByTimeAsync(10_000)
        await first
        chat.scriptstate.$source = '<edited text>'
        let finished = false
        const edited = runTrigger(char, 'display', { chat, displayMode: true, displayData: 'row' })
            .finally(() => { finished = true })
        await vi.advanceTimersByTimeAsync(1)
        expect(finished).toBe(true)
        await edited
    } finally { vi.stubGlobal('Worker', originalWorker); vi.useRealTimers() }
})

test('keeps lorebook name matches before a stalled comment, skips the rest, and later skips only that comment', async () => {
    vi.useFakeTimers()
    const originalWorker = globalThis.Worker
    vi.stubGlobal('Worker', StallingWorker)
    const posted = vi.spyOn(StallingWorker.prototype, 'postMessage')
    try {
        const { chat, char } = fixture()
        const lookup = (name: string) => [{ comment: 'lookup', type: 'start', conditions: [], effect: [{
            type: 'v2GetLorebookByName', name, nameType: 'value', outputVar: 'found', indent: 0,
        }] }] as character['triggerscript']
        const late = Array.from({ length: 70 }, (_, index) => ({ comment: `alpha ${index}` }))
        char.globalLore = [{ comment: 'Alpha' }, { comment: 'SLOW beta' }, {}, ...late] as character['globalLore']
        char.triggerscript = lookup('alpha')
        DBState.db = { characters: [char], templateDefaultVariables: '' } as never
        const requests = () => {
            const count = posted.mock.calls.filter(([message]) => message.type === 'testEach').length
            posted.mockClear()
            return count
        }
        const first = runTrigger(char, 'start', { chat, regexGeneration: {} })
        await vi.advanceTimersByTimeAsync(10_000)
        expect((await first)?.chat.scriptstate.$found).toBe('[0]')
        expect(requests()).toBe(1)
        const second = await runTrigger(char, 'start', { chat, regexGeneration: {} })
        expect(second?.chat.scriptstate.$found).toBe(JSON.stringify([0, ...late.map((_, index) => index + 3)]))
        expect(requests()).toBe(2)

        char.triggerscript = lookup('(')
        await expect(runTrigger(char, 'start', { chat, regexGeneration: {} })).rejects.toThrow(/Invalid regular expression/)
        char.globalLore = [{}] as character['globalLore']
        posted.mockClear()
        expect((await runTrigger(char, 'start', { chat, regexGeneration: {} }))?.chat.scriptstate.$found).toBe('[]')
        expect(requests()).toBe(0)
    } finally { posted.mockRestore(); vi.stubGlobal('Worker', originalWorker); vi.useRealTimers() }
})

test('keeps stored trigger permissions and parser identity stable during display', async () => {
    const { chat, char, session } = fixture()
    char.lowLevelAccess = true
    char.triggerscript[0].type = 'display'
    char.triggerscript[0].lowLevelAccess = false
    char.triggerscript[0].effect = []
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    const before = createChatParserDependencyStamp(char)

    for (let index = 0; index < 3; index++) {
        const result = await runTrigger(char, 'display', {
            chat,
            displayMode: true,
            displayData: 'unchanged',
        })
        expect(result?.displayData).toBe('unchanged')
        expect(char.triggerscript[0].lowLevelAccess).toBe(false)
        expect(createChatParserDependencyStamp(char)).toBe(before)
    }
    expect(session.activePinReasons).toEqual([])
})

test('records the chat variables a display trigger reads for its row', async () => {
    const { chat, char, session } = fixture()
    chat.scriptstate = { $hp: '9' }
    char.triggerscript[0].type = 'display'
    char.triggerscript[0].conditions = [
        { type: 'var', var: 'hp', value: '5', operator: '>' },
    ]
    char.triggerscript[0].effect = []
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    const reads = new DisplayReadRecord()
    const stopRecording = recordAmbientChatVariableReads(reads)
    try {
        await runTrigger(char, 'display', {
            chat,
            displayMode: true,
            displayData: 'unchanged',
        })
    } finally {
        stopRecording()
    }

    expect([...reads.keys]).toEqual(['hp'])
})

test('CAS-applies ordered trigger mutations and preserves an empty string value', async () => {
    const { chat, char, session } = fixture()
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never

    const result = await runTrigger(char, 'manual', {
        chat,
        manualName: 'ordered',
    })

    expect(result?.chat).toBe(chat)
    expect(chat.message).toEqual([
        { role: 'user', data: '', chatId: 'message-0' },
        { role: 'char', data: 'one', chatId: 'message-1' },
        { role: 'char', data: 'tail', chatId: expect.any(String) },
    ])
    expect(session.version).toBe(1)
    expect(session.activePinReasons).toEqual([])
})

test('serializes overlapping manual trigger commits and recursive calls', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript = [
        {
            comment: 'outer',
            type: 'manual',
            conditions: [],
            effect: [{ type: 'runtrigger', value: 'inner' }],
        },
        {
            comment: 'inner',
            type: 'manual',
            conditions: [],
            effect: [{ type: 'impersonate', role: 'char', value: 'tail' }],
        },
    ] as never
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    const results = await Promise.allSettled([
        runTrigger(char, 'manual', { chat, manualName: 'outer' }),
        runTrigger(char, 'manual', { chat, manualName: 'outer' }),
    ])
    expect(results.map((result) => result.status)).toEqual([
        'fulfilled',
        'fulfilled',
    ])
    expect(chat.message.map((entry) => entry.data)).toEqual([
        'zero',
        'one',
        'tail',
        'tail',
    ])
    expect(session.activePinReasons).toEqual([])
})

test.each(['command', 'v2Command'] as const)(
    'passes the owned operation to %s effects',
    async (type) => {
        const { chat, char, session } = fixture()
        char.triggerscript[0].effect = [
            { type, value: '/trigger inner', valueType: 'value' },
        ] as never
        char.triggerscript.push({
            comment: 'inner',
            type: 'manual',
            conditions: [],
            effect: [{ type: 'impersonate', role: 'char', value: 'nested' }],
        })
        runtime.session = session
        DBState.db = {
            characters: [char],
            templateDefaultVariables: '',
        } as never
        vi.mocked(processMultiCommand).mockImplementationOnce(
            async (_command, operation) => {
                expect(operation).toBeDefined()
                await runTrigger(char, 'manual', {
                    chat: operation!.chat,
                    manualName: 'inner',
                    conversationOperation: operation,
                })
                return ''
            },
        )
        await runTrigger(char, 'manual', { chat, manualName: 'ordered' })
        expect(chat.message.map((entry) => entry.data)).toEqual([
            'zero',
            'one',
            'nested',
        ])
        expect(session.activePinReasons).toEqual([])
    },
)

test.each(['command', 'v2Command'] as const)(
    'commits earlier Trigger variables before %s effects so /setvar keeps the last write',
    async (type) => {
        const { chat, char, session } = fixture()
        char.triggerscript[0].effect = [
            { type: 'setvar', var: 'state', value: 'trigger', operator: '=' },
            { type, value: '/setvar key=state command', valueType: 'value' },
        ] as never
        runtime.session = session
        DBState.db = {
            characters: [char],
            templateDefaultVariables: '',
        } as never
        let seenByCommand: unknown
        // `/getvar` reads the stored chat, and `/setvar` commits through the session that owns it.
        vi.mocked(processMultiCommand).mockImplementationOnce(async () => {
            seenByCommand = chat.scriptstate?.$state
            session.updateChatVariables(new Map([['state', 'command']]))
            return ''
        })

        await runTrigger(char, 'manual', { chat, manualName: 'ordered' })

        expect(seenByCommand).toBe('trigger')
        expect(chat.scriptstate).toEqual({ $state: 'command' })
    },
)

test('CAS-applies Trigger chat variables and note metadata', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].effect = [
        { type: 'setvar', var: 'state', value: '', operator: '=' },
        { type: 'v2GetAuthorNote', outputVar: 'before' },
        { type: 'v2SetAuthorNote', value: 'operation note', valueType: 'value' },
    ] as never
    runtime.session = session
    // The stored copy already equals the written value, so only the dropped link shows the write.
    chat.note = 'operation note'
    chat.authorNotePreset = { id: 'preset', fingerprint: authorNoteFingerprint('operation note') }
    const presets = [{ id: 'preset', name: 'Preset', text: 'Preset note' }]
    DBState.db = {
        characters: [char],
        authorNotePresets: presets,
        templateDefaultVariables: '',
    } as never

    await runTrigger(char, 'manual', {
        chat,
        manualName: 'ordered',
    })

    expect(chat.scriptstate).toEqual({ '$state': '', '$before': 'Preset note' })
    expect(effectiveAuthorNote(presets, chat)).toBe('operation note')
    expect(chat.authorNotePreset).toBeUndefined()
    expect(session.version).toBe(1)
})

test.each(['runtrigger', 'v2RunTrigger'] as const)('carries preparation cancellation through %s and nested commands', async (type) => {
    const { chat, char, session } = fixture()
    const controller = new AbortController()
    const generation = { signal: controller.signal, reservation: { isCurrent: () => true, release: vi.fn() } }
    char.triggerscript[0].effect = [
        { type, value: 'inner', target: 'inner', indent: 0 },
        { type: 'impersonate', role: 'char', value: 'after outer cancellation' },
    ] as never
    char.triggerscript.push({
        comment: 'inner', type: 'manual', conditions: [], effect: [
            { type: 'command', value: '/multisend first|||second' },
            { type: 'impersonate', role: 'char', value: 'after inner cancellation' },
        ],
    })
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    vi.mocked(processMultiCommand).mockImplementationOnce(async (_command, operation, inherited) => {
        expect(operation).toBeDefined()
        expect(inherited).toBe(generation)
        controller.abort()
        return ''
    })

    await runTrigger(char, 'manual', { chat, manualName: 'ordered', generation })

    expect(controller.signal.aborted).toBe(true)
    expect(chat.message.map(message => message.data)).toEqual(['zero', 'one'])
    expect(session.activePinReasons).toEqual([])
})

test.each(['manual', 'output'] as const)('keeps eager %s trigger metadata on its original owner after a later error', async mode => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].type = mode
    char.triggerscript[0].effect = [
        { type: 'setvar', var: 'state', value: '', operator: '=' },
        { type: 'v2SetAuthorNote', value: 'partial note', valueType: 'value' },
        { type: 'v2Command', value: 'fail', valueType: 'value' },
    ] as never
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never
    vi.mocked(processMultiCommand).mockRejectedValueOnce(new Error('command failed'))

    CurrentTriggerIdStore.set('clicked-button')
    try {
        await expect(runTrigger(char, mode, {
            chat, manualName: mode === 'manual' ? 'ordered' : undefined,
        })).rejects.toThrow('command failed')
        if (mode === 'output') expect(get(CurrentTriggerIdStore)).toBe('clicked-button')
    } finally { CurrentTriggerIdStore.set(null) }

    expect(chat.scriptstate).toEqual({ '$state': '' })
    expect(chat.note).toBe('partial note')
    // The variable commits before the command runs, and the note after it fails.
    expect(session.version).toBe(2)
    expect(session.activePinReasons).toEqual([])
})

test('rejects an awaited trigger batch after the active conversation advances', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].effect = [
        { type: 'impersonate', role: 'char', value: 'stale-tail' },
        { type: 'v2Wait', value: '0.001', valueType: 'value' },
    ] as never
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never
    vi.useFakeTimers()

    try {
        const pending = runTrigger(char, 'manual', {
            chat,
            manualName: 'ordered',
        })
        const settled = pending.then(
            () => null,
            (error: unknown) => error,
        )
        await vi.advanceTimersByTimeAsync(0)
        session.edit(session.locate(0), {
            role: 'user',
            data: 'concurrent',
            chatId: 'message-0',
        })
        await vi.advanceTimersByTimeAsync(1)

        expect(await settled).toEqual(expect.objectContaining({
            message: expect.stringMatching(/session version/i),
        }))
        expect(chat.message.map((entry) => entry.data)).toEqual(['concurrent', 'one'])
        expect(session.activePinReasons).toEqual([])
    }
    finally {
        vi.useRealTimers()
    }
})

test('runs a variable-only trigger without visiting messages', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].effect = [
        { type: 'setvar', var: 'state', value: 'new', operator: '=' },
    ] as never
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    // The length is a constant-time read; any element read or iteration visits messages.
    let messagesReadable = true
    chat.message = new Proxy(chat.message, {
        get(target, property, receiver) {
            if (!messagesReadable && property !== 'length') {
                throw new Error(`message read: ${String(property)}`)
            }
            return Reflect.get(target, property, receiver)
        },
    })

    messagesReadable = false
    try {
        const result = await runTrigger(char, 'manual', { chat, manualName: 'ordered' })
        expect(result?.chat).toBe(chat)
    } finally {
        messagesReadable = true
    }
    expect(chat.scriptstate).toEqual({ $state: 'new' })
    expect(session.activePinReasons).toEqual([])
})

test.each(['variables', 'message edit'] as const)(
    'keeps trigger variables when messages change while it waits (%s)',
    async (kind) => {
        const { chat, char, session } = fixture()
        char.triggerscript[0].effect = [
            { type: 'setvar', var: 'state', value: 'new', operator: '=' },
            { type: 'v2Wait', value: '0.001', valueType: 'value' },
            ...(kind === 'message edit' ? [{ type: 'modifychat', index: '1', value: 'edited' }] : []),
        ] as never
        runtime.session = session
        DBState.db = { characters: [char], templateDefaultVariables: '' } as never
        vi.useFakeTimers()

        try {
            const settled = runTrigger(char, 'manual', { chat, manualName: 'ordered' }).then(
                () => null,
                (error: unknown) => error,
            )
            await vi.advanceTimersByTimeAsync(0)
            // Variables set before the wait are committed while it waits.
            expect(chat.scriptstate).toEqual({ $state: 'new' })
            session.edit(session.locate(0), { role: 'user', data: 'concurrent', chatId: 'message-0' })
            await vi.advanceTimersByTimeAsync(1)

            // An edit computed before the change is not applied over it.
            expect(await settled).toEqual(kind === 'variables'
                ? null
                : expect.objectContaining({ message: expect.stringMatching(/session version/i) }))
            expect(chat.message.map((entry) => entry.data)).toEqual(['concurrent', 'one'])
            expect(chat.scriptstate).toEqual({ $state: 'new' })
            // The external edit and the trigger's variable commit.
            expect(session.version).toBe(2)
            expect(session.activePinReasons).toEqual([])
        } finally {
            vi.useRealTimers()
        }
    },
)

test.each([
    { type: 'runLLM', value: 'prompt', inputVar: 'out' },
    { type: 'runImgGen', value: 'prompt', negValue: '', inputVar: 'out' },
    { type: 'checkSimilarity', value: 'value', source: 'source', inputVar: 'out' },
    { type: 'v2RunLLM', value: 'prompt', valueType: 'value', outputVar: 'out', model: 'model' },
    { type: 'v2ImgGen', value: 'prompt', valueType: 'value', negValue: '', negValueType: 'value', outputVar: 'out' },
    { type: 'v2CheckSimilarity', source: 'source', sourceType: 'value', value: 'value', valueType: 'value', outputVar: 'out' },
])('commits trigger variables before $type waits and message edits at the end', async (wait) => {
    const { chat, char, session } = fixture()
    char.lowLevelAccess = true
    char.triggerscript[0].effect = [
        { type: 'modifychat', index: '0', value: 'edited' },
        { type: 'setvar', var: 'state', value: 'new', operator: '=' },
        wait,
    ] as never
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    const atWait: unknown[] = []
    const record = () => atWait.push({
        messages: chat.message.map((message) => message.data),
        scriptstate: { ...chat.scriptstate },
    })
    vi.mocked(requestChatData).mockReset().mockImplementation(async () => {
        record()
        return { type: 'success', result: 'done' } as never
    })
    vi.mocked(generateAIImage).mockReset().mockImplementation(async () => {
        record()
        return false
    })
    vi.mocked(HypaProcesser).mockReset().mockImplementation(function () {
        return {
            addText: async () => { record() },
            similaritySearch: async () => [],
        } as never
    })

    await runTrigger(char, 'manual', { chat, manualName: 'ordered' })

    expect(atWait).toEqual([{ messages: ['zero', 'one'], scriptstate: { $state: 'new' } }])
    expect(chat.message.map((message) => message.data)).toEqual(['edited', 'one'])
    expect(chat.scriptstate).toEqual({ $state: 'new', $out: expect.any(String) })
})

test('stops a trigger at the next wait once its conversation is no longer current', async () => {
    const original = fixture()
    original.char.lowLevelAccess = true
    original.char.triggerscript[0].effect = [
        // The first result is buffered after the conversation changed, so only the flush stops the second request.
        { type: 'v2RunLLM', value: 'first', valueType: 'value', outputVar: 'first', model: 'model' },
        { type: 'v2RunLLM', value: 'second', valueType: 'value', outputVar: 'second', model: 'model' },
    ] as never
    const replacement = fixture()
    replacement.char.chaId = 'character-2'
    replacement.chat.id = 'conversation-2'
    const replacementSession = new ActiveConversationSession({
        characterId: replacement.char.chaId,
        conversationId: replacement.chat.id!,
        conversation: replacement.chat,
        storeRevision: 20,
    })
    runtime.session = original.session
    DBState.db = {
        characters: [original.char, replacement.char],
        templateDefaultVariables: '',
    } as never
    let resolveFirst!: (value: unknown) => void
    vi.mocked(requestChatData).mockReset()
        .mockReturnValueOnce(new Promise((resolve) => { resolveFirst = resolve }) as never)
        .mockResolvedValue({ type: 'success', result: 'second' } as never)

    const settled = runTrigger(original.char, 'manual', {
        chat: original.chat,
        manualName: 'ordered',
    }).then(() => null, (error: unknown) => error)
    await vi.waitFor(() => expect(requestChatData).toHaveBeenCalledOnce())
    selectedCharID.set(1)
    runtime.session = replacementSession
    resolveFirst({ type: 'success', result: 'first' })

    expect(await settled).toEqual(expect.objectContaining({
        message: expect.stringMatching(/inactive/i),
    }))
    expect(requestChatData).toHaveBeenCalledOnce()
    expect(original.chat.scriptstate).toEqual({})
    expect(replacement.chat.scriptstate).toEqual({})
})

test.each([
    { type: 'v2UpdateGUI', rebuilt: [true, true] },
    { type: 'v2UpdateChatAt', index: '1', rebuilt: [false, true] },
])('rebuilds the mounted rows that $type reaches', async ({ rebuilt, ...effect }) => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].effect = [{ ...effect, indent: 0 }] as never
    runtime.session = session
    DBState.db = { characters: [char], templateDefaultVariables: '' } as never
    // Rows whose display read no chat variable.
    const rows = [0, 1].map((index) => {
        const row = registerDisplayRow(() => index)
        row.track('body', new DisplayReadRecord())
        return row
    })
    const initial = rows.map((row) => row.rebuildRevision)
    try {
        await runTrigger(char, 'manual', { chat, manualName: 'ordered' })

        expect(rows.map((row, index) => row.rebuildRevision !== initial[index])).toEqual(rebuilt)
    } finally {
        for (const row of rows) row.dispose()
    }
})

test('does not publish Trigger metadata to either conversation after navigation', async () => {
    const original = fixture()
    original.chat.note = 'original note'
    original.char.triggerscript[0].effect = [
        { type: 'setvar', var: 'state', value: 'operation value', operator: '=' },
        { type: 'v2SetAuthorNote', value: 'operation note', valueType: 'value' },
        { type: 'v2Wait', value: '0.001', valueType: 'value' },
    ] as never
    const replacement = fixture()
    replacement.char.chaId = 'character-2'
    replacement.chat.id = 'conversation-2'
    replacement.chat.note = 'replacement note'
    const replacementSession = new ActiveConversationSession({
        characterId: replacement.char.chaId,
        conversationId: replacement.chat.id!,
        conversation: replacement.chat,
        storeRevision: 20,
    })
    runtime.session = original.session
    DBState.db = {
        characters: [original.char, replacement.char],
        templateDefaultVariables: '',
    } as never
    vi.useFakeTimers()

    try {
        const pending = runTrigger(original.char, 'manual', {
            chat: original.chat,
            manualName: 'ordered',
        })
        const settled = pending.then(
            () => null,
            (error: unknown) => error,
        )
        await vi.advanceTimersByTimeAsync(0)
        selectedCharID.set(1)
        runtime.session = replacementSession
        await vi.advanceTimersByTimeAsync(1)

        expect(await settled).toEqual(expect.objectContaining({
            message: expect.stringMatching(/inactive/i),
        }))
        expect(original.chat.note).toBe('original note')
        // The variable was committed before the wait, while its conversation was current.
        expect(original.chat.scriptstate).toEqual({ $state: 'operation value' })
        expect(replacement.chat.note).toBe('replacement note')
        expect(replacement.chat.scriptstate).toEqual({})
    } finally {
        vi.useRealTimers()
    }
})

test('keeps eager character metadata on its original owner after navigation', async () => {
    const original = fixture()
    original.char.desc = 'original description'
    original.char.triggerscript[0].effect = [
        { type: 'v2Wait', value: '0.001', valueType: 'value' },
        { type: 'v2SetCharacterDesc', value: 'updated original', valueType: 'value' },
    ] as never
    const replacement = fixture()
    replacement.char.chaId = 'character-2'
    replacement.char.desc = 'replacement description'
    replacement.chat.id = 'conversation-2'
    const replacementSession = new ActiveConversationSession({
        characterId: replacement.char.chaId,
        conversationId: replacement.chat.id!,
        conversation: replacement.chat,
        storeRevision: 20,
    })
    runtime.session = original.session
    DBState.db = {
        characters: [original.char, replacement.char],
        templateDefaultVariables: '',
    } as never
    vi.useFakeTimers()

    try {
        const pending = runTrigger(original.char, 'manual', {
            chat: original.chat,
            manualName: 'ordered',
        })
        const settled = pending.then(
            () => null,
            (error: unknown) => error,
        )
        await vi.advanceTimersByTimeAsync(0)
        selectedCharID.set(1)
        runtime.session = replacementSession
        await vi.advanceTimersByTimeAsync(1)

        expect(await settled).toEqual(expect.objectContaining({
            message: expect.stringMatching(/inactive/i),
        }))
        expect((DBState.db.characters.find(
            (candidate) => candidate.chaId === original.char.chaId,
        ) as character | undefined)?.desc).toBe('updated original')
        expect((DBState.db.characters.find(
            (candidate) => candidate.chaId === replacement.char.chaId,
        ) as character | undefined)?.desc).toBe('replacement description')
    } finally {
        vi.useRealTimers()
    }
})

test('does not clone conversation history when no trigger exists', async () => {
    const { chat, char } = fixture()
    char.triggerscript = []
    let historyReads = 0
    Object.defineProperty(chat, 'note', {
        configurable: true,
        enumerable: true,
        get: () => {
            historyReads += 1
            return 'note'
        },
    })
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never

    await expect(runTrigger(char, 'manual', {
        chat,
        manualName: 'missing',
    })).resolves.toBeNull()

    expect(historyReads).toBe(0)
})

test('does not copy the character when no trigger runs in this mode', async () => {
    const { chat, char, session } = fixture()
    let descriptionReads = 0
    Object.defineProperty(char, 'desc', {
        configurable: true,
        enumerable: true,
        get: () => {
            descriptionReads += 1
            return 'description'
        },
    })
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never

    await runTrigger(char, 'output', { chat })
    await runTrigger(char, 'manual', { chat, manualName: 'missing' })
    expect(descriptionReads).toBe(0)

    await runTrigger(char, 'manual', { chat, manualName: 'ordered' })
    expect(chat.message.map((message) => message.data)).toEqual(['', 'one', 'tail'])
})

test('reuses a supplied operation chat across recursive triggers without cloning it', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript = [
        {
            comment: 'outer',
            type: 'manual',
            conditions: [],
            effect: [{ type: 'runtrigger', value: 'inner' }],
        },
        {
            comment: 'inner',
            type: 'manual',
            conditions: [],
            effect: [],
        },
    ] as never
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never
    const operation = createConversationOperationContext(session, chat)
    let operationChatReads = 0
    Object.defineProperty(operation.chat, 'note', {
        configurable: true,
        enumerable: true,
        get: () => {
            operationChatReads += 1
            return 'operation note'
        },
    })

    try {
        const result = await runTrigger(char, 'manual', {
            chat: operation.chat,
            manualName: 'outer',
            conversationOperation: operation,
        })

        expect(result?.chat).toBe(operation.chat)
        expect(operationChatReads).toBe(0)
    } finally {
        operation.release()
    }
})

test('does not clone inactive conversation bodies for a current-chat trigger', async () => {
    const { chat, char, session } = fixture()
    const archived = {
        id: 'conversation-archived',
        name: 'Archived',
        message: [{ role: 'user', data: 'archived', chatId: 'archived-0' }],
    } as Chat
    let archivedBodyReads = 0
    Object.defineProperty(archived.message[0], 'data', {
        configurable: true,
        enumerable: true,
        get: () => {
            archivedBodyReads += 1
            return 'archived'
        },
    })
    char.chats.push(archived)
    char.triggerscript[0].effect = [{
        type: 'v2SetCharacterDesc',
        value: 'updated description',
        valueType: 'value',
    }] as never
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never

    await runTrigger(char, 'manual', {
        chat,
        manualName: 'ordered',
    })

    expect(archivedBodyReads).toBe(0)
    expect((DBState.db.characters[0] as character).chats[1].message[0].data).toBe('archived')
})

test('releases the owned operation pin when trigger character setup fails', async () => {
    const { chat, char, session } = fixture()
    const archived = {
        id: 'conversation-archived',
        name: 'Archived',
        message: [],
    } as Chat
    Object.defineProperty(archived, 'note', {
        configurable: true,
        enumerable: true,
        get: () => {
            throw new Error('metadata failed')
        },
    })
    char.chats.push(archived)
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never

    await expect(runTrigger(char, 'manual', {
        chat,
        manualName: 'ordered',
    })).rejects.toThrow('metadata failed')

    expect(session.activePinReasons).toEqual([])
})

test('pins display history for the full awaited trigger lifetime', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].type = 'display'
    char.triggerscript[0].conditions = [
        { type: 'chatindex', value: '0', operator: '>' },
    ]
    char.triggerscript[0].effect = [
        { type: 'v2Comment', value: '' },
    ] as never
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never
    const pending = runTrigger(char, 'display', {
        chat,
        displayMode: true,
        displayData: 'display',
        additonalSysPrompt: {
            start: 'awaited prompt',
            historyend: '',
            promptend: '',
        },
    })

    expect(session.pinCount('compatibility')).toBe(1)
    await pending
    expect(session.pinCount('compatibility')).toBe(0)
})
