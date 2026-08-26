import { describe, expect, it } from 'vitest'

import { fnv1a } from '../../src/ts/process/tests/phase1Fixtures'
import { makeBoundedRegexFixture } from './fixture'

describe('regex native pilot fixtures', () => {
    it('keeps the 1 MiB cell exactly within the classifier byte bound', () => {
        const fixture = makeBoundedRegexFixture(500, 1_048_576)

        expect(new TextEncoder().encode(fixture.input)).toHaveLength(1_048_576)
        expect(fnv1a(fixture.input.replaceAll('rule-', 'done-'))).toBe(fixture.expectedHash)
    })
})
