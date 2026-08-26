import { describe, expect, it } from 'vitest'
import { createHash } from 'node:crypto'
import type { customscript } from '../storage/database.svelte'
import { executeRegexPlanSync, getRegexExecutionPlan } from './regexExecutionPlan'
import { classifyRegexSafePlan, tokenizeRegexReplacement } from './regexSafePlan'
import { fnv1a, makeRegexFixture } from './tests/phase1Fixtures'
import { createRegexWorkerMessageHandler } from './regexWorker'
import type { RegexWorkerResponse } from './regexWorkerClient'

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
            script('(ab|c){1,2}', '$1', 'gu<order 2>'),
            script('[A-Z]?z', '$&', 'u'),
        ], 'editoutput')

        const result = classifyRegexSafePlan(executionPlan, 'abz')

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

    it('matches the pinned JavaScript authority hash for 100,000 generated safe cases', () => {
        const plans = [
            getRegexExecutionPlan([script('(a)|(b)', '$2$1', 'g')], 'editoutput'),
            getRegexExecutionPlan([script('[A-Z]{1,2}', '$$[$&]', 'gu')], 'editoutput'),
            getRegexExecutionPlan([
                script('(?:x|y){1,3}', '[$`][$&][$\']', 'u'),
            ], 'editoutput'),
            getRegexExecutionPlan([
                script('[0-9]?z', '$&$&', 'g'),
                script('z', 'Z', 'g'),
            ], 'editoutput'),
        ]
        const hash = createHash('sha256')
        const lengthBytes = Buffer.allocUnsafe(4)

        for (let index = 0; index < 100_000; index++) {
            const plan = plans[index % plans.length]
            const input = `🙂abABzxy.${index % 997}\r\n`
            const classification = classifyRegexSafePlan(plan, input)
            expect(classification.accepted).toBe(true)
            const result = executeRegexPlanSync(plan, input, (value) => value)
            const output = Buffer.from(result.data)
            lengthBytes.writeUInt32LE(index)
            hash.update(lengthBytes)
            lengthBytes.writeUInt32LE(output.byteLength)
            hash.update(lengthBytes)
            hash.update(output)
            lengthBytes.writeUInt32LE(result.errors.length)
            hash.update(lengthBytes)
            for (const error of result.errors) {
                lengthBytes.writeUInt32LE(error.sourceIndex)
                hash.update(lengthBytes)
            }
        }

        expect(hash.digest('hex')).toBe(
            '6fc3f4ec46d9a5806b576b005aac64beba109e174d0e3e143dcb3cd93e19e4f5',
        )
    }, 30_000)

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

    it('matches the pinned JavaScript Worker hash for the generated corpus', () => {
        const plans = [
            getRegexExecutionPlan([script('(a)|(b)', '$2$1', 'g')], 'editoutput'),
            getRegexExecutionPlan([script('[A-Z]{1,2}', '$$[$&]', 'gu')], 'editoutput'),
            getRegexExecutionPlan([
                script('(?:x|y){1,3}', '[$`][$&][$\']', 'u'),
            ], 'editoutput'),
            getRegexExecutionPlan([
                script('[0-9]?z', '$&$&', 'g'),
                script('z', 'Z', 'g'),
            ], 'editoutput'),
        ]
        const responses: Array<RegexWorkerResponse | undefined> = []
        const handlers = plans.map((plan, planIndex) => {
            const handler = createRegexWorkerMessageHandler((response) => {
                responses[planIndex] = response
            })
            handler({
                type: 'register',
                revision: plan.revision,
                entries: plan.entries.map((entry) => [
                    entry.sourceIndex,
                    entry.pattern,
                    entry.replacement,
                    entry.flags,
                ]),
            })
            return handler
        })
        const hash = createHash('sha256')
        const lengthBytes = Buffer.allocUnsafe(4)

        for (let index = 0; index < 100_000; index++) {
            const planIndex = index % plans.length
            handlers[planIndex]({
                type: 'execute',
                id: index,
                revision: plans[planIndex].revision,
                input: `🙂abABzxy.${index % 997}\r\n`,
            })
            const response = responses[planIndex]
            if (response?.type !== 'result') {
                throw new Error('Generated Worker fixture did not return a result')
            }
            const output = Buffer.from(response.data)
            lengthBytes.writeUInt32LE(index)
            hash.update(lengthBytes)
            lengthBytes.writeUInt32LE(output.byteLength)
            hash.update(lengthBytes)
            hash.update(output)
            lengthBytes.writeUInt32LE(response.errors.length)
            hash.update(lengthBytes)
            for (const [sourceIndex] of response.errors) {
                lengthBytes.writeUInt32LE(sourceIndex)
                hash.update(lengthBytes)
            }
        }

        expect(hash.digest('hex')).toBe(
            '6fc3f4ec46d9a5806b576b005aac64beba109e174d0e3e143dcb3cd93e19e4f5',
        )
    }, 30_000)

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
