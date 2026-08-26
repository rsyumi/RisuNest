import { describe, expect, it } from 'vitest'
import type { customscript } from '../storage/database.svelte'
import { roadmap14Corpus } from '../storage/tests/roadmap14/losslessCorpus'
import {
    canExecuteRegexPlanInWorker,
    executeRegexPlanSync,
    getRegexExecutionPlan,
} from './regexExecutionPlan'
import { classifyRegexSafePlan, tokenizeRegexReplacement } from './regexSafePlan'
import { fnv1a, makeRegexFixture } from './tests/phase1Fixtures'

function script(pattern: string, replacement = 'x', flag = 'g'): customscript {
    return {
        comment: '',
        in: pattern,
        out: replacement,
        type: 'editoutput',
        flag,
        ableFlag: true,
    }
}

describe('Rust regex safe-plan classifier', () => {
    it('lowers an ordered ASCII literal plan into neutral IR', () => {
        const executionPlan = getRegexExecutionPlan([
            script('(?:ab|c){1,2}(d)', '$1', 'gu<order 2>'),
            script('[A-Z]?z', '$&', 'u'),
        ], 'editoutput')

        const result = classifyRegexSafePlan(executionPlan, 'abdz')

        expect(result).toMatchObject({
            accepted: true,
            plan: {
                version: 1,
                entries: [
                    { sourceIndex: 0, global: true, captureCount: 1 },
                    { sourceIndex: 1, global: false, captureCount: 0 },
                ],
            },
        })
    })

    it('rejects a pattern over the per-pattern source limit', () => {
        const executionPlan = getRegexExecutionPlan([
            script('a'.repeat(4_097)),
        ], 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'a')).toEqual({
            accepted: false,
            category: 'regex_safe_pattern_limit',
            sourceIndex: 0,
        })
    })

    it('rejects an input over the shadow executor limit', () => {
        const executionPlan = getRegexExecutionPlan([script('a')], 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'a'.repeat(1_048_577))).toEqual({
            accepted: false,
            category: 'regex_safe_input_limit',
        })
    })

    it('rejects an input below a route minimum before lowering rules', () => {
        const executionPlan = getRegexExecutionPlan([script('é')], 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'a', { minInputBytes: 2 })).toEqual({
            accepted: false,
            category: 'regex_safe_input_minimum',
        })
    })

    it('rejects plans over the aggregate pattern limit', () => {
        const scripts = Array.from({ length: 17 }, (_value, index) => (
            script(`${String.fromCharCode(65 + index)}${'a'.repeat(4_095)}`)
        ))
        const executionPlan = getRegexExecutionPlan(scripts, 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'a')).toEqual({
            accepted: false,
            category: 'regex_safe_pattern_total_limit',
        })
    })

    it('rejects plans over the aggregate replacement limit', () => {
        const executionPlan = getRegexExecutionPlan([
            script('a', 'x'.repeat(65_537)),
        ], 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'a')).toEqual({
            accepted: false,
            category: 'regex_safe_replacement_total_limit',
        })
    })

    it.each([
        ['lookahead', 'a(?=b)', 'g'],
        ['lookbehind', '(?<=a)b', 'g'],
        ['backreference', '(a)\\1', 'g'],
        ['named capture', '(?<name>a)', 'g'],
        ['start anchor', '^a', 'g'],
        ['word boundary', '\\ba', 'g'],
        ['dot', '.', 'g'],
        ['negated class', '[^a]', 'g'],
        ['star', 'a*', 'g'],
        ['plus', 'a+', 'g'],
        ['lazy quantifier', 'a??b', 'g'],
        ['open quantifier', 'a{1,}', 'g'],
        ['oversized quantifier', 'a{1,65}', 'g'],
        ['nested quantifier', '(a?){2}b', 'g'],
        ['digit class escape', '\\d', 'g'],
        ['word class escape', '\\w', 'g'],
        ['space class escape', '\\s', 'g'],
        ['Unicode property escape', '\\p{Letter}', 'u'],
        ['Unicode escape', '\\u0061', 'u'],
        ['hex escape', '\\x61', 'g'],
        ['identity letter escape', '\\a', 'g'],
        ['non-ASCII literal', 'é', 'u'],
        ['empty alternative', 'a|', 'g'],
        ['nullable group', '(a?)b', 'g'],
        ['indices flag', 'a', 'dg'],
        ['case-insensitive flag', 'a', 'gi'],
        ['multiline flag', 'a', 'gm'],
        ['dot-all flag', 'a', 'gs'],
        ['Unicode-sets flag', 'a', 'gv'],
        ['sticky flag', 'a', 'gy'],
    ])('rejects unsupported %s syntax or flags', (_name, pattern, flag) => {
        const executionPlan = getRegexExecutionPlan([
            script(pattern, 'x', flag),
        ], 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'ab')).toMatchObject({
            accepted: false,
        })
    })

    it('tokenizes the ECMAScript replacement contract without named captures', () => {
        expect(tokenizeRegexReplacement(
            '$$|$&|$`|$\'|$1|$2|$10|$99|$01|$<name>',
            10,
        )).toEqual([
            { kind: 'literal', value: '$|' },
            { kind: 'match' },
            { kind: 'literal', value: '|' },
            { kind: 'prefix' },
            { kind: 'literal', value: '|' },
            { kind: 'suffix' },
            { kind: 'literal', value: '|' },
            { kind: 'capture', index: 1 },
            { kind: 'literal', value: '|' },
            { kind: 'capture', index: 2 },
            { kind: 'literal', value: '|' },
            { kind: 'capture', index: 10 },
            { kind: 'literal', value: '|' },
            { kind: 'capture', index: 9 },
            { kind: 'literal', value: '9|' },
            { kind: 'capture', index: 1 },
            { kind: 'literal', value: '|$<name>' },
        ])
    })

    it('reports the first rejected entry in ordered execution order', () => {
        const executionPlan = getRegexExecutionPlan([
            script('a', 'x', 'g<order 1>'),
            script('.', 'x', 'g<order 2>'),
            script('^b', 'x', 'g<order 3>'),
        ], 'editoutput')

        expect(classifyRegexSafePlan(executionPlan, 'ab')).toEqual({
            accepted: false,
            category: 'regex_safe_ast_node',
            sourceIndex: 2,
        })
    })

    it('rejects captures under repetition instead of changing ECMAScript reset semantics', () => {
        const executionPlan = getRegexExecutionPlan([
            script('((a)|(b)){1,2}', '$3'),
        ], 'editoutput')
        const authority = executeRegexPlanSync(executionPlan, 'ba', (value) => value)

        expect(authority.data).toBe('')
        expect(classifyRegexSafePlan(executionPlan, 'ba')).toEqual({
            accepted: false,
            category: 'regex_safe_capture_under_repeat',
            sourceIndex: 0,
        })
    })

    it('accepts bounded structured nesting and rejects deeper ASTs', () => {
        const boundaryPattern = `${'(?:'.repeat(29)}a${')'.repeat(29)}`
        const concatPattern = `${'(?:'.repeat(29)}ab${')'.repeat(29)}`
        const classPattern = `${'(?:'.repeat(29)}[a-b]${')'.repeat(29)}`
        let structuredPattern = '[a-bx-z]'
        for (let depth = 0; depth < 29; depth++) {
            structuredPattern = `(?:x${structuredPattern}|y)`
        }
        const pattern = `${'(?:'.repeat(30)}a${')'.repeat(30)}`
        const boundaryPlan = getRegexExecutionPlan([script(boundaryPattern)], 'editoutput')
        const executionPlan = getRegexExecutionPlan([script(pattern)], 'editoutput')

        expect(classifyRegexSafePlan(boundaryPlan, 'a')).toMatchObject({ accepted: true })
        expect(classifyRegexSafePlan(
            getRegexExecutionPlan([script(concatPattern)], 'editoutput'),
            'ab',
        )).toMatchObject({ accepted: true })
        expect(classifyRegexSafePlan(
            getRegexExecutionPlan([script(classPattern)], 'editoutput'),
            'a',
        )).toMatchObject({ accepted: true })
        expect(classifyRegexSafePlan(
            getRegexExecutionPlan([script(structuredPattern)], 'editoutput'),
            `${'x'.repeat(29)}a`,
        )).toMatchObject({ accepted: true })
        expect(classifyRegexSafePlan(executionPlan, 'a')).toEqual({
            accepted: false,
            category: 'regex_safe_nest_limit',
            sourceIndex: 0,
        })
    })

    it('rejects ill-formed UTF-16 input and replacement values', () => {
        const loneHighSurrogate = String.fromCharCode(0xd800)
        const loneLowSurrogate = String.fromCharCode(0xdc00)
        const inputPlan = getRegexExecutionPlan([script('a')], 'editoutput')
        const replacementPlan = getRegexExecutionPlan([
            script('a', loneLowSurrogate),
        ], 'editoutput')

        expect(classifyRegexSafePlan(inputPlan, loneHighSurrogate)).toEqual({
            accepted: false,
            category: 'regex_safe_input_utf16',
        })
        expect(classifyRegexSafePlan(replacementPlan, 'a')).toEqual({
            accepted: false,
            category: 'regex_safe_replacement_utf16',
            sourceIndex: 0,
        })
    })

    it.each([20, 100, 500] as const)(
        'accepts the current %i-rule Phase 1 project fixture as one complete plan',
        (ruleCount) => {
            const fixture = makeRegexFixture(ruleCount)
            const plan = getRegexExecutionPlan(fixture.scripts, 'editoutput')

            const classification = classifyRegexSafePlan(plan, fixture.input)
            const authority = executeRegexPlanSync(plan, fixture.input, (value) => value)

            expect(classification).toMatchObject({
                accepted: true,
                plan: { entries: { length: ruleCount } },
            })
            expect(fnv1a(authority.data)).toBe(fixture.expectedHash)
            expect(authority.errors).toEqual([])
        },
    )

    it('keeps Rust-safe eligibility above the gate for current project fixtures', () => {
        const compatibilityScripts = roadmap14Corpus.database.characters.flatMap(
            (character) => character.customscript ?? [],
        )
        const fixtures = [
            ...([20, 100, 500] as const).map((ruleCount) => makeRegexFixture(ruleCount)),
            { scripts: compatibilityScripts, input: 'fixture' },
            { scripts: [script('(?=a)a')], input: 'a' },
            { scripts: [script('(a)\\1')], input: 'aa' },
            { scripts: [script('(?<named>a)')], input: 'a' },
            { scripts: [script('a', 'x', 'y')], input: 'a' },
            { scripts: [script('[')], input: 'a' },
        ]
        const workerEligible = fixtures
            .map(({ scripts, input }) => ({
                plan: getRegexExecutionPlan(scripts, 'editoutput'),
                input,
            }))
            .filter(({ plan, input }) => canExecuteRegexPlanInWorker(plan, input))
        const rustSafe = workerEligible.filter(
            ({ plan, input }) => classifyRegexSafePlan(plan, input).accepted,
        )

        expect(workerEligible).toHaveLength(9)
        expect(rustSafe).toHaveLength(4)
        expect(rustSafe.length / workerEligible.length).toBeGreaterThanOrEqual(0.25)
    })

    it('rejects every generated forbidden-AST mutation', () => {
        const safePatterns = ['a', '[A-Z]', '(a|b)', '(?:x|y){1,3}']
        const mutate = [
            (pattern: string) => `(?=${pattern})${pattern}`,
            (pattern: string) => `(${pattern})\\1`,
            (pattern: string) => `^${pattern}`,
            (pattern: string) => `${pattern}+`,
            (pattern: string) => `(?:${pattern})*`,
            (pattern: string) => `${pattern}??b`,
            (pattern: string) => `(?<named>${pattern})`,
            (_pattern: string) => '[^a]',
        ]

        for (let index = 0; index < 10_000; index++) {
            const pattern = mutate[index % mutate.length](
                safePatterns[index % safePatterns.length],
            )
            const plan = getRegexExecutionPlan([script(pattern)], 'editoutput')
            expect(classifyRegexSafePlan(plan, 'abxy')).toMatchObject({ accepted: false })
        }
    })

    it.each([
        ['mixed safe and lookaround', [script('a'), script('(?=b)b')], 'ab'],
        ['CBS action', [script('a', 'x', 'g<cbs>')], 'a'],
        ['stateful action', [script('a', 'x', 'g<inject>')], 'a'],
        ['directive', [script('a', '@@emo happy')], 'a'],
        ['invalid regex', [script('[')], 'a'],
        ['parser-risk replacement', [script('a', '{value')], 'a'],
        ['parser-risk input', [script('a')], 'a<input'],
    ])('rejects current-project %s plans as a whole', (_name, scripts, input) => {
        const plan = getRegexExecutionPlan(scripts, 'editoutput')

        expect(classifyRegexSafePlan(plan, input)).toMatchObject({ accepted: false })
    })
})
