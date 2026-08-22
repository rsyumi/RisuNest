import { describe, expect, it } from 'vitest'
import { createLocalColdStorageRuntime } from './localColdStorageRuntime'

describe('local cold storage runtime', () => {
    it('encodes and decodes local payloads through one rooted byte store', async () => {
        const values = new Map<string, Uint8Array>()
        const runtime = createLocalColdStorageRuntime({
            read: async (key) => values.get(key)?.slice() ?? null,
            write: async (key, data) => void values.set(key, data.slice()),
            list: async () => [...values.keys()].sort(),
            remove: async (key) => void values.delete(key),
        })
        const value = { message: [{ role: 'char', data: 'cold' }] }

        expect(await runtime.write('chat', value)).toBe(true)
        expect(await runtime.read('chat')).toEqual(value)
        expect(await runtime.list()).toEqual(['chat'])
        await runtime.remove(['chat'])
        expect(await runtime.read('chat')).toBeNull()
    })

    it('reports invalid local payloads without writing bytes', async () => {
        let writes = 0
        const runtime = createLocalColdStorageRuntime({
            read: async () => null,
            write: async () => { writes += 1 },
            list: async () => [],
            remove: async () => undefined,
        })

        expect(await runtime.write('bad', { unrelated: true })).toBe(false)
        expect(writes).toBe(0)
    })
})
