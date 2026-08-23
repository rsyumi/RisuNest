import { describe, expect, it } from 'vitest'
import { ByteBudgetLru } from '../byteBudgetLru'

function createCache(maxBytes: number) {
    return new ByteBudgetLru<string, string>(maxBytes, (_key, value) => value.length)
}

describe('ByteBudgetLru', () => {
    it('accounts for retained bytes', () => {
        const cache = createCache(10)

        expect(cache.set('one', 'abc')).toBe(true)
        expect(cache.set('two', 'de')).toBe(true)
        expect(cache.size).toBe(2)
        expect(cache.sizeBytes).toBe(5)
    })

    it('evicts the oldest entry when the byte budget is exceeded', () => {
        const cache = createCache(4)
        cache.set('old', 'ab')
        cache.set('new', 'cde')

        expect(cache.get('old')).toBeUndefined()
        expect(cache.get('new')).toBe('cde')
        expect(cache.sizeBytes).toBe(3)
    })

    it('promotes a retrieved entry so a less recent entry is evicted', () => {
        const cache = createCache(4)
        cache.set('first', 'ab')
        cache.set('second', 'cd')
        expect(cache.get('first')).toBe('ab')

        cache.set('third', 'ef')

        expect(cache.get('first')).toBe('ab')
        expect(cache.get('second')).toBeUndefined()
        expect(cache.get('third')).toBe('ef')
    })

    it('updates byte accounting when replacing an entry', () => {
        const cache = createCache(5)
        cache.set('item', 'a')
        cache.set('item', 'abcd')

        expect(cache.size).toBe(1)
        expect(cache.sizeBytes).toBe(4)
        expect(cache.get('item')).toBe('abcd')
    })

    it('does not retain entries with a zero budget', () => {
        const cache = createCache(0)

        expect(cache.set('item', 'a')).toBe(false)
        expect(cache.set('empty', '')).toBe(false)
        expect(cache.size).toBe(0)
        expect(cache.sizeBytes).toBe(0)
    })

    it('does not retain oversized entries or evict smaller retained entries for them', () => {
        const cache = createCache(4)
        cache.set('small', 'ab')

        expect(cache.set('large', 'abcde')).toBe(false)
        expect(cache.get('small')).toBe('ab')
        expect(cache.get('large')).toBeUndefined()
        expect(cache.sizeBytes).toBe(2)
    })

    it('drops the previous value when a same-key replace is oversized', () => {
        const cache = createCache(4)
        cache.set('item', 'ab')

        expect(cache.set('item', 'abcde')).toBe(false)
        expect(cache.get('item')).toBeUndefined()
        expect(cache.size).toBe(0)
        expect(cache.sizeBytes).toBe(0)
    })

    it('enforces an optional entry limit using LRU recency', () => {
        const cache = new ByteBudgetLru<string, string>(
            100,
            (_key, value) => value.length,
            2,
        )
        cache.set('first', 'aa')
        cache.set('second', 'bb')
        expect(cache.get('first')).toBe('aa')

        cache.set('third', 'cc')

        expect(cache.get('first')).toBe('aa')
        expect(cache.get('second')).toBeUndefined()
        expect(cache.get('third')).toBe('cc')
        expect(cache.size).toBe(2)
        expect(cache.sizeBytes).toBe(4)
    })

    it('deletes an entry and releases its bytes', () => {
        const cache = new ByteBudgetLru<string, string>(100, (_key, value) => value.length)
        cache.set('a', 'aaaa')
        expect(cache.delete('a')).toBe(true)
        expect(cache.get('a')).toBeUndefined()
        expect(cache.size).toBe(0)
        expect(cache.sizeBytes).toBe(0)
        expect(cache.delete('a')).toBe(false)
    })
})
