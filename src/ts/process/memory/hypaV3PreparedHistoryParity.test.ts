import { beforeEach, describe, expect, it, vi } from 'vitest'

const requestCalls = vi.hoisted(() => [] as any[][])

vi.mock('../transformers', () => ({
    runEmbedding: vi.fn(async (texts: string[]) => texts.map(() => new Float32Array([1, 0]))),
}))
vi.mock('src/ts/parser/parser.svelte', async () =>
    (await import('../tests/sendChatTestHarness')).parserModule())
vi.mock('../modules', async () =>
    (await import('../tests/sendChatTestHarness')).modulesModule())
vi.mock('../request/request', () => ({
    requestChatData: vi.fn(async ({ formated }: { formated: any[] }) => {
        requestCalls.push(structuredClone(formated))
        return { type: 'success', result: 'New summary' }
    }),
}))

import { setDatabaseLite, type Chat, type character } from 'src/ts/storage/database.svelte'
import { createHypaV3Preset, hypaMemoryV3 } from './hypav3'
import { planSummaryAwareProcessedHistory } from '../summaryAwarePromptHistory'
import { getRegexExecutionPlan, executeRegexPlanSync } from '../regexExecutionPlan'

function tokenizer(calls: string[]) {
    const count = (chat: { content: string; memo?: string }) => {
        calls.push(chat.memo ?? chat.content)
        return chat.content.length + 1
    }
    return {
        tokenizeChat: vi.fn(async (chat) => count(chat)),
        tokenizeChats: vi.fn(async (chats) => chats.reduce(
            (total: number, chat: any) => total + count(chat),
            0,
        )),
    } as any
}

function tokenTotal(chats: Array<{ content: string }>) {
    return chats.reduce((total, chat) => total + chat.content.length + 1, 128)
}

beforeEach(() => {
    requestCalls.length = 0
    const preset = createHypaV3Preset('Parity', {
        memoryTokensRatio: 0.4,
        maxChatsPerSummary: 100,
        recentMemoryRatio: 1,
        similarMemoryRatio: 0,
        queryChatCount: 1,
        preserveOrphanedMemory: false,
    })
    setDatabaseLite({
        maxResponse: 128,
        hypaModel: 'local-MiniLM-L6-v2',
        hypaV3PresetId: 0,
        hypaV3Presets: [preset],
    } as never)
    vi.spyOn(console, 'log').mockImplementation(() => undefined)
})

