import { describe, expect, it, vi } from 'vitest'
import type { PersistentDataStore, PluginStorageMutation } from '../storage/persistentDataStore'
import { createPluginStorageStore } from './pluginStorageStore'
import { applyPluginDatabaseUpdate } from './pluginDatabaseAccess'
import type { Database } from '../storage/database.svelte'
import { UNOWNED_PLUGIN_OWNER } from './pluginOwner'

interface Row {
    owner: string
    key: string
    value: unknown
}

/** A store every owner shares, so a leak between them would be visible. */
function sharedStore(initial: readonly Row[]) {
    let rows = initial.map((row) => ({ ...row }))
    const committed: PluginStorageMutation[] = []
    const store = {
        open: vi.fn(async () => undefined),
        queryPluginStorage: vi.fn(async () => ({
            revision: 1,
            items: rows.map((row) => ({
                owner: row.owner,
                key: row.key,
                byteSize: JSON.stringify(row.value).length,
            })),
        })),
        readPluginStorage: vi.fn(async (owner: string, key: string) => {
            const row = rows.find((item) => item.owner === owner && item.key === key)
            return row ? { revision: 1, value: structuredClone(row.value) } : null
        }),
        acquireRevision: vi.fn(async () => ({
            revision: 1,
            queryPluginStorage: async () => ({
                revision: 1,
                items: rows.map((row) => ({
                    owner: row.owner,
                    key: row.key,
                    byteSize: JSON.stringify(row.value).length,
                })),
            }),
            readPluginStorage: async (owner: string, key: string) => {
                const row = rows.find((item) => item.owner === owner && item.key === key)
                return row ? { revision: 1, value: structuredClone(row.value) } : null
            },
            release: vi.fn(async () => undefined),
        })),
    } as unknown as PersistentDataStore
    const storage = createPluginStorageStore({
        store,
        getStorageAuthorityEpoch: () => 0,
        assertPersistentMutationAllowed: vi.fn(),
        mutate: async (mutations) => {
            for (const mutation of mutations) {
                committed.push(mutation)
                if (mutation.type === 'clear') {
                    rows = rows.filter((row) => row.owner !== mutation.owner)
                    continue
                }
                rows = rows.filter(
                    (row) => !(row.owner === mutation.owner && row.key === mutation.key),
                )
                if (mutation.type === 'set') {
                    rows.push({
                        owner: mutation.owner,
                        key: mutation.key,
                        value: mutation.value,
                    })
                }
            }
        },
    })
    return { storage, committed, rows: () => rows }
}

