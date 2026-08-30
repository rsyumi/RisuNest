import { describe, expect, it } from 'vitest'

import golden from '../tests/fixtures/logicalRecordKeyV1Golden.json'
import {
    decodeLogicalRecordKey,
    encodeLogicalRecordKey,
    type LogicalRecordLocator,
} from './logicalRecordKey'

describe('logical record key codec', () => {
    it.each(golden.roundTrip)(
        'round-trips $encoded using one canonical encoded form',
        ({ locator, encoded }) => {
            expect(encodeLogicalRecordKey(locator as LogicalRecordLocator)).toBe(encoded)
            expect(decodeLogicalRecordKey(encoded)).toEqual(locator)
        },
    )

    it.each(golden.rejectedEncoded)('rejects invalid or noncanonical input %s', (encoded) => {
        expect(() => decodeLogicalRecordKey(encoded)).toThrow(TypeError)
    })

    it.each(golden.rejectedLocators)('rejects invalid locator %o', (locator) => {
        expect(() => encodeLogicalRecordKey(locator as LogicalRecordLocator)).toThrow(TypeError)
    })
})
