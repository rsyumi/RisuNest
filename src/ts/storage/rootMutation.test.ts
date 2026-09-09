import { describe, expect, it } from 'vitest'
import type { PersistentRoot } from './persistentDataStore'
import { applyRootMutations, diffRootMutations } from './rootMutation'

const root = (value: Record<string, unknown>) => value as PersistentRoot

describe('persistent root mutations', () => {
    it('sends only changed fields and distinguishes null from deletion', () => {
        const before = root({
            username: 'Before',
            large: 'x'.repeat(6 * 1024 * 1024),
            remove: true,
        })
        const after = root({ username: 'After', large: before['large'], nullable: null })
        const mutations = diffRootMutations(before, after)
        expect(mutations).toEqual([
            { type: 'set', key: 'nullable', value: null },
            { type: 'delete', key: 'remove' },
            { type: 'set', key: 'username', value: 'After' },
        ])
        expect(JSON.stringify(mutations).length).toBeLessThan(256)
        expect(applyRootMutations(before, mutations)).toEqual(after)
        expect(before.username).toBe('Before')
    })

    it('preserves own special keys and detaches inserted values', () => {
        const value = { nested: [1] }
        const result = applyRootMutations(root({}), [
            { type: 'set', key: '__proto__', value },
            { type: 'set', key: 'constructor', value: null },
        ])
        value.nested.push(2)
        expect(Object.getPrototypeOf(result)).toBe(Object.prototype)
        expect(Object.hasOwn(result, '__proto__')).toBe(true)
        expect(result['__proto__']).toEqual({ nested: [1] })
        expect(result.constructor).toBe(null)
    })

    it('normalizes object ordering and detects nested changes', () => {
        expect(
            diffRootMutations(root({ custom: { a: 1, b: 2 } }), root({ custom: { b: 2, a: 1 } })),
        ).toEqual([])
        expect(diffRootMutations(root({ custom: [1, 2] }), root({ custom: [2, 1] }))).toEqual([
            { type: 'set', key: 'custom', value: [2, 1] },
        ])
    })

    it.each(['characters', 'botPresets', 'pluginCustomStorage'])(
        'rejects the separately owned %s area',
        (key) => {
            expect(() => applyRootMutations(root({}), [{ type: 'delete', key }])).toThrow(
                /root mutation/i,
            )
        },
    )

    it('rejects duplicate keys and values missing from set operations', () => {
        expect(() =>
            applyRootMutations(root({}), [
                { type: 'set', key: 'username', value: 'After' },
                { type: 'delete', key: 'username' },
            ]),
        ).toThrow(/duplicate/i)
        expect(() =>
            applyRootMutations(root({}), [{ type: 'set', key: 'username', value: undefined }]),
        ).toThrow(/value/i)
    })
})
