import { beforeEach, expect, test, vi } from 'vitest'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { Chat, character } from '../storage/database.svelte'
import { DBState, selectedCharID } from '../stores.svelte'
import { processMultiCommand } from './command'
import { createConversationOperationContext } from './conversationOperationContext'
import { createChatParserDependencyStamp } from '../chatRenderIdentity'

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
vi.mock('../parser/parser.svelte', () => ({
    risuChatParser: (value: string) => value,
}))
vi.mock('./command', () => ({ processMultiCommand: vi.fn() }))
vi.mock('./request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('./stableDiff', () => ({ generateAIImage: vi.fn() }))
vi.mock('./files/inlays', () => ({ writeInlayImage: vi.fn() }))

const { runTrigger } = await import('./triggers')

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
        { role: 'char', data: 'tail' },
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

test('CAS-applies Trigger chat variables and note metadata', async () => {
    const { chat, char, session } = fixture()
    char.triggerscript[0].effect = [
        { type: 'setvar', var: 'state', value: '', operator: '=' },
        { type: 'v2SetAuthorNote', value: 'operation note', valueType: 'value' },
    ] as never
    runtime.session = session
    DBState.db = {
        characters: [char],
        templateDefaultVariables: '',
    } as never

    await runTrigger(char, 'manual', {
        chat,
        manualName: 'ordered',
    })

    expect(chat.scriptstate).toEqual({ '$state': '' })
    expect(chat.note).toBe('operation note')
    expect(session.version).toBe(1)
})

test('keeps eager Trigger metadata on its original owner after a later error', async () => {
    const { chat, char, session } = fixture()
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

    await expect(runTrigger(char, 'manual', {
        chat,
        manualName: 'ordered',
    })).rejects.toThrow('command failed')

    expect(chat.scriptstate).toEqual({ '$state': '' })
    expect(chat.note).toBe('partial note')
    expect(session.version).toBe(1)
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
        expect(original.chat.scriptstate).toEqual({})
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
