import { describe, expect, it } from 'vitest'
import { compareSemver } from './manifest'

describe('app update versions', () => {
    it('compares all numeric SemVer components', () => {
        expect(compareSemver('2026.10.1', '2026.9.99')).toBe(1)
        expect(compareSemver('2026.8.250', '2026.8.250')).toBe(0)
        expect(compareSemver('1.2.3', '1.3.0')).toBe(-1)
    })

    it('rejects versions outside the stable product contract', () => {
        for (const value of ['01.2.3', '1.2', '1.2.3-beta', '1.2.3+build']) {
            expect(() => compareSemver(value, '1.2.3')).toThrow()
        }
    })
})
