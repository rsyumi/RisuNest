import { describe, expect, it } from 'vitest'
import {
    CONVERSATION_PATCH_LEDGER_LIMIT,
    CONVERSATION_PATCH_LEDGER_RETENTION_MS,
    createConversationPatchLedger,
    normalizeConversationPatchInput,
    planLiveConversationPatch,
    type ConversationPatchResult,
} from './conversationPatch'

const base = { characterId: 'char', conversationId: 'conv', mutationId: 'm' }
const entry = (set: Record<string, unknown>, extra: Record<string, unknown> = {}) =>
    ({ ...base, messages: [{ index: 0, messageId: 'id', set, ...extra }] })

describe('normalizeConversationPatchInput', () => {
    it('accepts data and plugin fields, removal by undefined, and chat variables', () => {
        const request = normalizeConversationPatchInput({
            ...base,
            messages: [{ index: 2, messageId: null, set: { data: 'text', __tr: { a: [1, 'b', null, true] }, __gone: undefined } }],
            chatVariables: [{ key: '$a', value: 1 }, { key: '$b', value: null }],
        })

        expect(request).toEqual({
            ...base,
            messages: [{ index: 2, messageId: null, set: { data: 'text', __tr: { a: [1, 'b', null, true] }, __gone: undefined } }],
            chatVariables: [{ key: '$a', value: 1 }, { key: '$b', value: null }],
        })
        expect(Object.hasOwn(request.messages[0].set, '__gone')).toBe(true)
    })

    it.each([
        ['a host field', { role: 'user' }],
        ['the bare prefix', { __: 1 }],
        ['__proto__', JSON.parse('{"__proto__": 1}')],
        ['a non-string data', { data: 1 }],
        ['a removed data', { data: undefined }],
        ['a function', { __x: () => 1 }],
        ['a nested undefined', { __x: { a: undefined } }],
        ['a non-finite number', { __x: Number.NaN }],
        ['a date', { __x: new Date(0) }],
        ['a map', { __x: new Map() }],
        ['a bigint', { __x: 1n }],
        ['an array hole', { __x: [1, , 3] }],
    ])('rejects %s in set', (_name, set) => {
        expect(() => normalizeConversationPatchInput(entry(set))).toThrow()
    })

    it('rejects a cycle and an oversized field', () => {
        const cycle: Record<string, unknown> = {}
        cycle.self = cycle
        expect(() => normalizeConversationPatchInput(entry({ __x: cycle }))).toThrow(/cycle/)
        expect(() => normalizeConversationPatchInput(entry({ __x: 'a'.repeat(1_048_575) }))).toThrow(/exceeds/)
        expect(() => normalizeConversationPatchInput(entry({ __x: '가'.repeat(400_000) }))).toThrow(/exceeds/)
        expect(() => normalizeConversationPatchInput(entry({ __x: 'a'.repeat(1_048_574) }))).not.toThrow()
    })

    it.each([
        ['no input', undefined],
        ['an empty character ID', { ...base, characterId: '' }],
        ['a missing conversation ID', { characterId: 'char', mutationId: 'm' }],
        ['an empty mutation ID', { ...base, mutationId: '' }],
        ['a long mutation ID', { ...base, mutationId: 'x'.repeat(129) }],
        ['too many messages', { ...base, messages: Array.from({ length: 65 }, (_, index) => ({ index, messageId: `m${index}`, set: {} })) }],
        ['a repeated index', { ...base, messages: [{ index: 1, messageId: 'a', set: {} }, { index: 1, messageId: 'b', set: {} }] }],
        ['a fractional index', { ...base, messages: [{ index: 1.5, messageId: 'a', set: {} }] }],
        ['a missing message ID', { ...base, messages: [{ index: 1, set: {} }] }],
        ['a missing set', { ...base, messages: [{ index: 1, messageId: 'a' }] }],
        ['too many chat variables', { ...base, chatVariables: Array.from({ length: 65 }, (_, index) => ({ key: `$${index}`, value: 1 })) }],
        ['a repeated chat variable', { ...base, chatVariables: [{ key: '$a', value: 1 }, { key: '$a', value: 2 }] }],
        ['an object chat variable', { ...base, chatVariables: [{ key: '$a', value: {} }] }],
        ['an infinite chat variable', { ...base, chatVariables: [{ key: '$a', value: Infinity }] }],
    ])('rejects %s', (_name, input) => {
        expect(() => normalizeConversationPatchInput(input)).toThrow()
    })

    it('accepts the limits', () => {
        expect(() => normalizeConversationPatchInput({
            ...base,
            mutationId: 'x'.repeat(128),
            messages: Array.from({ length: 64 }, (_, index) => ({ index, messageId: `m${index}`, set: {} })),
            chatVariables: Array.from({ length: 64 }, (_, index) => ({ key: `$${index}`, value: index })),
        })).not.toThrow()
    })
})

