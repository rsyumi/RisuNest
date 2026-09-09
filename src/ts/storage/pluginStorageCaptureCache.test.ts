import { afterEach, describe, expect, it, vi } from 'vitest'
import {
    PluginStorageBaseline,
    PluginStorageCaptureCache,
    pluginStorageJson,
} from './saveCoordinatorHelpers'

afterEach(() => vi.restoreAllMocks())

describe('PluginStorageCaptureCache', () => {
    it('does not re-encode large unchanged primitive entries or parse until requested', () => {
        const storage = { payload: 'x'.repeat(1024 * 1024), count: 1 }
        const cache = new PluginStorageCaptureCache()
        const stringify = vi.spyOn(JSON, 'stringify')
        const parse = vi.spyOn(JSON, 'parse')
        const initial = cache.capture(storage)
        const initialCalls = stringify.mock.calls.length
        expect(initialCalls).toBeGreaterThan(0)
        for (let index = 0; index < 10; index++) {
            expect(cache.capture(storage)).toBe(initial)
        }
        expect(stringify).toHaveBeenCalledTimes(initialCalls)
        expect(parse).not.toHaveBeenCalled()
        storage.count = 2
        const changed = cache.capture(storage)
        expect(changed === initial).toBe(false)
        expect(stringify).toHaveBeenCalledTimes(initialCalls + 1)
        expect(stringify.mock.calls.filter(([value]) => value === storage.payload)).toHaveLength(1)
        storage.count = 1
        expect(initial.json).toBe(pluginStorageJson(storage))
        expect(parse.mock.calls.length).toBe(0)
        const detached = initial.value
        expect(parse.mock.calls.length).toBe(1)
        expect(detached).toEqual(storage)
    })

    it('observes same-reference plain nested mutations and owns the captured bytes', () => {
        const raw = { nested: { count: 1 }, items: [1, 2] }
        const storage = { raw }
        const cache = new PluginStorageCaptureCache()
        const initial = cache.capture(storage)
        raw.nested.count = 2
        raw.items.push(3)
        const changed = cache.capture(storage)
        expect(changed).not.toBe(initial)
        expect(changed.json).toBe(pluginStorageJson(storage))
        expect(initial.value).toEqual({ raw: { nested: { count: 1 }, items: [1, 2] } })
        expect(cache.capture(storage)).toBe(changed)
    })

    it('never exposes a mutable cached snapshot to later callers', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = { nested: { count: 1 } }
        const initial = cache.capture(storage)
        initial.value.nested.count = 99
        expect(cache.capture(storage)).toBe(initial)
        expect(initial.value).toEqual(storage)
        expect(initial.json).toBe('{"nested":{"count":1}}')
    })

    it('preserves own dangerous keys, numeric and insertion order, nested sorting and undefined', () => {
        const cache = new PluginStorageCaptureCache()
        const storage: Record<string, any> = JSON.parse(
            '{"z":{"z":2,"a":1},"10":"ten","2":"two","__proto__":{"value":1},"a":3}',
        )
        storage.absent = undefined
        const initial = cache.capture(storage)
        expect(initial.json).toBe(pluginStorageJson(storage))
        expect(initial.json).toBe(
            '{"2":"two","10":"ten","z":{"a":1,"z":2},"__proto__":{"value":1},"a":3}',
        )
        expect(Object.hasOwn(initial.value, '__proto__')).toBe(true)
        delete storage.absent
        expect(cache.capture(storage)).toBe(initial)
        delete storage.z
        storage.z = { a: 1, z: 2 }
        const reordered = cache.capture(storage)
        expect(reordered).not.toBe(initial)
        expect(reordered.json).toBe(pluginStorageJson(storage))
    })

    it('rereads getters and only reuses encodings for unchanged immutable values', () => {
        const cache = new PluginStorageCaptureCache()
        let current = 'first'
        const getter = vi.fn(() => current)
        const storage = {
            get entry() {
                return getter()
            },
        }
        const initial = cache.capture(storage)
        expect(cache.capture(storage)).toBe(initial)
        expect(getter).toHaveBeenCalledTimes(2)
        current = 'second'
        expect(cache.capture(storage).json).toBe('{"entry":"second"}')
        expect(initial.json).toBe('{"entry":"first"}')
    })

    it('uses serialized equality for NaN, infinities and signed zero', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = { number: NaN, zero: 0 }
        const initial = cache.capture(storage)
        storage.number = Infinity
        storage.zero = -0
        expect(cache.capture(storage)).toBe(initial)
        expect(initial.json).toBe('{"number":null,"zero":0}')
    })

    it('preserves callable toJSON key arguments and full-object evaluation ordering', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = {
            a: {
                toJSON(key: string) {
                    return key
                },
            },
            b: { x: 1 },
        }
        expect(cache.capture(storage).json).toBe(pluginStorageJson(storage))
        expect(cache.capture(storage).json).toBe('{"a":"a","b":{"x":1}}')
        const root = {
            toJSON() {
                return { replaced: true }
            },
        }
        expect(cache.capture(root).json).toBe(pluginStorageJson(root))
        expect(cache.capture({}).json).toBe('{}')
    })

    it('normalizes all entries before invoking serialization hooks', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = {
            a: {
                toJSON() {
                    storage.b.x = 2
                    return 'hook'
                },
            },
            b: { x: 1 },
        }
        const captured = cache.capture(storage)
        expect(captured.json).toBe('{"a":"hook","b":{"x":1}}')
        expect(storage.b.x).toBe(2)
        expect(cache.capture(storage).json).toBe('{"a":"hook","b":{"x":2}}')
    })

    it('does not reuse removed entries or retain a stale result after a failed capture', () => {
        const cache = new PluginStorageCaptureCache()
        const storage: Record<string, any> = { entry: 'old' }
        const initial = cache.capture(storage)
        delete storage.entry
        expect(cache.capture(storage).json).toBe('{}')
        storage.entry = 'new'
        expect(cache.capture(storage).json).toBe('{"entry":"new"}')
        storage.invalid = 1n
        expect(() => cache.capture(storage)).toThrow()
        delete storage.invalid
        expect(cache.capture(storage).json).toBe('{"entry":"new"}')
        expect(initial.json).toBe('{"entry":"old"}')
    })

    it('clears retained captures and encodings without invalidating owned snapshots', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = { payload: 'immutable' }
        const initial = cache.capture(storage)
        cache.clear()
        const stringify = vi.spyOn(JSON, 'stringify')
        const next = cache.capture(storage)
        expect(next).not.toBe(initial)
        expect(stringify).toHaveBeenCalledWith('immutable')
        expect(initial.json).toBe('{"payload":"immutable"}')
        expect(next.json).toBe(initial.json)
    })

    it('exposes only frozen owned entry tuples', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = { z: { b: 2, a: 1 }, a: 'value' }
        const capture = cache.capture(storage)
        expect(capture.entries).toEqual([
            ['z', '{"a":1,"b":2}'],
            ['a', '"value"'],
        ])
        expect(Object.isFrozen(capture)).toBe(true)
        expect(Object.isFrozen(capture.entries)).toBe(true)
        expect(capture.entries!.every(Object.isFrozen)).toBe(true)
        storage.z.a = 3
        expect(capture.entries![0][1]).toBe('{"a":1,"b":2}')
    })

    it('matches ten small mutations without full JSON assembly or parsing', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = { payload: 'x'.repeat(1024 * 1024), count: 0 }
        const baseline = new PluginStorageBaseline(pluginStorageJson(storage))
        expect(baseline.matches(cache.capture(storage))).toBe(true)
        const baselineJson = vi.spyOn(PluginStorageBaseline.prototype, 'json', 'get')
        const parse = vi.spyOn(JSON, 'parse')
        const join = vi.spyOn(Array.prototype, 'join')
        const matches: boolean[] = []
        for (let count = 1; count <= 10; count++) {
            storage.count = count
            baseline.apply([{ type: 'set', key: 'count', value: count }])
            const capture = cache.capture(storage)
            matches.push(
                baseline.matches({
                    entries: capture.entries,
                    get json(): string {
                        throw new Error('Full JSON must stay lazy')
                    },
                    get value(): never {
                        throw new Error('Parsing must stay lazy')
                    },
                }),
            )
        }
        // Collect counts before matchers themselves have a chance to use JSON.
        const parseCalls = parse.mock.calls.length
        const joinCalls = join.mock.calls.length
        expect(matches).toEqual(Array(10).fill(true))
        expect(parseCalls).toBe(0)
        expect(joinCalls).toBe(0)
        expect(baselineJson).not.toHaveBeenCalled()
    })

    it('rejects raw nested edits and order changes while matching numeric-key order', () => {
        const cache = new PluginStorageCaptureCache()
        const storage: Record<string, any> = { z: { count: 1 }, a: 2, 10: 'ten', 2: 'two' }
        const baseline = new PluginStorageBaseline(pluginStorageJson(storage))
        expect(baseline.matches(cache.capture(storage))).toBe(true)
        storage.z.count = 2
        expect(baseline.matches(cache.capture(storage))).toBe(false)
        storage.z.count = 1
        expect(baseline.matches(cache.capture(storage))).toBe(true)
        delete storage.z
        storage.z = { count: 1 }
        expect(baseline.matches(cache.capture(storage))).toBe(false)
        baseline.apply([
            { type: 'delete', key: 'z' },
            { type: 'set', key: 'z', value: { count: 1 } },
        ])
        expect(baseline.matches(cache.capture(storage))).toBe(true)
        baseline.apply([{ type: 'set', key: '1', value: 'one' }])
        storage['1'] = 'one'
        expect(baseline.matches(cache.capture(storage))).toBe(true)
    })

    it('compares immutable prior captures against the baseline at call time', () => {
        const cache = new PluginStorageCaptureCache()
        const storage = { count: 1 }
        const original = cache.capture(storage)
        const baseline = new PluginStorageBaseline(original.json)
        expect(baseline.matches(original)).toBe(true)
        baseline.apply([{ type: 'set', key: 'count', value: 2 }])
        expect(baseline.matches(original)).toBe(false)
        storage.count = 2
        const changed = cache.capture(storage)
        expect(baseline.matches(changed)).toBe(true)
        baseline.apply([{ type: 'set', key: 'count', value: 1 }])
        expect(baseline.matches(changed)).toBe(false)
        expect(baseline.matches(original)).toBe(true)
        expect(baseline.json).toBe(original.json)
    })

    it('uses conservative whole-object equality for callable captures', () => {
        const cache = new PluginStorageCaptureCache()
        let count = 1
        const storage = {
            toJSON() {
                return { count }
            },
        }
        const capture = cache.capture(storage)
        const baseline = new PluginStorageBaseline(capture.json)
        expect(capture.entries).toBe(null)
        expect(baseline.matches(capture)).toBe(true)
        count = 2
        expect(baseline.matches(cache.capture(storage))).toBe(false)
        const data = cache.capture({ count })
        expect(data.entries).toEqual([['count', '2']])
        expect(cache.capture(storage).entries).toBe(null)
    })
})
