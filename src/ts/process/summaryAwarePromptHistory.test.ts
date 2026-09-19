import { describe, expect, it } from 'vitest'
import {
    planSummaryAwarePromptHistory,
    planSummaryAwarePromptMetadata,
    planSummaryAwareProcessedHistory,
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


describe('summary-aware preprocessing with a complete compatibility history', () => {
    it('admits plain substitutions and output-only rules without removing stored messages', () => {
        const source = chat([
            { chatId: 'a', role: 'user', data: 'covered' },
            { chatId: 'b', role: 'user', data: 'recent' },
        ], [{ chatMemos: ['a'] }])
        const original = structuredClone(source)
        const decision = planSummaryAwareProcessedHistory(source, [
            { type: 'editprocess', in: 'covered', out: 'replaced', flag: 'g', ableFlag: true, comment: '' },
            { type: 'editoutput', in: '.', out: '@@inject', flag: 'g', ableFlag: true, comment: '' },
        ], false, 1, 'character')
        expect(decision.route).toBe('summary-aware')
        expect(source).toEqual(original)
    })

    it.each(['@@inject', '@@repeat_back', '{{setvar::x::1}}', '<tag>'])
    ('retains complete processing for nonlocal or parser-producing replacement %s', (out) => {
        expect(planSummaryAwareProcessedHistory(chat([
            { chatId: 'a', role: 'user', data: 'covered' },
        ], [{ chatMemos: ['a'] }]), [
            { type: 'editprocess', in: '.', out, flag: 'g', ableFlag: true, comment: '' },
        ], false, 0, 'character')).toMatchObject({ route: 'complete' })
    })

    it('retains the covered suffix needed by a memory similarity query', () => {
        expect(planSummaryAwareProcessedHistory(chat([
            { chatId: 'a', role: 'user', data: 'covered' },
            { chatId: 'b', role: 'user', data: 'recent' },
        ], [{ chatMemos: ['a'] }]), [], false, 2, 'character')).toMatchObject({
            route: 'complete', reason: 'memory-query-needs-covered-history',
        })
    })
})
