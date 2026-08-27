import { Buffer } from 'buffer'
import { describe, expect, it } from 'vitest'
import {
    classifyChatParserHistory,
    extendChatParserHistoryBounds,
    type ChatParserUnsafeHistoryDependency,
} from './chatParserHistory'

describe('chat parser history classification', () => {
    it.each([
        '{{user-history}}',
        '{{CHAR_HISTORY}}',
        '{{message-unixtime-array}}',
        '{{idle duration}}',
        '{{last_message_index}}',
        '{{roll-pick::1d6}}',
    ])('normalizes full-history CBS aliases in %s', (source) => {
        expect(classifyChatParserHistory({ source })).toMatchObject({
            requiresFullHistory: true,
            reasons: ['full-history-cbs'],
        })
    })

    it('follows recursive CBS indirection before choosing a bounded projection', () => {
        const result = classifyChatParserHistory({
            source: '{{personality}}',
            indirections: {
                personality: '{{history}}',
            },
        })

        expect(result.requiresFullHistory).toBe(true)
        expect(result.reasons).toContain('full-history-cbs')
    })

    it('extends a bounded projection for literal previous-chat-log indices', () => {
        const classification = classifyChatParserHistory({
            source: '{{previous-chat-log :: 10}} {{previous_chat_log::95}}',
        })

        expect(classification).toEqual({
            requiresFullHistory: false,
            absoluteMessageIndices: [10, 95],
            reasons: [],
        })
        expect(
            extendChatParserHistoryBounds(classification, {
                start: 89,
                end: 90,
                totalMessages: 100,
            }),
        ).toEqual({ start: 10, end: 96 })
    })

    it('requires full history when a previouschatlog index is dynamic', () => {
        expect(
            classifyChatParserHistory({
                source: '{{previouschatlog::{{getvar::target}}}}',
            }),
        ).toMatchObject({
            requiresFullHistory: true,
            reasons: ['dynamic-previous-chat-log'],
        })
    })

    it('classifies CBS hidden in decoded risu-style CSS', () => {
        const encodedCss = Buffer.from('.turn::after{content:"{{history}}"}').toString('hex')

        expect(
            classifyChatParserHistory({
                source: `<risu-style>${encodedCss}</risu-style>`,
            }),
        ).toMatchObject({
            requiresFullHistory: true,
            reasons: ['full-history-cbs'],
        })
    })

    it('keeps known-safe risu-style CSS bounded and falls back on ambiguous encoding', () => {
        const safeCss = Buffer.from('.turn{color:red}').toString('hex')
        expect(
            classifyChatParserHistory({
                source: `<risu-style>${safeCss}</risu-style>`,
            }),
        ).toMatchObject({ requiresFullHistory: false, reasons: [] })
        expect(
            classifyChatParserHistory({
                source: '<risu-style>not-hex</risu-style>',
            }),
        ).toMatchObject({
            requiresFullHistory: true,
            reasons: ['ambiguous-risu-style'],
        })
    })

    it.each<ChatParserUnsafeHistoryDependency>([
        'lua',
        'plugin-v2',
        'display-trigger',
        'inject',
    ])('requires full history for unsafe %s processing', (dependency) => {
        expect(
            classifyChatParserHistory({
                source: 'ordinary text',
                unsafeDependencies: [dependency],
            }),
        ).toEqual({
            requiresFullHistory: true,
            absoluteMessageIndices: [],
            reasons: [dependency],
        })
    })

    it('collects parser input strings recursively without following cycles', () => {
        const source: { nested: unknown; self?: unknown } = {
            nested: [{ value: '{{previouschatlog::3}}' }],
        }
        source.self = source

        expect(classifyChatParserHistory({ source })).toEqual({
            requiresFullHistory: false,
            absoluteMessageIndices: [3],
            reasons: [],
        })
    })

    it('ignores requested literal indices outside the available conversation', () => {
        const classification = classifyChatParserHistory({
            source: '{{previouschatlog::500}}',
        })

        expect(
            extendChatParserHistoryBounds(classification, {
                start: 89,
                end: 90,
                totalMessages: 100,
            }),
        ).toEqual({ start: 89, end: 90 })
    })
})