describe('planLiveConversationPatch', () => {
    const messages = [
        { role: 'user' as const, data: 'a', chatId: 'a' },
        { role: 'char' as const, data: 'b', chatId: 'b' },
        { role: 'char' as const, data: 'c', chatId: 'c' },
        { role: 'user' as const, data: 'd', chatId: 'd' },
    ]

    it('groups touched messages into runs and leaves untouched neighbors out', () => {
        const request = normalizeConversationPatchInput({
            ...base,
            messages: [{ index: 3, messageId: 'd', set: { __x: 3 } }, { index: 0, messageId: 'a', set: { __x: 0 } }, { index: 1, messageId: 'b', set: { __x: 1 } }],
        })
        const planned = planLiveConversationPatch(request, messages, undefined)

        expect(planned).toEqual({ kind: 'apply', scriptstate: null, runs: [
            { start: 0, messages: [{ ...messages[0], __x: 0 }, { ...messages[1], __x: 1 }] },
            { start: 3, messages: [{ ...messages[3], __x: 3 }] },
        ] })
        expect(messages[0]).not.toHaveProperty('__x')
    })

    it('skips unchanged values and removes plugin fields', () => {
        const source = [{ ...messages[0], __x: 1, __y: 2 }]
        const unchanged = planLiveConversationPatch(normalizeConversationPatchInput({ ...base, messages: [{ index: 0, messageId: 'a', set: { __x: 1 } }], chatVariables: [{ key: '$a', value: 1 }] }), source, { $a: 1 })
        expect(unchanged).toEqual({ kind: 'apply', runs: [], scriptstate: null })
        const removed = planLiveConversationPatch(normalizeConversationPatchInput({ ...base, messages: [{ index: 0, messageId: 'a', set: { __y: undefined } }] }), source, undefined)
        expect(removed).toEqual({ kind: 'apply', scriptstate: null, runs: [{ start: 0, messages: [{ ...messages[0], __x: 1 }] }] })
    })
})

describe('createConversationPatchLedger', () => {
    const outcome = (status: ConversationPatchResult['status'], revision = 1): ConversationPatchResult => ({ status, revision })

    it('replays the first outcome, reporting a first application as already applied', async () => {
        const ledger = createConversationPatchLedger(() => 0)
        await expect(ledger.run('a', async () => outcome('applied', 3))).resolves.toEqual(outcome('applied', 3))
        await expect(ledger.run('a', async () => outcome('conflict'))).resolves.toEqual(outcome('already-applied', 3))
        await expect(ledger.run('c', async () => outcome('conflict', 4))).resolves.toEqual(outcome('conflict', 4))
        await expect(ledger.run('c', async () => outcome('applied'))).resolves.toEqual(outcome('conflict', 4))
    })

    it('records neither busy outcomes nor failures', async () => {
        const ledger = createConversationPatchLedger(() => 0)
        await expect(ledger.run('b', async () => outcome('busy'))).resolves.toEqual(outcome('busy'))
        await expect(ledger.run('b', async () => outcome('applied'))).resolves.toEqual(outcome('applied'))
        await expect(ledger.run('f', async () => { throw new Error('failed') })).rejects.toThrow('failed')
        await expect(ledger.run('f', async () => outcome('applied'))).resolves.toEqual(outcome('applied'))
    })

    it('shares an outcome still in flight', async () => {
        const ledger = createConversationPatchLedger(() => 0)
        let finish!: (value: ConversationPatchResult) => void
        const first = ledger.run('a', () => new Promise((resolve) => { finish = resolve }))
        const second = ledger.run('a', async () => outcome('conflict'))
        finish(outcome('applied', 2))
        await expect(first).resolves.toEqual(outcome('applied', 2))
        await expect(second).resolves.toEqual(outcome('already-applied', 2))
    })

    it('keeps the newest entries for the retention period', async () => {
        let time = 0
        const ledger = createConversationPatchLedger(() => time)
        for (let index = 0; index <= CONVERSATION_PATCH_LEDGER_LIMIT; index++) {
            await ledger.run(`id-${index}`, async () => outcome('applied', index))
        }
        await expect(ledger.run('id-0', async () => outcome('applied', 99))).resolves.toEqual(outcome('applied', 99))
        await expect(ledger.run(`id-${CONVERSATION_PATCH_LEDGER_LIMIT}`, async () => outcome('conflict')))
            .resolves.toEqual(outcome('already-applied', CONVERSATION_PATCH_LEDGER_LIMIT))
        time = CONVERSATION_PATCH_LEDGER_RETENTION_MS
        await expect(ledger.run('id-0', async () => outcome('conflict'))).resolves.toEqual(outcome('already-applied', 99))
        time += 1
        await expect(ledger.run('id-0', async () => outcome('conflict', 5))).resolves.toEqual(outcome('conflict', 5))
    })
})