describe('Hypa V3 prepared history parity', () => {
    it('preserves real Hypa output and memory when plain regex preprocessing omits covered bodies', async () => {
        const room = {
            id: 'chat', name: 'Chat',
            message: ['a', 'b', 'c'].map(chatId => ({ role: 'user', data: 'old ' + chatId, chatId })),
            hypaV3Data: { summaries: [{ text: 'Earlier events', chatMemos: ['a', 'b'], isImportant: false }] },
        } as Chat
        const scripts = [{ type: 'editprocess', in: 'old', out: 'new expanded', flag: 'g', ableFlag: true, comment: '' }]
        const decision = planSummaryAwareProcessedHistory(room, scripts, false, 1, 'character')
        expect(decision.route).toBe('summary-aware')
        if (decision.route !== 'summary-aware') throw new Error('Expected an eligible fixture')
        const regexPlan = getRegexExecutionPlan(scripts, 'editprocess')
        const process = (skip: boolean) => room.message
            .filter(message => !skip || !decision.plan.coveredMessageIds.has(message.chatId!))
            .map(message => ({ role: 'user', content: executeRegexPlanSync(regexPlan, message.data, text => text).data, memo: message.chatId }))
        const fullChats = process(false) as any[]
        const preparedChats = process(true) as any[]
        const owner = { type: 'character', chaId: 'character', name: 'Character' } as character
        const complete = await hypaMemoryV3(fullChats, tokenTotal(fullChats), 4096, structuredClone(room), owner, tokenizer([]))
        const completeRequests = structuredClone(requestCalls)
        requestCalls.length = 0
        const prepared = await hypaMemoryV3(preparedChats, tokenTotal(preparedChats), 4096, structuredClone(room), owner, tokenizer([]), {
            boundaryMemo: decision.plan.boundaryMemo,
            effectiveMessageMemos: decision.plan.effectiveMessageMemos,
            historyStartIndex: 0,
        })
        expect(prepared.chats).toEqual(complete.chats)
        expect(prepared.currentTokens).toEqual(complete.currentTokens)
        expect(prepared.memory).toEqual(complete.memory)
        expect(requestCalls).toEqual(completeRequests)
        expect(room.message.map(message => message.data)).toEqual(['old a', 'old b', 'old c'])
    })

    it('preserves memory selection, output, token budget, and stored metrics', async () => {
        const prefix = [
            { role: 'user', content: 'covered user', memo: 'a' },
            { role: 'assistant', content: 'covered assistant', memo: 'b' },
        ] as const
        const preamble = [{ role: 'system', content: '[Start a new chat]', memo: 'NewChat' }] as const
        const suffix = [{ role: 'user', content: 'unsummarized tail', memo: 'c' }] as const
        const fullChats = [...preamble, ...prefix, ...suffix] as any[]
        const preparedChats = [...preamble, ...suffix] as any[]
        const room = {
            id: 'chat',
            name: 'Chat',
            message: [],
            hypaV3Data: {
                summaries: [{
                    text: 'Earlier events',
                    chatMemos: ['a', 'b'],
                    isImportant: false,
                }],
            },
        } as unknown as Chat
        const character = { type: 'character', chaId: 'character', name: 'Character' } as character
        const fullCalls: string[] = []
        const preparedCalls: string[] = []

        const complete = await hypaMemoryV3(
            structuredClone(fullChats),
            tokenTotal(fullChats),
            4_096,
            structuredClone(room),
            character,
            tokenizer(fullCalls),
        )
        const prepared = await hypaMemoryV3(
            structuredClone(preparedChats),
            tokenTotal(preparedChats),
            4_096,
            structuredClone(room),
            character,
            tokenizer(preparedCalls),
            {
                boundaryMemo: 'b',
                effectiveMessageMemos: ['a', 'b', 'c'],
                historyStartIndex: preamble.length,
            },
        )

        expect(prepared.chats).toEqual(complete.chats)
        expect(prepared.currentTokens).toBe(complete.currentTokens)
        expect(prepared.memory).toEqual(complete.memory)
        expect(fullCalls).toEqual(expect.arrayContaining(['a', 'b']))
        expect(preparedCalls).not.toEqual(expect.arrayContaining(['a', 'b']))
    })

    it('preserves additional summary request grouping and order', async () => {
        const preset = createHypaV3Preset('Additional summary parity', {
            summarizationModel: 'subModel',
            memoryTokensRatio: 0.2,
            extraSummarizationRatio: 0,
            maxChatsPerSummary: 2,
            recentMemoryRatio: 1,
            similarMemoryRatio: 0,
            queryChatCount: 1,
            preserveOrphanedMemory: false,
        })
        setDatabaseLite({
            maxResponse: 128,
            hypaModel: 'local-MiniLM-L6-v2',
            hypaV3PresetId: 0,
            hypaV3Presets: [preset],
        } as never)
        const preamble = [{ role: 'system', content: 'start', memo: 'NewChat' }]
        const prefix = [
            { role: 'user', content: 'covered-a', memo: 'a' },
            { role: 'assistant', content: 'covered-b', memo: 'b' },
        ]
        const suffix = ['c', 'd', 'e'].map((memo, index) => ({
            role: index % 2 ? 'assistant' : 'user',
            content: `${memo}-${'x'.repeat(80)}`,
            memo,
        }))
        const room = {
            id: 'chat',
            name: 'Chat',
            message: [],
            hypaV3Data: {
                summaries: [{ text: 'Earlier events', chatMemos: ['a', 'b'], isImportant: false }],
            },
        } as unknown as Chat
        const character = { type: 'character', chaId: 'character', name: 'Character' } as character
        const fullChats = [...preamble, ...prefix, ...suffix] as any[]
        const preparedChats = [...preamble, ...suffix] as any[]

        const complete = await hypaMemoryV3(
            structuredClone(fullChats), tokenTotal(fullChats), 180,
            structuredClone(room), character, tokenizer([]),
        )
        const completeRequests = structuredClone(requestCalls)
        requestCalls.length = 0
        const prepared = await hypaMemoryV3(
            structuredClone(preparedChats), tokenTotal(preparedChats), 180,
            structuredClone(room), character, tokenizer([]), {
                boundaryMemo: 'b',
                effectiveMessageMemos: ['a', 'b', 'c', 'd', 'e'],
                historyStartIndex: preamble.length,
            },
        )

        expect(requestCalls).toEqual(completeRequests)
        expect(requestCalls).toHaveLength(1)
        expect(JSON.stringify(requestCalls[0])).toContain('c-')
        expect(JSON.stringify(requestCalls[0])).toContain('d-')
        expect(JSON.stringify(requestCalls[0])).not.toContain('e-')
        expect(prepared.chats).toEqual(complete.chats)
        expect(prepared.currentTokens).toBe(complete.currentTokens)
        expect(prepared.memory).toEqual(complete.memory)
    })
})
