import { expect, vi } from 'vitest'
import type { Database, Message } from '../../storage/database.svelte'
import type { PersistentDataRuntime } from '../../storage/persistentDataRuntime'
import type { IndexedDbPersistentDataStore } from '../../storage/indexedDbPersistentDataStore'
import type { PluginDatabaseAccess } from '../../plugins/pluginDatabaseAccess'
import { DBState } from '../../stores.svelte'
import { beginGenerationEndRun, subscribeGenerationEnd, type GenerationEndRecord } from '../generationEnd'
import { sendChat } from '../index.svelte'
import { requestChatData } from '../request/request'
import { conversationPatchAccess } from '../../plugins/conversationPatchHost'

const target = { characterId: 'character-a', conversationId: 'chat-a' }
const partialText = 'streamed prefix'
export const finalText = 'streamed prefix and final chunk'

function gate() {
    let resolve!: () => void
    const promise = new Promise<void>(done => { resolve = done })
    return { resolve, promise }
}

interface OutputFixture {
    runtime: PersistentDataRuntime
    access: PluginDatabaseAccess
    connect(): Promise<IndexedDbPersistentDataStore>
    length: number
    windowed: boolean
}

export async function liveMessage(runtime: PersistentDataRuntime, index: number) {
    const session = runtime.getActiveConversationSession()
    if (session) return index < session.totalMessages ? session.readMessage(session.locate(index)) : undefined
    const source = runtime.getActiveConversationViewportSource()!
    await source.ensureRange({ startIndex: index, limit: 1, reason: 'viewport' })
    return source.snapshot().rowAt(index)?.message
}

export async function whileStreaming(
    fixture: Pick<OutputFixture, 'runtime' | 'length' | 'windowed'>,
    action: (stream: { reply: Message; signal: AbortSignal; finishStream(): void; chunk(text: string): void }) => Promise<void>,
    beforeProviderResponse?: () => Promise<void>,
    expectedCompletion = true,
) {
    const { runtime, length, windowed } = fixture
    let stream!: ReadableStreamDefaultController<Record<string, string>>
    let closed = false
    const body = new ReadableStream<Record<string, string>>({
        start(controller) { stream = controller },
        cancel() { closed = true },
    })
    vi.mocked(requestChatData).mockImplementationOnce(async () => {
        await beforeProviderResponse?.()
        return { type: 'streaming', result: body }
    })
    stream.enqueue({ response: partialText })
    const finishStream = () => {
        if (closed) return
        closed = true
        stream.enqueue({ response: finalText })
        stream.close()
    }
    const controller = new AbortController()
    const events: GenerationEndRecord[] = []
    const unsubscribe = subscribeGenerationEnd(event => events.push(event))
    const run = beginGenerationEndRun(target)
    const sending = run.capture(() => sendChat({ historyLimit: windowed, signal: controller.signal }))
    try {
        const reply = await vi.waitFor(async () => {
            const row = await liveMessage(runtime, length)
            expect(row?.data).toBe(partialText)
            return row!
        }, { timeout: 5_000 })
        const token = () => runtime.getActiveConversationSession()?.sessionToken ?? runtime.captureSelectedConversationAuthority()?.sessionToken
        const initialToken = token()
        expect(initialToken).toEqual(expect.any(String))
        expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
        await action({ reply, signal: controller.signal, finishStream, chunk: text => stream.enqueue({ response: text }) })
        if (!closed) {
            expect(runtime.getSelectedConversationMode()).toBe(windowed ? 'windowed' : 'complete')
            expect(token()).toBe(initialToken)
        }
        finishStream()
        const completed = await sending
        run.finish(completed ? 'completed' : 'failed')
        expect(completed).toBe(expectedCompletion)
        if (expectedCompletion) {
            expect(events).toEqual([expect.objectContaining({ ...target, status: 'completed', messageIds: expect.arrayContaining([reply.chatId]) })])
        } else expect(events).toEqual([expect.objectContaining({ ...target, status: 'failed' })])
        return reply
    } finally {
        finishStream()
        controller.abort()
        await sending
        unsubscribe()
    }
}

export const setterCases = [
    'earlier message', 'note only', 'copy predating reply', 'unread copy predating user message', 'character conversation', 'character field',
    'unread character conversation', 'database', 'unread database', 'database lite', 'insert', 'delete', 'reorder', 'move reply', 'reply fields', 'legacy row', 'patch', 'deleted target',
] as const
export type SetterCase = typeof setterCases[number]