describe('plugin storage isolation', () => {
    /** Invariant 11. */
    it('never lets one plugin read, list or clear another plugin key', async () => {
        const { storage, rows } = sharedStore([
            { owner: 'plugin-a', key: 'a-key', value: 'a' },
            { owner: 'plugin-b', key: 'b-key', value: 'b' },
            { owner: 'plugin-b', key: 'shared', value: 'b shared' },
            { owner: 'plugin-a', key: 'shared', value: 'a shared' },
        ])
        const a = storage.forOwner('plugin-a')
        const b = storage.forOwner('plugin-b')

        await expect(a.keys()).resolves.toEqual(['a-key', 'shared'])
        await expect(a.length()).resolves.toBe(2)
        await expect(a.getItem('b-key')).resolves.toBeNull()
        await expect(a.getItem('shared')).resolves.toBe('a shared')
        await expect(a.snapshot()).resolves.toEqual({ 'a-key': 'a', shared: 'a shared' })

        await a.clear()

        await expect(a.keys()).resolves.toEqual([])
        await expect(b.keys()).resolves.toEqual(['b-key', 'shared'])
        await expect(b.getItem('shared')).resolves.toBe('b shared')
        expect(rows().map((row) => `${row.owner}/${row.key}`)).toEqual([
            'plugin-b/b-key',
            'plugin-b/shared',
        ])

        await a.removeItem('b-key')
        await expect(b.getItem('b-key')).resolves.toBe('b')
    })

    /** Invariant 13. */
    it('keeps the unowned rows out of every ordinary plugin answer', async () => {
        const { storage } = sharedStore([
            { owner: UNOWNED_PLUGIN_OWNER, key: 'pm_store', value: { apiKey: 'imported' } },
            { owner: 'provider-manager', key: 'own', value: 'mine' },
        ])
        const plugin = storage.forOwner('provider-manager')

        await expect(plugin.getItem('pm_store')).resolves.toBeNull()
        await expect(plugin.keys()).resolves.toEqual(['own'])
        await expect(plugin.length()).resolves.toBe(1)
        await expect(plugin.snapshot()).resolves.toEqual({ own: 'mine' })
    })

    /** Invariant 23. */
    it('leaves an unowned row untouched when a plugin writes the same key', async () => {
        const { storage, rows, committed } = sharedStore([
            { owner: UNOWNED_PLUGIN_OWNER, key: 'pm_store', value: { apiKey: 'imported' } },
        ])
        const plugin = storage.forOwner('provider-manager')

        await expect(plugin.getItem('pm_store')).resolves.toBeNull()
        await plugin.setItem('pm_store', { apiKey: 'default' })

        expect(committed).toEqual([
            {
                type: 'set',
                owner: 'provider-manager',
                key: 'pm_store',
                value: { apiKey: 'default' },
            },
        ])
        expect(rows()).toEqual([
            { owner: UNOWNED_PLUGIN_OWNER, key: 'pm_store', value: { apiKey: 'imported' } },
            { owner: 'provider-manager', key: 'pm_store', value: { apiKey: 'default' } },
        ])
        await expect(plugin.getItem('pm_store')).resolves.toEqual({ apiKey: 'default' })

        await plugin.clear()
        expect(rows()).toEqual([
            { owner: UNOWNED_PLUGIN_OWNER, key: 'pm_store', value: { apiKey: 'imported' } },
        ])
    })

    /** Invariant 11, through the full database entry point. */
    it('names the writing plugin in the sidecar a full replacement carries', () => {
        const candidate = {
            pluginCustomStorage: { theirs: 'kept', mine: 'old' },
        } as unknown as Database
        applyPluginDatabaseUpdate(
            candidate,
            { mine: 'new' },
            ['username'],
            'plugin-a',
            (key) => (key === 'theirs' ? 'plugin-b' : 'plugin-a'),
        )

        expect(candidate.pluginCustomStorage).toEqual({ theirs: 'kept', mine: 'new' })
        const meta = (candidate as Database & {
            pluginStorageMeta?: Record<string, { plugin: string }>
        }).pluginStorageMeta
        expect(meta?.theirs.plugin).toBe('plugin-b')
        expect(meta?.mine.plugin).toBe('plugin-a')
    })

    /** Invariant 11, for the read-modify-write round trip plugins usually make. */
    it('keeps the other plugin keys a full replacement never showed the caller', () => {
        const candidate = {
            pluginCustomStorage: { theirs: 'kept', mine: 'old' },
        } as unknown as Database
        applyPluginDatabaseUpdate(
            candidate,
            { pluginCustomStorage: { mine2: 'new' }, characters: [] },
            ['pluginCustomStorage', 'characters'],
            'plugin-a',
            (key) => (key === 'theirs' ? 'plugin-b' : 'plugin-a'),
        )

        expect(candidate.pluginCustomStorage).toEqual({ theirs: 'kept', mine2: 'new' })
        const meta = (candidate as Database & {
            pluginStorageMeta?: Record<string, { plugin: string }>
        }).pluginStorageMeta
        expect(meta?.theirs.plugin).toBe('plugin-b')
        expect(meta?.mine2.plugin).toBe('plugin-a')
    })

    /** Invariant 12. */
    it('resolves a write only once the store has committed it', async () => {
        let release: (() => void) | null = null
        const gate = new Promise<void>((resolve) => {
            release = resolve
        })
        const rows: Row[] = []
        const store = {
            open: vi.fn(async () => undefined),
            queryPluginStorage: vi.fn(async () => ({ revision: 1, items: [] })),
            readPluginStorage: vi.fn(async () => null),
        } as unknown as PersistentDataStore
        const storage = createPluginStorageStore({
            store,
            getStorageAuthorityEpoch: () => 0,
            assertPersistentMutationAllowed: vi.fn(),
            mutate: async (mutations) => {
                await gate
                for (const mutation of mutations) {
                    if (mutation.type === 'set') {
                        rows.push({
                            owner: mutation.owner,
                            key: mutation.key,
                            value: mutation.value,
                        })
                    }
                }
            },
        })

        let settled = false
        const write = storage
            .forOwner('plugin-a')
            .setItem('durable', 'value')
            .then(() => {
                settled = true
            })
        await Promise.resolve()
        await Promise.resolve()
        expect(settled).toBe(false)
        expect(rows).toEqual([])

        release?.()
        await write
        expect(settled).toBe(true)
        expect(rows).toEqual([{ owner: 'plugin-a', key: 'durable', value: 'value' }])
    })
})
