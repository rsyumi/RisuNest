import {
    makeRegexFixture,
    type RegexFixtureSize,
} from '../../src/ts/process/tests/phase1Fixtures'

const expectedHashes: Record<RegexFixtureSize, Record<number, string>> = {
    20: { 32768: 'beb20a50', 262144: '0f11edc3', 1048576: '3b973055' },
    100: { 32768: 'ebc78fd4', 262144: 'db1986d1', 1048576: 'e8acfe45' },
    500: { 32768: '92548f27', 262144: '7093f5ec', 1048576: 'cab988bd' },
}

export function makeBoundedRegexFixture(
    ruleCount: RegexFixtureSize,
    targetBytes: number,
): ReturnType<typeof makeRegexFixture> {
    const expectedHash = expectedHashes[ruleCount]?.[targetBytes]
    if (expectedHash === undefined) {
        throw new RangeError('Regex pilot fixture requires a recorded oracle for this cell')
    }
    const fixture = makeRegexFixture(ruleCount, targetBytes)
    const input = fixture.input.slice(0, targetBytes)
    return {
        ...fixture,
        input,
        expectedHash,
    }
}
