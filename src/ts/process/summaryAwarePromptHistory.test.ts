import { describe, expect, it } from 'vitest'
import {
    planSummaryAwarePromptHistory,
    planSummaryAwarePromptMetadata,
} from './summaryAwarePromptHistory'

const chat = (messages: any[], summaries: any[]) => ({
    id: 'chat',
    name: 'Chat',
    message: messages,
    hypaV3Data: { summaries },
} as any)

describe('summary-aware prompt history admission', () => {
    it('resolves the exact effective boundary across disabled and allBefore rows', () => {
        const decision = planSummaryAwarePromptHistory(chat([
            { chatId: 'old', data: 'old', role: 'user' },
            { chatId: 'reset', data: 'reset', role: 'user', disabled: 'allBefore' },
            { chatId: 'a', data: 'a', role: 'user' },
            { chatId: 'disabled', data: 'x', role: 'user', disabled: true },
            { chatId: 'b', data: 'b', role: 'char' },
            { chatId: 'c', data: 'c', role: 'user' },
        ], [{ chatMemos: ['a', 'b'] }]), false)
        expect(decision.route).toBe('summary-aware')
        if (decision.route === 'summary-aware') {
            expect([...decision.plan.coveredMessageIds]).toEqual(['a', 'b'])
            expect(decision.plan.effectiveMessageMemos).toEqual(['a', 'b', 'c'])
        }
    })

    it.each([
        ['duplicate-message-id', [{ chatId: 'a', data: 'a' }, { chatId: 'a', data: 'b' }]],
        ['missing-message-id', [{ data: 'a' }]],
        ['summarized-message-has-dynamic-processing', [{ chatId: 'a', data: '{{getvar::x}}' }]],
        ['summarized-message-has-dynamic-processing', [{ chatId: 'a', data: null }]],
    ])('falls back for %s', (reason, messages) => {
        expect(planSummaryAwarePromptHistory(
            chat(messages, [{ chatMemos: ['a'] }]),
            false,
        )).toMatchObject({ route: 'complete', reason })
    })

    it('falls back instead of reinterpreting malformed summary metadata', () => {
        expect(planSummaryAwarePromptHistory(chat([
            { chatId: 'a', data: 'a' },
        ], [
            { chatMemos: ['a'] },
            { chatMemos: 'a' },
        ]), false)).toEqual({ route: 'complete', reason: 'invalid-summary-shape' })
    })

    it('applies the existing orphan cleanup policy before choosing the last summary', () => {
        const decision = planSummaryAwarePromptHistory(chat([
            { chatId: 'a', data: 'a' },
            { chatId: 'b', data: 'b' },
        ], [
            { chatMemos: ['a'] },
            { chatMemos: ['missing'] },
        ]), false)
        expect(decision).toMatchObject({ route: 'summary-aware', plan: { boundaryMemo: 'a' } })
        expect(planSummaryAwarePromptHistory(chat([
            { chatId: 'a', data: 'a' },
        ], [{ chatMemos: ['missing'] }]), true)).toMatchObject({
            route: 'complete',
            reason: 'unresolved-summary-boundary',
        })
    })

    it('keeps disabled rows out of metadata boundary ordinals without renumbering bodies', () => {
        const conversation = chat([], [{ chatMemos: ['a', 'b'] }])
        const decision = planSummaryAwarePromptMetadata(conversation, [
            { chatId: 'a', role: 'user', parserInert: true },
            { chatId: 'disabled', role: 'char', disabled: true, parserInert: true },
            { chatId: 'b', role: 'char', parserInert: true },
            { chatId: 'c', role: 'user', parserInert: true },
        ], false)
        expect(decision).toMatchObject({
            route: 'summary-aware',
            plan: {
                boundaryMemo: 'b',
                bodyStartIndex: 3,
                effectiveMessageMemos: ['a', 'b', 'c'],
            },
        })
    })

    it('uses the complete route when allBefore would require omitted greeting history', () => {
        const conversation = chat([], [{ chatMemos: ['a'] }])
        expect(planSummaryAwarePromptMetadata(conversation, [
            { chatId: 'reset', role: 'user', disabled: 'allBefore', parserInert: true },
            { chatId: 'a', role: 'user', parserInert: true },
            { chatId: 'b', role: 'char', parserInert: true },
        ], false)).toEqual({
            route: 'complete',
            reason: 'all-before-before-summary-boundary',
        })
    })

    it('falls back when any metadata row can run history-dependent parsing', () => {
        const conversation = chat([], [{ chatMemos: ['a'] }])
        expect(planSummaryAwarePromptMetadata(conversation, [
            { chatId: 'a', role: 'user', parserInert: true },
            { chatId: 'b', role: 'char', parserInert: false },
        ], false)).toEqual({
            route: 'complete',
            reason: 'summarized-message-has-dynamic-processing',
        })
    })
})
