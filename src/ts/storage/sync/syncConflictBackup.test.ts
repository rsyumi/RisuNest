import { beforeEach, describe, expect, it, vi } from 'vitest'

const platformMocks = vi.hoisted(() => {
    const settingValues = new Map<string, unknown>()
    const blobValues = new Map<string, Uint8Array>()
    return {
        isTauri: false,
        settingValues,
        blobValues,
        localforage: {
            createInstance: vi.fn(() => {
                throw new Error('LocalForage must not be opened for native conflict backups')
            }),
        },
        deviceSettings: {
            get: vi.fn(async (key: string) => settingValues.get(key) ?? null),
            set: vi.fn(async (key: string, value: unknown) => {
                if (value === null) settingValues.delete(key)
                else settingValues.set(key, value)
            }),
            patch: vi.fn(async () => {
                throw new Error('The conflict index is stored as a whole value')
            }),
        },
        blobStore: {
            put: vi.fn(async (key: string, bytes: Uint8Array) => {
                blobValues.set(key, bytes.slice())
                return {
                    key,
                    kind: 'asset' as const,
                    size: bytes.byteLength,
                    mime: 'application/octet-stream',
                    name: key,
                    ext: 'risudat',
                }
            }),
            read: vi.fn(async (key: string) => blobValues.get(key)?.slice() ?? null),
            remove: vi.fn(async (key: string) => {
                blobValues.delete(key)
            }),
        },
    }
})

vi.mock('localforage', () => ({ default: platformMocks.localforage }))
vi.mock('../../platform', () => ({
    get isTauri() { return platformMocks.isTauri },
}))
vi.mock('../nativeDeviceSettings', () => ({
    createNativeDeviceSettings: () => platformMocks.deviceSettings,
}))
vi.mock('../platformBlobStore', () => ({ getBlobStore: () => platformMocks.blobStore }))

import {
    getSyncConflictBackupStore,
    SyncConflictBackupStore,
    type SyncConflictBackupKv,
    type SyncConflictBackupPayloadStore,
} from './syncConflictBackup'

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

function memoryPayloadStore(): SyncConflictBackupPayloadStore & {
    values: Map<string, Uint8Array>
} {
    const values = new Map<string, Uint8Array>()
    return {
        values,
        write: async (id, bytes) => {
            values.set(id, bytes.slice())
        },
        read: async (id) => values.get(id)?.slice() ?? null,
        remove: async (id) => {
            values.delete(id)
        },
    }
}

