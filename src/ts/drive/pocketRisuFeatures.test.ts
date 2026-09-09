import { describe, expect, it } from 'vitest'
import fixtures from './fixtures/pocket-features.json'
import {
    normalizePocketFeatures,
    normalizePocketMessage,
    normalizePocketColdPayload,
} from './pocketRisuFeatures'
import { encodeColdStoragePayload, decodeColdStoragePayload } from '../process/coldstorageData'

describe('Pocket feature import', () => {
    it.each(fixtures.map((fixture, index) => [index, fixture] as const))(
        'matches the shared native fixture %i',
        (_, fixture) => {
            const input = structuredClone(fixture.input)
            if ('invalid' in fixture) expect(() => normalizePocketMessage(input, 'fallback')).toThrow()
            else {
                normalizePocketMessage(input, 'fallback')
                expect(input).toEqual(fixture.expected)
            }
        },
    )
    it('preserves toggle snapshots, persona IDs and excluded opaque fields without interpreting them', () => {
        const db = {
            personas: [{ id: 'persona', name: 'Synthetic' }],
            togglePresets: [
                { name: 'same', values: {} },
                { name: 'same', values: { toggle_a: '0' } },
            ],
            defaultToggleValues: {},
            disableToggleBinding: false,
            modelPresets: [{ opaque: true }],
            characters: [{ chats: [{ savedToggleValues: {}, bindedPersona: 'missing', message: [] }] }],
        }
        expect(normalizePocketFeatures(structuredClone(db))).toEqual(db)
        expect(() => normalizePocketFeatures({ personas: [{ id: 'same' }, { id: 'same' }] })).toThrow()
        expect(() => normalizePocketFeatures({ defaultToggleValues: { other: '1' } })).toThrow()
        expect(() =>
            normalizePocketFeatures({ characters: [{ chats: [{ savedToggleValues: { toggle_a: 1 } }] }] }),
        ).toThrow()
    })
    it('normalizes cold payloads on decode without changing the input backup', async () => {
        const original = {
            message: [fixtures[0].input],
            savedToggleValues: { toggle_a: '0' },
            bindedPersona: 'persona',
        }
        const encoded = await encodeColdStoragePayload(original)
        const result = await decodeColdStoragePayload(encoded)
        expect(result).toEqual({ ...original, message: [fixtures[0].expected] })
        expect(original.message[0]).not.toHaveProperty('responseVariants')
        expect(normalizePocketColdPayload([structuredClone(fixtures[0].input)])[0]).toEqual(
            fixtures[0].expected,
        )
    })
})