export async function checkStreamingWrite(fixture: OutputFixture, edit: SetterCase) {
    const { access, runtime, length } = fixture
    const context = { pluginName: 'streaming-plugin', signal: new AbortController().signal }
    const original = await fixture.connect()
    const readBeforeReply = edit === 'copy predating reply' || (edit === 'legacy row' && fixture.windowed)
    const before = readBeforeReply
        ? (await access.getChatFromIndex(0, 0, context))!
        : (await original.readConversation(target.characterId, target.conversationId))!.value
    const unreadCharacter = edit === 'unread character conversation' || edit === 'unread database'
        ? { ...(await original.readCharacter(target.characterId))!.value, chats: [before] } : null
    const expected = before.message.map(row => ({ id: row.chatId, data: row.data }))
    if (edit === 'unread copy predating user message') {
        before.message.pop()
        expected.pop()
    }
    const first = expected[0]
    const marker = 'edited by plugin'
    const reply = await whileStreaming(fixture, async ({ reply, signal }) => {
        const call = { ...context, signal }
        if (edit === 'patch') {
            const result = await conversationPatchAccess.patchConversation({
                ...target, mutationId: 'streaming-patch', chatVariables: [],
                messages: [
                    { index: 0, messageId: first.id!, set: { data: marker } },
                    { index: length, messageId: reply.chatId!, set: { data: 'plugin reply text', __public: marker } },
                ],
            }, signal)
            expect(result.status).toBe('applied')
            first.data = marker
        } else if (edit === 'database' || edit === 'unread database' || edit === 'database lite') {
            const snapshot = unreadCharacter ? { characters: [unreadCharacter] }
                : await access.getDatabaseSnapshot(['characters'], ['characters'])
            const characters = snapshot.characters as Database['characters']
            characters[0].chats[0].message[0].data = marker
            await access[edit === 'database lite' ? 'setDatabaseLite' : 'setDatabase'](snapshot, ['characters'])
            first.data = marker
        } else if (edit === 'character conversation' || edit === 'unread character conversation' || edit === 'character field') {
            const character = unreadCharacter ?? (await access.getCharacterFromIndex(0, call))!
            if (edit !== 'character field') {
                character.chats[0].message[0].data = marker
                first.data = marker
            } else character.desc = marker
            await access.setCharacterToIndex(0, character, call)
        } else {
            const chat = readBeforeReply || edit === 'unread copy predating user message'
                ? before : (await access.getChatFromIndex(0, 0, call))!
            if (edit === 'note only') chat.note = marker
            else if (edit === 'insert') {
                chat.message.splice(1, 0, { role: 'char', data: marker, chatId: 'inserted' })
                expected.splice(1, 0, { data: marker, id: 'inserted' })
            } else if (edit === 'delete') {
                chat.message.splice(1, 1)
                expected.splice(1, 1)
            } else if (edit === 'reorder') {
                chat.message.splice(0, 2, chat.message[1], chat.message[0])
                expected.splice(0, 2, expected[1], expected[0])
            } else if (edit === 'move reply') {
                chat.message.unshift(chat.message.pop()!)
            } else if (edit === 'reply fields') {
                Object.assign(chat.message.at(-1)!, { data: 'plugin reply text', __public: marker })
            } else if (edit === 'deleted target') {
                const remote = await fixture.connect()
                const revision = (await remote.readRoot()).revision
                await remote.commit({ expectedRevision: revision, conversations: [
                    { type: 'replace-range', ...target, start: 0, deleteCount: 1, messages: [] },
                ] })
                chat.message[0].data = marker
                expected.shift()
            } else {
                if (edit === 'legacy row') expect(chat.message[0].chatId).toBeUndefined()
                chat.message[0].data = marker
                first.data = marker
            }
            await access.setChatToIndex(0, 0, chat, call)
        }
        if (edit === 'legacy row') {
            const edited = await liveMessage(runtime, 0)
            expect(edited?.data).toBe(marker)
            if (edited?.chatId) expect(edited.chatId).toMatch(/^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/i)
            first.id = edited?.chatId
        }
        const currentReply = await liveMessage(runtime, expected.length)
        expect.soft(currentReply?.chatId).toBe(reply.chatId)
        if (edit === 'reply fields' || edit === 'patch') {
            expect(currentReply).toMatchObject({ data: 'plugin reply text', __public: marker })
        } else expect.soft(currentReply?.data).toBe(partialText)
    }, edit === 'legacy row' && !fixture.windowed ? async () => {
        // Complete prompt construction assigns IDs. A request-phase plugin can still supply a legacy row.
        const chat = (await access.getChatFromIndex(0, 0, context))!
        delete chat.message[0].chatId
        await access.setChatToIndex(0, 0, chat, context)
    } : undefined)
    expect.soft((await liveMessage(runtime, 0))?.data).toBe(expected[0].data)
    if (edit === 'note only') expect(DBState.db.characters[0].chats[0].note).toBe(marker)
    if (edit === 'character field') expect(DBState.db.characters[0].desc).toBe(marker)
    const completedReply = await liveMessage(runtime, expected.length)
    expect.soft(completedReply).toMatchObject({ chatId: reply.chatId, data: finalText })
    if (edit === 'patch' || edit === 'reply fields') expect(completedReply).toMatchObject({ __public: marker })
    expected.push({ id: reply.chatId, data: finalText })
    await runtime.flushPendingDataLocally('streaming-plugin-test')
    const reader = await fixture.connect()
    const stored = (await reader.readConversation(target.characterId, target.conversationId))!.value
    expect(stored.message.map(row => ({ id: row.chatId, data: row.data }))).toEqual(expected)
    if (edit === 'note only') expect(stored.note).toBe(marker)
    if (edit === 'patch' || edit === 'reply fields') expect(stored.message.at(-1)).toMatchObject({ __public: marker })
    if (edit === 'character field') expect((await reader.readCharacter(target.characterId))!.value.desc).toBe(marker)
}

