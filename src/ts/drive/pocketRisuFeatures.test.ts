import { describe, expect, it } from 'vitest'
import fixtures from './fixtures/pocket-features.json'
import chatFixtures from './fixtures/pocket-chat-features.json'
import {
    normalizePocketChat,
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
            normalizePocketMessage(input, 'fallback')
            expect(input).toEqual(fixture.expected)
        },
    )
    it.each(chatFixtures.map((fixture, index) => [index, fixture] as const))(
        'matches the shared native chat fixture %i',
        (_, fixture) => {
            const input = structuredClone(fixture.input)
            normalizePocketChat(input, 'fallback', { risuNestSave: 'risuNestSave' in fixture && fixture.risuNestSave })
            expect(input).toEqual(fixture.expected)
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
    })
    it('drops PocketRisu values RisuNest cannot read instead of rejecting the backup', () => {
        const db = normalizePocketFeatures({
            disableToggleBinding: null,
            defaultToggleValues: null,
            togglePresets: [
                { name: 'Imported', values: { toggle_a: '1', custom: '2' } },
                { values: {} },
                { name: 'No values' },
            ],
            personas: [{ id: 'same' }, { id: 'same' }, { id: 3 }],
            characters: [{ chats: [
                { savedToggleValues: { toggle_kept: '1', other: 'x', toggle_number: 2 }, bindedPersona: null, message: [] },
                { savedToggleValues: null, message: [] },
            ] }],
        })
        expect(db).not.toHaveProperty('disableToggleBinding')
        expect(db).not.toHaveProperty('defaultToggleValues')
        expect(db.togglePresets).toEqual([
            { name: 'Imported', values: { toggle_a: '1' } },
            { name: 'No values', values: {} },
        ])
        const ids = db.personas.map((persona) => persona.id)
        expect(ids[0]).toBe('same')
        expect(ids.every((id) => typeof id === 'string')).toBe(true)
        expect(new Set(ids).size).toBe(3)
        expect(db.characters[0].chats[0]).toEqual({ savedToggleValues: { toggle_kept: '1' }, message: [] })
        expect(db.characters[0].chats[1]).toEqual({ message: [] })
        expect(normalizePocketFeatures({ togglePresets: null })).toEqual({})
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
