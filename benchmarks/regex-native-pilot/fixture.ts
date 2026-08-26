import {
    fnv1a,
    makeRegexFixture,
    type RegexFixtureSize,
} from '../../src/ts/process/tests/phase1Fixtures'

export function makeBoundedRegexFixture(
    ruleCount: RegexFixtureSize,
    targetBytes: number,
): ReturnType<typeof makeRegexFixture> {
    const fixture = makeRegexFixture(ruleCount, targetBytes)
    const input = fixture.input.slice(0, targetBytes)
    return {
        ...fixture,
        input,
        expectedHash: fnv1a(input.replaceAll('rule-', 'done-')),
    }
}