export function installOutputTrigger(db: Database) {
    const character = db.characters[0]
    character.lowLevelAccess = true
    const index = character.chats[0].message.length
    character.triggerscript = [{ type: 'output', comment: '', conditions: [], effect: [
        { type: 'modifychat', index: String(index), value: 'reply edited by trigger' },
        { type: 'runLLM', value: 'synthetic trigger request', inputVar: 'result' },
    ] }]
}

export async function checkOutputProcessing(fixture: OutputFixture) {
    const { access, runtime } = fixture
    let entered = false
    const release = gate()
    const patch = { ...target, mutationId: 'processing-retry', messages: [
        { index: 0, messageId: null as string | null, set: { __retry: 'applied after generation' } },
    ], chatVariables: [] }
    let writing: Promise<void> | undefined
    try {
        await whileStreaming(fixture, async ({ signal, finishStream }) => {
            const context = { pluginName: 'streaming-plugin', signal }
            const owner = runtime.getActiveConversationSession() ?? runtime.getActiveConversationViewportSource()
            const chat = (await access.getChatFromIndex(0, 0, context))!
            patch.messages[0].messageId = chat.message[0].chatId!
            chat.message[0].data = 'setter during script'
            vi.mocked(requestChatData).mockImplementationOnce(async () => {
                entered = true
                await release.promise
                return { type: 'success', result: 'trigger result' }
            })
            finishStream()
            try {
                await vi.waitFor(() => expect(entered).toBe(true), { timeout: 3_000 })
                let settled = false
                writing = access.setChatToIndex(0, 0, chat, context).then(async () => {
                    settled = true
                    expect((await liveMessage(runtime, fixture.length))?.data).toBe('reply edited by trigger')
                })
                void writing.catch(() => {})
                expect((await conversationPatchAccess.patchConversation(patch, signal)).status).toBe('busy')
                await new Promise<void>(done => setImmediate(done))
                expect(settled).toBe(false)
                expect(runtime.getActiveConversationSession() ?? runtime.getActiveConversationViewportSource()).toBe(owner)
            } finally { release.resolve() }
            await writing
        })
        expect((await conversationPatchAccess.patchConversation(patch)).status).toBe('applied')
        await runtime.flushPendingDataLocally('output-trigger-test')
        const reader = await fixture.connect()
        const stored = (await reader.readConversation(target.characterId, target.conversationId))!.value
        expect(stored.message[0]).toMatchObject({ data: 'setter during script', __retry: 'applied after generation' })
        expect(stored.message.at(-1)?.data).toBe('reply edited by trigger')
        expect(stored.scriptstate?.$result).toBe('trigger result')
    } finally {
        release.resolve()
        await writing
    }
}