describe('SyncConflictBackupStore', () => {
    beforeEach(() => {
        platformMocks.isTauri = false
        platformMocks.settingValues.clear()
        platformMocks.blobValues.clear()
        vi.clearAllMocks()
    })

    it('saves entries and lists them newest first', async () => {
        const store = new SyncConflictBackupStore(memoryKv(), sequenceClock())

        await store.save({ side: 'remote', bytes: Uint8Array.of(1, 2), characterCount: 3 })
        await store.save({ side: 'local', bytes: Uint8Array.of(3), characterCount: 5 })

        const entries = await store.list()
        expect(entries).toHaveLength(2)
        expect(entries[0]).toMatchObject({
            side: 'local', characterCount: 5, byteLength: 1, scope: 'database-only',
        })
        expect(entries[1]).toMatchObject({
            side: 'remote', characterCount: 3, byteLength: 2, scope: 'database-only',
        })
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

    it('keeps a visible entry readable when removing it cannot update the index', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)
        const entry = await store.save({
            side: 'local', bytes: Uint8Array.of(1), characterCount: 1,
        })
        vi.spyOn(kv, 'setItem').mockRejectedValueOnce(new Error('index unavailable'))

        await expect(store.remove(entry.id)).rejects.toThrow('index unavailable')

        expect((await store.list()).map((value) => value.id)).toEqual([entry.id])
        expect(await store.read(entry.id)).toEqual(Uint8Array.of(1))
    })

    it('removes the index entry even when payload cleanup fails', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)
        const entry = await store.save({
            side: 'local', bytes: Uint8Array.of(1), characterCount: 1,
        })
        vi.spyOn(payloads, 'remove').mockRejectedValueOnce(new Error('blob unavailable'))

        await expect(store.remove(entry.id)).resolves.toBeUndefined()

        expect(await store.list()).toEqual([])
        expect(payloads.values.get(entry.id)).toEqual(Uint8Array.of(1))
    })

    it('keeps the previous index when saving a new entry cannot update it', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)
        const kept = await store.save({
            side: 'remote', bytes: Uint8Array.of(1), characterCount: 1,
        })
        vi.spyOn(kv, 'setItem').mockRejectedValueOnce(new Error('index unavailable'))

        await expect(store.save({
            side: 'local', bytes: Uint8Array.of(2), characterCount: 2,
        })).rejects.toThrow('index unavailable')

        expect((await store.list()).map((value) => value.id)).toEqual([kept.id])
        expect(payloads.values.size).toBe(1)
        expect(payloads.values.get(kept.id)).toEqual(Uint8Array.of(1))
    })

    it('preserves the index error when failed-save payload cleanup also fails', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)
        const kept = await store.save({
            side: 'remote', bytes: Uint8Array.of(1), characterCount: 1,
        })
        vi.spyOn(kv, 'setItem').mockRejectedValueOnce(new Error('index unavailable'))
        const remove = vi.spyOn(payloads, 'remove')
            .mockRejectedValueOnce(new Error('blob unavailable'))

        await expect(store.save({
            side: 'local', bytes: Uint8Array.of(2), characterCount: 2,
        })).rejects.toThrow('index unavailable')

        expect(remove).toHaveBeenCalledOnce()
        expect((await store.list()).map((value) => value.id)).toEqual([kept.id])
    })

    it('keeps a newly indexed backup when pruning an old payload fails', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)
        for (let index = 0; index < 5; index++) {
            await store.save({
                side: 'remote', bytes: Uint8Array.of(index), characterCount: index,
            })
        }
        vi.spyOn(payloads, 'remove').mockRejectedValueOnce(new Error('blob unavailable'))

        await expect(store.save({
            side: 'local', bytes: Uint8Array.of(9), characterCount: 9,
        })).resolves.toMatchObject({ side: 'local', characterCount: 9 })

        expect(await store.list()).toHaveLength(5)
    })

    it('serializes concurrent saves so both entries remain indexed', async () => {
        const store = new SyncConflictBackupStore(
            memoryKv(),
            sequenceClock(),
            memoryPayloadStore(),
        )

        await Promise.all([
            store.save({ side: 'local', bytes: Uint8Array.of(1), characterCount: 1 }),
            store.save({ side: 'remote', bytes: Uint8Array.of(2), characterCount: 2 }),
        ])

        expect(await store.list()).toHaveLength(2)
    })

    it('treats a corrupted index as empty', async () => {
        const kv = memoryKv()
        kv.values.set('index', { not: 'an array' })
        const store = new SyncConflictBackupStore(kv, sequenceClock())

        expect(await store.list()).toEqual([])

        kv.values.set('index', [{ id: 'x' }, null, 'garbage'])
        expect(await store.list()).toEqual([])
    })

    it('keeps valid metadata when a neighboring index entry is corrupted', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)
        const kept = await store.save({
            side: 'remote', bytes: Uint8Array.of(1), characterCount: 1,
        })
        kv.values.set('index', [
            ...(kv.values.get('index') as unknown[]),
            { id: 'corrupt' },
        ])

        await store.save({
            side: 'local', bytes: Uint8Array.of(2), characterCount: 2,
        })

        expect((await store.list()).map((entry) => entry.id)).toContain(kept.id)
    })

    it('keeps payload bytes outside the metadata key-value store', async () => {
        const kv = memoryKv()
        const payloads = memoryPayloadStore()
        const store = new SyncConflictBackupStore(kv, sequenceClock(), payloads)

        const entry = await store.save({
            side: 'local',
            bytes: Uint8Array.of(9, 8, 7),
            characterCount: 4,
        })

        expect(kv.values.has(`payload:${entry.id}`)).toBe(false)
        expect(payloads.values.get(entry.id)).toEqual(Uint8Array.of(9, 8, 7))
        expect(await store.read(entry.id)).toEqual(Uint8Array.of(9, 8, 7))
    })

    it('uses native app metadata and BlobStore payloads on Tauri', async () => {
        platformMocks.isTauri = true
        const store = getSyncConflictBackupStore()

        const entry = await store.save({
            side: 'remote',
            bytes: Uint8Array.of(6, 5, 4),
            characterCount: 2,
        })

        expect(platformMocks.settingValues.get('sync-conflict-backups.index.v1')).toEqual([
            expect.objectContaining({ id: entry.id, scope: 'database-only' }),
        ])
        expect(platformMocks.blobValues.get(`sync-conflict-backups/${entry.id}.risudat`))
            .toEqual(Uint8Array.of(6, 5, 4))
        expect(await store.read(entry.id)).toEqual(Uint8Array.of(6, 5, 4))
    })
})
