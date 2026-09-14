import { describe, expect, it } from 'vitest'

import { fnv1a } from '../../src/ts/process/tests/phase1Fixtures'
import { makeBoundedRegexFixture } from './fixture'
import { executeRegexPlanSync, getRegexExecutionPlan } from '../../src/ts/process/regexExecutionPlan'

describe('regex native pilot fixtures', () => {
    it('keeps the 1 MiB cell exactly within the classifier byte bound', () => {
        const fixture = makeBoundedRegexFixture(500, 1_048_576)

        expect(new TextEncoder().encode(fixture.input)).toHaveLength(1_048_576)
        expect(fixture.expectedHash).toBe('cab988bd')
    })

    it.each([20, 100, 500] as const)('pins actual JS output for all %i-rule cells', (rules) => {
        for (const size of [32768, 262144, 1048576]) {
            const fixture = makeBoundedRegexFixture(rules, size)
            const result = executeRegexPlanSync(getRegexExecutionPlan(fixture.scripts, 'editoutput'), fixture.input, (value) => value)
            expect(fnv1a(result.data)).toBe(fixture.expectedHash)
            expect(result.errors).toEqual([])
        }
    })

    it('rejects an unrecorded cell instead of deriving its own expected hash', () => {
        expect(() => makeBoundedRegexFixture(500, 1024)).toThrow(RangeError)
    })
})
