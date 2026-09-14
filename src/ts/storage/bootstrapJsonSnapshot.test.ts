import { describe, expect, it, vi } from 'vitest'
import {
    bootstrapJsonSnapshot,
    cloneBootstrapSnapshot,
    equalBootstrapSnapshots,
} from './bootstrapJsonSnapshot'
import { canonicalJson } from './saveCoordinatorHelpers'

describe('bootstrap JSON snapshots', () => {
    it('matches canonical JSON for nested JSON values and omitted or non-finite entries', () => {
        const values: unknown[] = [
            null,
            true,
            -0,
            NaN,
            Infinity,
            '\ud800🙂\\"',
            { z: [undefined, , NaN, Symbol('omitted')], a: undefined, b: { y: 2, x: null } },
            JSON.parse('{"__proto__":{"polluted":true},"constructor":"data","10":10,"2":2}'),
            { date: new Date(0), boxed: new Number(4), empty: {} },
        ]
        for (const value of values) {
            const snapshot = bootstrapJsonSnapshot(value)
            expect(JSON.stringify(snapshot)).toBe(canonicalJson(value))
            expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(value))).toBe(true)
        }
        expect({}).not.toHaveProperty('polluted')
    })

    it('keeps callable JSON hook output and property ordering identical to serialization', () => {
        const input = {
            z: {
                toJSON(key: string) {
                    return { z: key, a: 1 }
                },
            },
            omitted: () => 1,
        }
        const before = canonicalJson(input)
        const snapshot = bootstrapJsonSnapshot(input)
        expect(JSON.stringify(snapshot)).toBe(before)
        const detached = cloneBootstrapSnapshot<typeof input>(snapshot)
        expect(JSON.stringify(detached)).toBe(before)
        expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(detached))).toBe(
            canonicalJson(detached) === before,
        )
    })

    it('detaches every mutable container and detects equal-size nested and ordering changes', () => {
        const input = { z: { items: [{ text: 'first' }, { text: 'other' }] }, a: 'immutable' }
        const snapshot = bootstrapJsonSnapshot(input)
        const detached = cloneBootstrapSnapshot<typeof input>(snapshot)
        expect(detached).not.toBe(input)
        expect(detached.z.items[0]).not.toBe(input.z.items[0])
        detached.z.items[0].text = 'FIRST'
        expect(input.z.items[0].text).toBe('first')
        expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(detached))).toBe(false)
        detached.z.items[0].text = 'first'
        detached.z.items.reverse()
        expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(detached))).toBe(false)
        detached.z.items.reverse()
        expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(detached))).toBe(true)
        input.z.items[0].text = 'later'
        expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(detached))).toBe(true)
    })

    it('copies and compares large JSON strings without serializing the database', () => {
        const input = { plugin: 'x'.repeat(16 * 1024 * 1024), characters: [{ chats: [null] }] }
        const stringify = vi.spyOn(JSON, 'stringify')
        try {
            const snapshot = bootstrapJsonSnapshot(input)
            const detached = cloneBootstrapSnapshot<typeof input>(snapshot)
            expect(equalBootstrapSnapshots(snapshot, bootstrapJsonSnapshot(detached))).toBe(true)
            expect(stringify).not.toHaveBeenCalled()
            expect(detached.plugin.length).toBe(16 * 1024 * 1024)
        } finally {
            stringify.mockRestore()
        }
    })

    it('rejects unrepresentable values without modifying them and permits repeated references', () => {
        expect(() => bootstrapJsonSnapshot({ value: 1n })).toThrow()
        const cyclic: { self?: unknown } = {}
        cyclic.self = cyclic
        expect(() => bootstrapJsonSnapshot(cyclic)).toThrow('Circular bootstrap database')
        expect(cyclic.self).toBe(cyclic)
        const shared = { value: 3 }
        expect(bootstrapJsonSnapshot([shared, shared])).toEqual([shared, shared])
    })
})
