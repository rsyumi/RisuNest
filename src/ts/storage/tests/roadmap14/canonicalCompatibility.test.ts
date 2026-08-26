import { describe, expect, it } from 'vitest'

import {
    ABSENT,
    canonicalBytes,
    canonicalSha256,
    compareSemanticValues,
    toCanonicalValue,
} from './canonicalCompatibility'

describe('roadmap 14 canonical compatibility', () => {
    it('preserves ordered objects, duplicate array values, and distinct empty values', () => {
        const value = {
            second: [false, false, 0, '', {}, []],
            first: undefined,
            nullable: null,
        }

        expect(toCanonicalValue(value)).toEqual({
            tag: 'object',
            entries: [
                [
                    'second',
                    {
                        tag: 'array',
                        items: [
                            { tag: 'boolean', value: false },
                            { tag: 'boolean', value: false },
                            { tag: 'number', value: 0 },
                            { tag: 'string', value: '' },
                            { tag: 'object', entries: [] },
                            { tag: 'array', items: [] },
                        ],
                    },
                ],
                ['first', { tag: 'undefined' }],
                ['nullable', { tag: 'null' }],
            ],
        })
        expect(toCanonicalValue(ABSENT)).toEqual({ tag: 'absent' })
        expect(compareSemanticValues({}, { first: undefined }).equal).toBe(false)
        expect(compareSemanticValues({ first: 1, second: 2 }, { second: 2, first: 1 }))
            .toMatchObject({
                equal: false,
                mismatches: [{ path: '$', kind: 'object-key-order' }],
            })
    })

    it('uses a deterministic length-delimited byte encoding with distinct scalar tags', () => {
        expect(Buffer.from(canonicalBytes({ a: 0 })).toString('hex')).toBe(
            '4f0000000100000001610000000d44000000080000000000000000',
        )
        const distinctValues = [ABSENT, undefined, null, false, 0, '', {}, []]
        expect(new Set(distinctValues.map((value) => canonicalSha256(value))).size).toBe(
            distinctValues.length,
        )
        expect(canonicalSha256(['duplicate', 'duplicate'])).toBe(
            canonicalSha256(['duplicate', 'duplicate']),
        )
    })
})
