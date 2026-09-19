import { describe, expect, it } from 'vitest'
import { planSummaryAwarePromptHistory } from './summaryAwarePromptHistory'

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
})
