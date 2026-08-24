import { describe, expect, it } from 'vitest'
import { SyncConflictBackupStore, type SyncConflictBackupKv } from './syncConflictBackup'

function memoryKv(): SyncConflictBackupKv & { values: Map<string, unknown> } {
    const values = new Map<string, unknown>()
    return {
        values,
        getItem: async (key) => values.get(key) ?? null,
        setItem: async (key, value) => {
            values.set(key, value)
            return value
        },
        removeItem: async (key) => {
            values.delete(key)
        },
    }
}

function sequenceClock(start = 1_000): () => number {
    let current = start
    return () => current++
}

describe('SyncConflictBackupStore', () => {
    it('saves entries and lists them newest first', async () => {
        const store = new SyncConflictBackupStore(memoryKv(), sequenceClock())

        await store.save({ side: 'remote', bytes: Uint8Array.of(1, 2), characterCount: 3 })
        await store.save({ side: 'local', bytes: Uint8Array.of(3), characterCount: 5 })

        const entries = await store.list()
        expect(entries).toHaveLength(2)
        expect(entries[0]).toMatchObject({ side: 'local', characterCount: 5, byteLength: 1 })
        expect(entries[1]).toMatchObject({ side: 'remote', characterCount: 3, byteLength: 2 })
        expect(entries[0].createdAt).toBeGreaterThan(entries[1].createdAt)
    })

    it('reads a saved payload back by id and returns null for unknown ids', async () => {
        const store = new SyncConflictBackupStore(memoryKv(), sequenceClock())
        const entry = await store.save({
            side: 'remote',
            bytes: Uint8Array.of(7, 8, 9),
            characterCount: 1,
        })

        expect(await store.read(entry.id)).toEqual(Uint8Array.of(7, 8, 9))
        expect(await store.read('missing')).toBeNull()
    })

    it('normalizes payloads a storage driver deserialized as ArrayBuffer', async () => {
        const kv = memoryKv()
        const store = new SyncConflictBackupStore(kv, sequenceClock())
        const entry = await store.save({ side: 'local', bytes: Uint8Array.of(4, 5), characterCount: 2 })
        kv.values.set(`payload:${entry.id}`, Uint8Array.of(4, 5).buffer)

        expect(await store.read(entry.id)).toEqual(Uint8Array.of(4, 5))
    })

    it('prunes to the retention limit and deletes the dropped payloads', async () => {
        const kv = memoryKv()
        const store = new SyncConflictBackupStore(kv, sequenceClock())
        const first = await store.save({ side: 'local', bytes: Uint8Array.of(0), characterCount: 0 })
        for (let index = 0; index < 5; index++) {
            await store.save({ side: 'remote', bytes: Uint8Array.of(index), characterCount: index })
        }

        const entries = await store.list()
        expect(entries).toHaveLength(5)
        expect(entries.some((entry) => entry.id === first.id)).toBe(false)
        expect(await store.read(first.id)).toBeNull()
        expect(kv.values.has(`payload:${first.id}`)).toBe(false)
    })

    it('removes an entry together with its payload', async () => {
        const store = new SyncConflictBackupStore(memoryKv(), sequenceClock())
        const entry = await store.save({ side: 'local', bytes: Uint8Array.of(1), characterCount: 1 })
        const kept = await store.save({ side: 'remote', bytes: Uint8Array.of(2), characterCount: 2 })

        await store.remove(entry.id)

        expect((await store.list()).map((value) => value.id)).toEqual([kept.id])
        expect(await store.read(entry.id)).toBeNull()
    })

    it('treats a corrupted index as empty', async () => {
        const kv = memoryKv()
        kv.values.set('index', { not: 'an array' })
        const store = new SyncConflictBackupStore(kv, sequenceClock())

        expect(await store.list()).toEqual([])

        kv.values.set('index', [{ id: 'x' }, null, 'garbage'])
        expect(await store.list()).toEqual([])
    })
})
