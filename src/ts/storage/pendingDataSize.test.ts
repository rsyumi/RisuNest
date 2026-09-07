import { describe, expect, it, vi } from 'vitest'
import { measurePendingDataSize } from './pendingDataSize'

describe('pending save size hint', () => {
    it('stops before traversing the rest of a large database', () => {
        const unrelated = vi.fn(() => {
            throw new Error('must not visit the rest')
        })
        const value = { large: 'x'.repeat(2 * 1024 * 1024) }
        Object.defineProperty(value, 'rest', {
            enumerable: true,
            get: unrelated,
        })
        expect(measurePendingDataSize(() => value)).toBe(1_048_576)
        expect(unrelated).not.toHaveBeenCalled()
    })

    it.each([
        null,
        'small',
        'escaped\n"text',
        { a: 1, b: [true, null, 'text'] },
    ])('keeps the ordinary JSON length for small %j values', (value) => {
        expect(measurePendingDataSize(() => value)).toBe(
            JSON.stringify(value).length,
        )
    })

    it('does not count omitted values as a large serialized payload', () => {
        expect(measurePendingDataSize(() => ({ ignored: undefined }), 4)).toBe(
            2,
        )
    })

    it('keeps failed or cyclic measurements from throwing into the save observer', () => {
        const cyclic: { self?: unknown } = {}
        cyclic.self = cyclic
        expect(measurePendingDataSize(() => cyclic)).toBe(0)
        expect(
            measurePendingDataSize(() => {
                throw new Error('synthetic read failure')
            }),
        ).toBe(0)
        expect(measurePendingDataSize(() => undefined)).toBe(0)
    })
})
