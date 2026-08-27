import { describe, expect, it, vi } from 'vitest'
import {
    ChatParserCompleteProjectionRequiredError,
    ChatParserHistoryProjectionStaleError,
    createChatParserHistoryProjection,
    type ChatParserCompleteProjectionLease,
    type ChatParserHistoryProjectionInput,
    type ChatParserHistoryProjectionReader,
} from './chatParserHistoryProjection'
import type { ProcessScriptCaptureContext } from './process/scripts'
import type { Message } from './storage/database.svelte'

const CHARACTER_ID = 'character-1'
const CONVERSATION_ID = 'conversation-1'
const REVISION = 7

function makeMessages(count: number): Message[] {
    return Array.from({ length: count }, (_, index) => ({
        role: index % 2 === 0 ? 'char' : 'user',
        data: `turn ${index}`,
        chatId: `message-${index}`,
    }))
}

function makeContext(messages: Message[] = []): ProcessScriptCaptureContext {
    const chat = {
        id: CONVERSATION_ID,
        message: structuredClone(messages),
        note: '',
        name: '',
        localLore: [],
    }
    const character = {
        type: 'character' as const,
        chaId: CHARACTER_ID,
        name: 'Character',
        chats: [chat],
        chatPage: 0,
        customscript: [],
    }
    return {
        presetRegex: [],
        moduleRegexScripts: [],
        moduleAssets: [],
        dynamicAssets: false,
        dynamicAssetsEditDisplay: false,
        parserContext: {
            database: { characters: [character] } as never,
            character: character as never,
            userName: 'User',
            personaPrompt: '',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
    }
}

function makeReader(messages: Message[]) {
    const frozen = structuredClone(messages)
    const reads: Array<{ startIndex: number; limit: number }> = []
    const reader: ChatParserHistoryProjectionReader = {
        revision: REVISION,
        async readConversationWindow(query) {
            const startIndex = query.startIndex ?? 0
            const limit = query.limit ?? frozen.length
            const endIndex = Math.min(startIndex + limit, frozen.length)
            reads.push({ startIndex, limit })
            return {
                revision: REVISION,
                value: {
                    characterId: CHARACTER_ID,
                    conversationId: CONVERSATION_ID,
                    messages: structuredClone(frozen.slice(startIndex, endIndex)),
                    startIndex,
                    endIndex,
                    totalMessages: frozen.length,
                    hasMoreBefore: startIndex > 0,
                    hasMoreAfter: endIndex < frozen.length,
                },
            }
        },
    }
    return { reader, reads }
}

function makeCompleteLease(messages: Message[]): ChatParserCompleteProjectionLease {
    return {
        characterId: CHARACTER_ID,
        conversationId: CONVERSATION_ID,
        revision: REVISION,
        totalMessages: messages.length,
        context: makeContext(messages),
        release: vi.fn(),
    }
}

function makeInput(
    messages: Message[],
    currentAbsoluteIndex: number,
    overrides: Partial<ChatParserHistoryProjectionInput> = {},
) {
    const { reader, reads } = makeReader(messages)
    const input: ChatParserHistoryProjectionInput = {
        characterId: CHARACTER_ID,
        conversationId: CONVERSATION_ID,
        revision: REVISION,
        totalMessages: messages.length,
        currentAbsoluteIndex,
        maxProjectionMessages: 128,
        parserSource: '',
        contextSeed: makeContext(),
        reader,
        ...overrides,
    }
    return { input, reads, reader }
}

describe('live chat parser history projection', () => {
    it('builds a bounded common-row projection with absolute and projected offsets', async () => {
        const messages = makeMessages(100)
        const { input, reads } = makeInput(messages, 99, { maxProjectionMessages: 32 })
        const originalSeed = structuredClone(input.contextSeed)

        const result = await createChatParserHistoryProjection(input)

        expect(result).toMatchObject({
            kind: 'bounded',
            characterId: CHARACTER_ID,
            conversationId: CONVERSATION_ID,
            revision: REVISION,
            totalMessages: 100,
            chatID: 99,
            projectedChatID: 4,
            historyOffset: 95,
        })
        if (result.kind !== 'bounded') throw new Error('Expected bounded projection')
        expect(result.messages.map((message) => message.data)).toEqual([
            'turn 95',
            'turn 96',
            'turn 97',
            'turn 98',
            'turn 99',
        ])
        expect(
            result.context.parserContext.character.chats[0].message,
        ).toEqual(result.messages)
        expect(result.context.parserContext.historyOffset).toBe(95)
        expect(input.contextSeed).toEqual(originalSeed)
        expect(reads[0]).toEqual({ startIndex: 99, limit: 1 })
        expect(Math.max(...reads.map(({ limit }) => limit))).toBeLessThanOrEqual(32)
    })

    it('keeps a literal previouschatlog index in one exact contiguous projection', async () => {
        const messages = makeMessages(100)
        messages[89].data = '{{previouschatlog::10}}'
        const { input, reads } = makeInput(messages, 89, { maxProjectionMessages: 80 })

        const result = await createChatParserHistoryProjection(input)

        expect(result).toMatchObject({
            kind: 'bounded',
            historyOffset: 10,
            projectedChatID: 79,
        })
        if (result.kind !== 'bounded') throw new Error('Expected bounded projection')
        expect(result.messages).toHaveLength(80)
        expect(result.messages[0].data).toBe('turn 10')
        expect(result.messages[79].data).toBe('{{previouschatlog::10}}')
        expect(reads).toEqual([
            { startIndex: 89, limit: 1 },
            { startIndex: 10, limit: 79 },
        ])
    })

    it('falls back without spreading an imported message with many literal indices', async () => {
        const literalCount = 200_000
        const currentAbsoluteIndex = literalCount + 1
        const currentMessage: Message = { role: 'char', data: 'current' }
        const { input, reader } = makeInput([currentMessage], 0, {
            totalMessages: currentAbsoluteIndex + 1,
            currentAbsoluteIndex,
            maxProjectionMessages: 16,
            parserSource: Array.from(
                { length: literalCount },
                (_, index) => `{{previouschatlog::${index}}}`,
            ).join(''),
        })
        reader.readConversationWindow = async (query) => ({
            revision: REVISION,
            value: {
                characterId: CHARACTER_ID,
                conversationId: CONVERSATION_ID,
                messages: [currentMessage],
                startIndex: query.startIndex ?? 0,
                endIndex: (query.startIndex ?? 0) + 1,
                totalMessages: currentAbsoluteIndex + 1,
                hasMoreBefore: true,
                hasMoreAfter: false,
            },
        })
        const lease = makeCompleteLease(
            Array.from({ length: currentAbsoluteIndex + 1 }, () => currentMessage),
        )
        input.acquireCompleteProjection = vi.fn(async () => lease)

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('complete')
        expect(input.acquireCompleteProjection).toHaveBeenCalledOnce()
    })

    it('includes the prior character row and two prior user rows needed by parser semantics', async () => {
        const messages: Message[] = [
            { role: 'char', data: 'old char' },
            { role: 'user', data: 'old user' },
            { role: 'char', data: 'middle char' },
            { role: 'user', data: 'first needed user' },
            { role: 'char', data: 'noise char' },
            { role: 'user', data: 'second needed user' },
            { role: 'char', data: 'previous same role' },
            { role: 'user', data: 'near user' },
            { role: 'char', data: 'current' },
        ]
        const { input } = makeInput(messages, 8, { maxProjectionMessages: 8 })

        const result = await createChatParserHistoryProjection(input)

        expect(result).toMatchObject({
            kind: 'bounded',
            historyOffset: 5,
            projectedChatID: 3,
        })
        if (result.kind !== 'bounded') throw new Error('Expected bounded projection')
        expect(result.messages.map((message) => message.data)).toEqual([
            'second needed user',
            'previous same role',
            'near user',
            'current',
        ])
    })

    it('scans large same-role history in linear work', async () => {
        const messageCount = 2_048
        let roleReads = 0
        const messages = Array.from({ length: messageCount }, (_, index) => ({
            get role() {
                roleReads += 1
                return 'char' as const
            },
            data: `turn ${index}`,
        }))
        const { input, reader } = makeInput(messages, messageCount - 1, {
            maxProjectionMessages: messageCount,
        })
        reader.readConversationWindow = async (query) => {
            const startIndex = query.startIndex ?? 0
            const limit = query.limit ?? messageCount
            const endIndex = startIndex + limit
            return {
                revision: REVISION,
                value: {
                    characterId: CHARACTER_ID,
                    conversationId: CONVERSATION_ID,
                    messages: messages.slice(startIndex, endIndex),
                    startIndex,
                    endIndex,
                    totalMessages: messageCount,
                    hasMoreBefore: startIndex > 0,
                    hasMoreAfter: endIndex < messageCount,
                },
            }
        }
        roleReads = 0

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('bounded')
        expect(roleReads).toBeLessThanOrEqual(messageCount * 3 + 2)
    })

    it('falls back before exceeding the bounded budget for a distant literal dependency', async () => {
        const messages = makeMessages(100)
        messages[99].data = '{{previouschatlog::10}}'
        const complete = vi.fn(async () => makeCompleteLease(messages))
        const { input, reads } = makeInput(messages, 99, {
            maxProjectionMessages: 32,
            acquireCompleteProjection: complete,
        })

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('complete')
        expect(complete).toHaveBeenCalledOnce()
        expect(complete).toHaveBeenCalledWith(
            expect.objectContaining({
                reasons: ['projection-budget'],
                currentAbsoluteIndex: 99,
            }),
        )
        expect(reads).toEqual([{ startIndex: 99, limit: 1 }])
    })

    it('falls back when the backward-role scan would exceed the projection budget', async () => {
        const messages = Array.from({ length: 100 }, (_, index) => ({
            role: 'char' as const,
            data: `turn ${index}`,
        }))
        const complete = vi.fn(async () => makeCompleteLease(messages))
        const { input, reads } = makeInput(messages, 99, {
            maxProjectionMessages: 8,
            acquireCompleteProjection: complete,
        })

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('complete')
        expect(complete).toHaveBeenCalledOnce()
        expect(reads).toEqual([
            { startIndex: 99, limit: 1 },
            { startIndex: 92, limit: 7 },
        ])
    })

    it.each([
        '{{lastmessage}}',
        '{{lastmessageid}}',
        '{{pick::one::two}}',
        '{{rollp::1d6}}',
        '{{previouschatlog::{{getvar::target}}}}',
        '<risu-style>not-hex</risu-style>',
    ])('conservatively acquires complete tail or ambiguous evidence for %s', async (source) => {
        const messages = makeMessages(20)
        const complete = vi.fn(async () => makeCompleteLease(messages))
        const { input, reads } = makeInput(messages, 19, {
            parserSource: source,
            acquireCompleteProjection: complete,
        })

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('complete')
        expect(complete).toHaveBeenCalledOnce()
        expect(reads).toEqual([{ startIndex: 19, limit: 1 }])
    })

    it('acquires complete history when the current CBS recursively expands to history', async () => {
        const messages = makeMessages(20)
        messages[19].data = '{{personality}}'
        const complete = vi.fn(async () => makeCompleteLease(messages))
        const { input } = makeInput(messages, 19, {
            parserIndirections: {
                personality: '{{history}}',
            },
            acquireCompleteProjection: complete,
        })

        const result = await createChatParserHistoryProjection(input)

        expect(result).toMatchObject({
            kind: 'complete',
            reasons: ['full-history-cbs'],
        })
        expect(complete).toHaveBeenCalledOnce()
    })

    it.each(['lua', 'plugin-v2', 'display-trigger', 'inject'] as const)(
        'acquires one validated complete projection for unsafe %s processing',
        async (dependency) => {
            const messages = makeMessages(20)
            const complete = vi.fn(async () => makeCompleteLease(messages))
            const { input } = makeInput(messages, 19, {
                unsafeDependencies: [dependency],
                acquireCompleteProjection: complete,
            })

            const result = await createChatParserHistoryProjection(input)

            expect(result).toMatchObject({
                kind: 'complete',
                chatID: 19,
                projectedChatID: 19,
                historyOffset: 0,
                reasons: [dependency],
            })
            expect(complete).toHaveBeenCalledOnce()
        },
    )

    it('uses one complete acquisition when full, unsafe, and budget reasons overlap', async () => {
        const messages = makeMessages(100)
        messages[99].data = '{{history}} {{previouschatlog::0}}'
        let acquiredReasons: readonly string[] = []
        const complete = vi.fn(async (request: { reasons: readonly string[] }) => {
            acquiredReasons = request.reasons
            return makeCompleteLease(messages)
        })
        const { input } = makeInput(messages, 99, {
            unsafeDependencies: ['plugin-v2'],
            maxProjectionMessages: 8,
            acquireCompleteProjection: complete,
        })

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('complete')
        expect(complete).toHaveBeenCalledOnce()
        expect(acquiredReasons).toEqual([
            'plugin-v2',
            'full-history-cbs',
            'projection-budget',
        ])
    })

    it('transfers a validated complete lease to the result without releasing it', async () => {
        const messages = makeMessages(20)
        const lease = makeCompleteLease(messages)
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => lease,
        })

        const result = await createChatParserHistoryProjection(input)

        expect(result.kind).toBe('complete')
        if (result.kind !== 'complete') throw new Error('Expected complete projection')
        expect(result.lease).toBe(lease)
        expect(lease.release).not.toHaveBeenCalled()
    })

    it('fails closed when complete history is required but no callback is available', async () => {
        const messages = makeMessages(100)
        messages[99].data = '{{previouschatlog::0}}'
        const { input } = makeInput(messages, 99, { maxProjectionMessages: 16 })

        await expect(createChatParserHistoryProjection(input)).rejects.toBeInstanceOf(
            ChatParserCompleteProjectionRequiredError,
        )
    })

    it('rejects stale work after an awaited PDS read', async () => {
        const messages = makeMessages(10)
        const { input, reader } = makeInput(messages, 9)
        let current = true
        let finishRead!: () => void
        const originalRead = reader.readConversationWindow.bind(reader)
        reader.readConversationWindow = async (query) => {
            await new Promise<void>((resolve) => {
                finishRead = resolve
            })
            return originalRead(query)
        }
        input.isCurrent = () => current

        const pending = createChatParserHistoryProjection(input)
        current = false
        finishRead()

        await expect(pending).rejects.toBeInstanceOf(ChatParserHistoryProjectionStaleError)
    })

    it('honors abort before reading and after asynchronous work', async () => {
        const messages = makeMessages(10)
        const preAborted = new AbortController()
        preAborted.abort()
        const before = makeInput(messages, 9, { signal: preAborted.signal })
        await expect(createChatParserHistoryProjection(before.input)).rejects.toMatchObject({
            name: 'AbortError',
        })
        expect(before.reads).toEqual([])

        const during = new AbortController()
        const delayed = makeInput(messages, 9, { signal: during.signal })
        const originalRead = delayed.reader.readConversationWindow.bind(delayed.reader)
        delayed.reader.readConversationWindow = async (query) => {
            const result = await originalRead(query)
            during.abort()
            return result
        }
        await expect(createChatParserHistoryProjection(delayed.input)).rejects.toMatchObject({
            name: 'AbortError',
        })
    })

    it('rejects mismatched PDS revision, identity, count, and exact range evidence', async () => {
        const messages = makeMessages(10)
        const cases = [
            { revision: REVISION + 1 },
            { characterId: 'wrong-character' },
            { conversationId: 'wrong-conversation' },
            { totalMessages: messages.length + 1 },
            { startIndex: 8 },
            { endIndex: 9 },
        ]

        for (const replacement of cases) {
            const { input, reader } = makeInput(messages, 9)
            const originalRead = reader.readConversationWindow.bind(reader)
            reader.readConversationWindow = async (query) => {
                const result = await originalRead(query)
                if (!result) return result
                return {
                    ...result,
                    revision: replacement.revision ?? result.revision,
                    value: { ...result.value, ...replacement },
                }
            }

            await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
                /reader|window|revision|conversation/i,
            )
        }
    })

    it.each(['sparse', 'undefined'] as const)(
        'rejects a %s PDS message window before parser projection',
        async (mode) => {
            const messages = makeMessages(10)
            const { input, reader } = makeInput(messages, 9)
            reader.readConversationWindow = async () => {
                const rows = new Array<Message>(1)
                if (mode === 'undefined') rows[0] = undefined as never
                return {
                    revision: REVISION,
                    value: {
                        characterId: CHARACTER_ID,
                        conversationId: CONVERSATION_ID,
                        messages: rows,
                        startIndex: 9,
                        endIndex: 10,
                        totalMessages: 10,
                        hasMoreBefore: true,
                        hasMoreAfter: false,
                    },
                }
            }

            await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
                /conversation window.*message row/i,
            )
        },
    )

    it('rejects a complete callback that returns a partial context as complete', async () => {
        const messages = makeMessages(20)
        const partial = {
            ...makeCompleteLease(messages.slice(10)),
            totalMessages: messages.length,
        }
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => partial,
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection.*message count/i,
        )
        expect(partial.release).toHaveBeenCalledOnce()
    })

    it('rejects a complete callback result without a callable release', async () => {
        const messages = makeMessages(20)
        const malformed = {
            ...makeCompleteLease(messages),
            release: undefined,
        }
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => malformed as never,
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection lease.*release/i,
        )
    })

    it('rejects divergent parser and database character authorities', async () => {
        const messages = makeMessages(20)
        const lease = makeCompleteLease(messages)
        lease.context.parserContext.database.characters[0] = structuredClone(
            lease.context.parserContext.character,
        )
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => lease,
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection.*authority/i,
        )
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('rejects sparse complete message arrays with the expected length', async () => {
        const messages = makeMessages(20)
        const lease = makeCompleteLease(messages)
        const sparse = new Array<Message>(messages.length)
        sparse[messages.length - 1] = messages[messages.length - 1]
        lease.context.parserContext.character.chats[0].message = sparse
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => lease,
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection.*dense/i,
        )
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('rejects an explicit undefined complete message row', async () => {
        const messages = makeMessages(20)
        const lease = makeCompleteLease(messages)
        lease.context.parserContext.character.chats[0].message[2] = undefined as never
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => lease,
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection.*message row/i,
        )
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('rejects complete history whose current row differs from the pinned PDS row', async () => {
        const messages = makeMessages(20)
        const completeMessages = structuredClone(messages)
        completeMessages[19].data = 'stale current row'
        const lease = makeCompleteLease(completeMessages)
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async () => lease,
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection.*current row/i,
        )
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it('does not let complete acquisition mutate the pinned current-row evidence', async () => {
        const messages = makeMessages(20)
        messages[19].generationInfo = { model: 'pinned model' }
        const lease = makeCompleteLease(messages)
        const { input } = makeInput(messages, 19, {
            parserSource: '{{history}}',
            acquireCompleteProjection: async (request) => {
                request.currentMessage.data = 'callback mutation'
                if (!request.currentMessage.generationInfo) {
                    throw new Error('Expected nested generation evidence')
                }
                request.currentMessage.generationInfo.model = 'callback model'
                lease.context.parserContext.character.chats[0].message[19] = request.currentMessage
                return lease
            },
        })

        await expect(createChatParserHistoryProjection(input)).rejects.toThrow(
            /complete projection.*current row/i,
        )
        expect(lease.release).toHaveBeenCalledOnce()
    })

    it.each(['stale', 'abort'] as const)(
        'releases a complete lease when acquisition resolves after %s',
        async (mode) => {
            const messages = makeMessages(20)
            const lease = makeCompleteLease(messages)
            let current = true
            let resolveComplete!: (lease: ChatParserCompleteProjectionLease) => void
            let markAcquisitionStarted!: () => void
            const acquisitionStarted = new Promise<void>((resolve) => {
                markAcquisitionStarted = resolve
            })
            const complete = new Promise<ChatParserCompleteProjectionLease>((resolve) => {
                resolveComplete = resolve
            })
            const abortController = new AbortController()
            const { input } = makeInput(messages, 19, {
                parserSource: '{{history}}',
                signal: abortController.signal,
                isCurrent: () => current,
                acquireCompleteProjection: () => {
                    markAcquisitionStarted()
                    return complete
                },
            })

            const pending = createChatParserHistoryProjection(input)
            await acquisitionStarted
            if (mode === 'stale') current = false
            else abortController.abort()
            resolveComplete(lease)

            await expect(pending).rejects.toMatchObject(
                mode === 'stale'
                    ? { name: 'ChatParserHistoryProjectionStaleError' }
                    : { name: 'AbortError' },
            )
            expect(lease.release).toHaveBeenCalledOnce()
        },
    )
})
