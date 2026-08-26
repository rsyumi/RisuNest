import { beforeEach, expect, test, vi } from 'vitest'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import type { Chat, character } from '../storage/database.svelte'
import { DBState, selectedCharID } from '../stores.svelte'

const runtime = vi.hoisted(() => ({
    session: null as ActiveConversationSession | null,
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
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
