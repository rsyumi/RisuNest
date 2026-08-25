import { describe, expect, it, vi } from 'vitest'
import type { customscript } from '../storage/database.svelte'
import { setRuntimePerformanceProfile } from '../runtimePerformanceProfile'
import { executeRegexPlanSync, getRegexExecutionPlan } from './regexExecutionPlan'

function script(
    input: string,
    output: string,
    flag = 'g',
    ableFlag = true,
): customscript {
    return {
        comment: '',
        in: input,
        out: output,
        type: 'editoutput',
        flag,
        ableFlag,
    }
}

const identity = (value: string) => value

describe('regex execution plans', () => {
    it('reuses a plan for the same ordered script content', () => {
        const scripts = [script('a', 'b')]

        const first = getRegexExecutionPlan(scripts, 'editoutput')
        const second = getRegexExecutionPlan(scripts.map((value) => ({ ...value })), 'editoutput')
        const changed = getRegexExecutionPlan([script('a', 'c')], 'editoutput')

        expect(second).toBe(first)
        expect(Number.isSafeInteger(first.revision)).toBe(true)
        expect(changed).not.toBe(first)
        expect(changed.revision).toBeGreaterThan(first.revision)
    })

    it('clears retained plans when switching to the lower low-spec budget', () => {
        setRuntimePerformanceProfile('normal')
        const scripts = [script('profile-cache', 'changed')]
        const beforeSwitch = getRegexExecutionPlan(scripts, 'editoutput')

        setRuntimePerformanceProfile('low-spec')
        const afterSwitch = getRegexExecutionPlan(scripts, 'editoutput')
        setRuntimePerformanceProfile('normal')

        expect(afterSwitch).not.toBe(beforeSwitch)
        expect(afterSwitch.revision).toBeGreaterThan(beforeSwitch.revision)
    })

    it('applies descending order metadata with stable ties', () => {
        const plan = getRegexExecutionPlan([
            script('a', 'b', 'g<order 1>'),
            script('b', 'c', 'g<order 2>'),
            script('b', 'd', 'g<order 1>'),
        ], 'editoutput')

        expect(executeRegexPlanSync(plan, 'a', identity).data).toBe('d')
    })

    it('uses default flags when flags are disabled', () => {
        const plan = getRegexExecutionPlan([
            script('A', 'x', 'i', false),
        ], 'editoutput')

        expect(executeRegexPlanSync(plan, 'a A', identity).data).toBe('a x')
    })

    it('normalizes supported flags and removes duplicates', () => {
        const plan = getRegexExecutionPlan([
            script('a', 'x', ' gggi!! '),
        ], 'editoutput')

        expect(executeRegexPlanSync(plan, 'a A', identity).data).toBe('x x')
    })

    it('falls back to unicode mode when no supported flag remains', () => {
        const plan = getRegexExecutionPlan([
            script('a', 'x', '!!!'),
        ], 'editoutput')

        expect(executeRegexPlanSync(plan, 'a a', identity).data).toBe('x a')
    })

    it('feeds each replacement through the parser before the next rule', () => {
        const plan = getRegexExecutionPlan([
            script('a', 'b'),
            script('B', 'c'),
        ], 'editoutput')

        expect(executeRegexPlanSync(plan, 'a', (value) => value.toUpperCase()).data).toBe('C')
    })

    it.each([
        {
            name: 'lookaround',
            rules: [script('(?<=foo)bar', 'baz')],
            input: 'foobar',
            expected: 'foobaz',
        },
        {
            name: 'numbered backreference replacement',
            rules: [script('(a)(b)', '$2$1')],
            input: 'ab',
            expected: 'ba',
        },
        {
            name: 'named capture replacement',
            rules: [script('(?<left>a)(?<right>b)', '$<right>$<left>')],
            input: 'ab',
            expected: 'ba\n',
        },
        {
            name: 'ECMAScript replacement tokens',
            rules: [script('(abc)', '[$&][$1][$`][$\']')],
            input: 'abc',
            expected: '[abc][abc][][]',
        },
    ])('preserves $name behavior', ({ rules, input, expected }) => {
        const plan = getRegexExecutionPlan(rules, 'editoutput')

        expect(executeRegexPlanSync(plan, input, identity).data).toBe(expected)
    })

    it.each([
        { flag: 'g', input: 'aa', expected: 'xx' },
        { flag: 'y', input: 'a', expected: 'x' },
    ])('resets $flag lastIndex before every execution', ({ flag, input, expected }) => {
        const plan = getRegexExecutionPlan([script('a', 'x', flag)], 'editoutput')

        expect(executeRegexPlanSync(plan, input, identity).data).toBe(expected)
        expect(executeRegexPlanSync(plan, input, identity).data).toBe(expected)
    })

    it('replaces at position 0 when a sticky rule with actions tests first', () => {
        const plan = getRegexExecutionPlan([
            script('foo', 'X', 'y<no_end_nl>'),
        ], 'editoutput')

        expect(executeRegexPlanSync(plan, 'foofoo', identity).data).toBe('Xfoo')
        expect(executeRegexPlanSync(plan, 'foofoo', identity).data).toBe('Xfoo')
    })

    it('isolates an invalid regex and continues with later rules', () => {
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const plan = getRegexExecutionPlan([
            script('[', 'broken'),
            script('a', 'b'),
        ], 'editoutput')

        const result = executeRegexPlanSync(plan, 'a', identity)

        expect(result.data).toBe('b')
        expect(result.errors).toHaveLength(1)
        expect(result.errors[0].sourceIndex).toBe(0)
        errorLog.mockRestore()
    })

    it('reparses CBS patterns for every invocation', () => {
        const plan = getRegexExecutionPlan([
            script('<pattern>', 'b', 'g<cbs>'),
        ], 'editoutput')
        let patternParses = 0
        const parse = (value: string) => {
            if (value === '<pattern>') {
                patternParses++
                return 'a'
            }
            return value
        }

        expect(executeRegexPlanSync(plan, 'a', parse).data).toBe('b')
        expect(executeRegexPlanSync(plan, 'a', parse).data).toBe('b')
        expect(patternParses).toBe(2)
    })

    it('keeps a recently reused plan when the cache evicts', () => {
        const scriptSets = Array.from({ length: 32 }, (_value, index) => [script(`plan-${index}`, 'x')])
        const plans = scriptSets.map((set) => getRegexExecutionPlan(set, 'editoutput'))

        expect(getRegexExecutionPlan(scriptSets[0], 'editoutput')).toBe(plans[0])
        getRegexExecutionPlan([script('plan-extra', 'x')], 'editoutput')

        expect(getRegexExecutionPlan(scriptSets[0], 'editoutput')).toBe(plans[0])
        expect(getRegexExecutionPlan(scriptSets[1], 'editoutput')).not.toBe(plans[1])
    })

    it('rejects stateful actions from worker eligibility', () => {
        const directivePlan = getRegexExecutionPlan([
            script('a', '@@emo happy'),
        ], 'editoutput')
        const actionPlan = getRegexExecutionPlan([
            script('a', 'b', 'g<inject>'),
        ], 'editoutput')
        const orderedReplacementPlan = getRegexExecutionPlan([
            script('a', 'b', 'g<order 1>'),
        ], 'editoutput')

        expect(directivePlan.workerEligible).toBe(false)
        expect(actionPlan.workerEligible).toBe(false)
        expect(orderedReplacementPlan.workerEligible).toBe(true)
    })
})
