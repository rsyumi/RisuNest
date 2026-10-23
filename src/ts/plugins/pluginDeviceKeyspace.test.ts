import { advancePluginStorageRemoteChanges, setPluginStorageReloadHandler } from './pluginStorageProtection'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
    PluginDeviceKeyspace,
    PLUGIN_DEVICE_CACHE_BYTES,
    createBrowserPluginDeviceBackend,
    getPluginDeviceKeyspace,
    invalidatePluginDeviceKeyspaces,
    type PluginDeviceBackend,
    type PluginDeviceMutation,
    type PluginDeviceSpace,
    type PluginDeviceValue,
} from './pluginDeviceKeyspace'
import { SafeLocalPluginStorage } from './pluginSafeClass'
import { deletePluginDataItems, type PluginDataItem } from './pluginDataInventory'
import { pluginDeviceStorage } from './pluginDeviceStorage'

vi.mock('./plugins.svelte', () => ({ pluginStorageStore: { invalidate: vi.fn(), forOwner: vi.fn() } }))
vi.mock('../storage/persistentDataStoreFactory', () => ({ getPersistentDataStore: vi.fn() }))

vi.mock('./pluginDeviceStorage', () => {
    const values = new Map<string, unknown>()
    return {
        pluginDevicePrefix: 'safe_plugin_',
        pluginDeviceStorage: {
            async getItem<T>(key: string): Promise<T | null> {
                return (values.get(key) as T) ?? null
            },
            async setItem<T>(key: string, value: T): Promise<T> {
                values.set(key, value)
                return value
            },
            async removeItem(key: string): Promise<void> {
                values.delete(key)
            },
            async iterate(visit: (value: unknown, key: string) => void): Promise<void> {
                for (const [key, value] of values) visit(value, key)
            },
            async clear(): Promise<void> {
                values.clear()
            },
        },
    }
})

/** A store every owner shares, so a leak between keyspaces would be visible. */
function recordingBackend(
    rows: Array<{ owner: string; space: PluginDeviceSpace; key: string; value: PluginDeviceValue }> = [],
    options: { complete?: boolean } = {},
) {
    const state = rows.map((row) => ({ ...row }))
    const backend: PluginDeviceBackend = {
        async hydrate(owner: string) {
            const entries = state.filter((row) => row.owner === owner)
            const byteSize = entries.reduce((total, row) => total + row.value.length, 0)
            const complete = options.complete ?? true
            return {
                complete,
                byteSize,
                entries: complete
                    ? entries.map(({ space, key, value }) => ({ space, key, value }))
                    : [],
            }
        },
        async read(owner, space, key) {
            return (
                state.find(
                    (row) => row.owner === owner && row.space === space && row.key === key,
                )?.value ?? null
            )
        },
        async keys(owner, space) {
            return state
                .filter((row) => row.owner === owner && row.space === space)
                .map((row) => row.key)
        },
        async write(owner, mutations: readonly PluginDeviceMutation[]) {
            for (const mutation of mutations) {
                for (let index = state.length - 1; index >= 0; index -= 1) {
                    const row = state[index]
                    if (row.owner !== owner || row.space !== mutation.space) continue
                    if (mutation.type !== 'clear' && row.key !== mutation.key) continue
                    state.splice(index, 1)
                }
                if (mutation.type === 'set') {
                    state.push({
                        owner,
                        space: mutation.space,
                        key: mutation.key,
                        value: mutation.value,
                    })
                }
            }
        },
    }
    return { backend, rows: () => state }
}

describe('plugin device keyspace', () => {
    beforeEach(() => {
        localStorage.clear()
        invalidatePluginDeviceKeyspaces()
    })

    it('deletes device values through inventory and refreshes held and reacquired wrappers', async () => {
        const owner = 'inventory-delete'
        const held = getPluginDeviceKeyspace(owner)
        await held.setItem('json', 'json-value', '{"ok":true}')
        await held.setItem('string', 'string-value', 'text')
        const rows: PluginDataItem[] = (['json', 'string'] as const).map(space => ({
            owner, space, key: `${space}-value`, valueType: space, byteSize: 12, automatic: false,
        }))
        await deletePluginDataItems(rows, 'device')
        for (const wrapper of [held, getPluginDeviceKeyspace(owner)]) {
            await expect(wrapper.getItem('json', 'json-value')).resolves.toBeNull()
            await expect(wrapper.getItem('string', 'string-value')).resolves.toBeNull()
            await expect(wrapper.keys('json')).resolves.toEqual([])
            await expect(wrapper.keys('string')).resolves.toEqual([])
        }
    })

    it('retains durable values after a failed inventory deletion', async () => {
        const owner = 'inventory-failure'
        const held = getPluginDeviceKeyspace(owner)
        await held.setItem('json', 'kept', '42')
        const failure = vi.spyOn(pluginDeviceStorage, 'removeItem').mockRejectedValueOnce(new Error('disk failure'))
        await expect(deletePluginDataItems([{ owner, space: 'json', key: 'kept', valueType: 'json', byteSize: 2, automatic: false }], 'device')).rejects.toThrow('disk failure')
        await expect(held.getItem('json', 'kept')).resolves.toBe('42')
        await expect(held.keys('json')).resolves.toEqual(['kept'])
        failure.mockRestore()
    })

    it('rejects a mixed or mismatched deletion batch before any backend mutation', async () => {
        const owner = 'inventory-scope'
        const held = getPluginDeviceKeyspace(owner)
        await held.setItem('string', 'kept', 'text')
        const row: PluginDataItem = { owner, space: 'string', key: 'kept', valueType: 'string', byteSize: 4, automatic: false }
        await expect(deletePluginDataItems([row], 'library')).rejects.toThrow('scope changed')
        await expect(deletePluginDataItems([row, { ...row, space: undefined }], 'device')).rejects.toThrow('scope changed')
        await expect(held.getItem('string', 'kept')).resolves.toBe('text')
    })

    it('preserves binary contents and types after invalidating the device cache', async () => {
        const storage = new SafeLocalPluginStorage('binary-contract')
        const buffer = Uint8Array.of(0, 255, 128, 3).buffer
        await storage.setItem('value', { buffer, view: new Uint8Array(buffer, 1, 2), blob: new Blob([buffer], { type: 'audio/wav' }) })
        for (const wrapper of [storage, new SafeLocalPluginStorage('binary-contract')]) {
            invalidatePluginDeviceKeyspaces('binary-contract')
            const value = await wrapper.getItem<{ buffer: ArrayBuffer; view: Uint8Array; blob: Blob }>('value')
            expect(value!.buffer).toBeInstanceOf(ArrayBuffer)
            expect(new Uint8Array(value!.buffer)).toEqual(Uint8Array.of(0, 255, 128, 3))
            expect(value!.view).toBeInstanceOf(Uint8Array)
            expect(value!.view.buffer).toBe(value!.buffer)
            expect(value!.view.byteOffset).toBe(1)
            expect([...value!.view]).toEqual([255, 128])
            expect(value!.blob.type).toBe('audio/wav')
            expect(new Uint8Array(await value!.blob.arrayBuffer())).toEqual(new Uint8Array(buffer))
        }
        expect(await storage.keys()).toEqual(['value'])
        await storage.clear()
        expect(await storage.getItem('value')).toBeNull()
    })

    it('round-trips JSON representations and rejects unsupported values before writing', async () => {
        const storage = new SafeLocalPluginStorage('json-contract')
        const values = [{ a: 1 }, [1, null], null, 42, 'text']
        for (const value of values) {
            await storage.setItem('value', value)
            await expect(storage.getItem('value')).resolves.toEqual(JSON.parse(JSON.stringify(value)))
        }
        await storage.setItem('value', 'kept')
        const cycle: { self?: unknown } = {}; cycle.self = cycle
        const write = vi.spyOn(pluginDeviceStorage, 'setItem')
        for (const value of [undefined, () => {}, Symbol('unsupported'), cycle]) {
            await expect(storage.setItem('value', value)).rejects.toBeInstanceOf(TypeError)
            await expect(storage.getItem('value')).resolves.toBe('kept')
        }
        expect(write).not.toHaveBeenCalled()
        invalidatePluginDeviceKeyspaces('json-contract')
        await expect(storage.getItem('value')).resolves.toBe('kept')
        await expect(storage.keys()).resolves.toEqual(['value'])
        write.mockRestore()
    })

    it('keeps rich values and legacy JSON behavior, and rejects unsupported rich values without replacing data', async () => {
        const storage = new SafeLocalPluginStorage('rich-contract')
        for (const value of [new Date('2020-01-01T00:00:00Z'), new Map([['x', 1n]]), new Set([1, 2]), /test/gi, 1n, new DataView(Uint8Array.of(1, 2).buffer)]) {
            await storage.setItem('rich', value)
            invalidatePluginDeviceKeyspaces('rich-contract')
            expect(await storage.getItem('rich')).toEqual(value)
        }
        await storage.setItem('plain', { optional: undefined, nan: NaN, zero: -0 })
        expect(await storage.getItem('plain')).toEqual({ nan: null, zero: 0 })
        await storage.setItem('plain', { toJSON: () => ({ converted: true }) })
        expect(await storage.getItem('plain')).toEqual({ converted: true })
        await expect(storage.setItem('plain', new Error('unsupported'))).rejects.toMatchObject({ name: 'DataCloneError' })
        expect(await storage.getItem('plain')).toEqual({ converted: true })
    })

    it('orders slow binary writes across wrappers, reads and clear without blocking another key', async () => {
        const owner = 'ordered-binary'
        const first = new SafeLocalPluginStorage(owner)
        const second = new SafeLocalPluginStorage(owner)
        const blob = new Blob(['slow'])
        let finish!: (value: ArrayBuffer) => void
        vi.spyOn(blob, 'arrayBuffer').mockImplementation(() => new Promise(resolve => { finish = resolve }))
        const slow = first.setItem('slow', blob)
        const listed = first.keys()
        expect(await second.getItem('other')).toBeNull()
        const next = second.setItem('slow', Uint8Array.of(7))
        const read = first.getItem('slow')
        const cleared = second.clear()
        const after = first.getItem('slow')
        finish(new TextEncoder().encode('slow').buffer)
        await slow
        expect(await listed).toEqual(['slow'])
        await next
        expect(await read).toEqual(Uint8Array.of(7))
        await cleared
        expect(await after).toBeNull()
    })

    it('invalidates an already-held shared wrapper', async () => {
        const backend = createBrowserPluginDeviceBackend()
        const keyspace = getPluginDeviceKeyspace('shared-wrapper')
        await keyspace.setItem('string', 'key', 'old')
        await backend.write('shared-wrapper', [{ type: 'set', space: 'string', key: 'key', value: 'received' }])
        invalidatePluginDeviceKeyspaces('shared-wrapper')
        expect(getPluginDeviceKeyspace('shared-wrapper')).toBe(keyspace)
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('received')
    })

    it('does not resurrect a hydration invalidated by native application', async () => {
        const { backend } = recordingBackend([{ owner: 'a', space: 'string', key: 'key', value: 'old' }])
        const old = await backend.hydrate('a')
        let finish!: (value: typeof old) => void
        vi.spyOn(backend, 'hydrate').mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        const keyspace = new PluginDeviceKeyspace('a', backend)
        const first = keyspace.getItem('string', 'key')
        keyspace.invalidate()
        await backend.write('a', [{ type: 'set', space: 'string', key: 'key', value: 'received' }])
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('received')
        finish(old)
        await expect(first).resolves.toBe('received')
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('received')
    })

    it('discards a new hydration if an older in-flight write commits afterwards', async () => {
        const { backend } = recordingBackend([{ owner: 'a', space: 'string', key: 'key', value: 'old' }])
        const write = backend.write.bind(backend)
        let finish!: () => void
        const gate = new Promise<void>(resolve => { finish = resolve })
        const writes = vi.spyOn(backend, 'write').mockImplementationOnce(async (owner, mutations) => {
            await gate
            await write(owner, mutations)
        })
        const keyspace = new PluginDeviceKeyspace('a', backend)
        const pending = keyspace.setItem('string', 'key', 'new')
        await vi.waitFor(() => expect(writes).toHaveBeenCalledOnce())
        keyspace.invalidate()
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('old')
        finish()
        await pending
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('new')
    })

    it('does not let a late write acknowledgement overwrite a newer cached value', async () => {
        const { backend } = recordingBackend()
        const write = backend.write.bind(backend)
        let finish!: () => void
        const gate = new Promise<void>(resolve => { finish = resolve })
        const writes = vi.spyOn(backend, 'write').mockImplementationOnce(async (owner, mutations) => {
            await write(owner, mutations)
            await gate
        })
        const keyspace = new PluginDeviceKeyspace('a', backend)
        const older = keyspace.setItem('string', 'key', 'old')
        await vi.waitFor(() => expect(writes).toHaveBeenCalledOnce())
        await keyspace.setItem('string', 'key', 'new')
        finish()
        await older
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('new')
    })

    it.each(['set', 'delete', 'clear'] as const)('refreshes the cache after a committed %s loses its acknowledgement', async (operation) => {
        const { backend } = recordingBackend([{ owner: 'a', space: 'string', key: 'key', value: 'old' }])
        const write = backend.write.bind(backend)
        const keyspace = new PluginDeviceKeyspace('a', backend)
        await expect(keyspace.getItem('string', 'key')).resolves.toBe('old')
        vi.spyOn(backend, 'write').mockImplementationOnce(async (owner, mutations) => {
            await write(owner, mutations)
            throw new Error('write acknowledgement lost')
        })

        const pending = operation === 'set'
            ? keyspace.setItem('string', 'key', 'new')
            : operation === 'delete'
                ? keyspace.removeItem('string', 'key')
                : keyspace.clear('string')
        await expect(pending).rejects.toThrow('write acknowledgement lost')
        await expect(keyspace.getItem('string', 'key')).resolves.toBe(operation === 'set' ? 'new' : null)
        await expect(keyspace.keys('string')).resolves.toEqual(operation === 'set' ? ['key'] : [])
    })

    it('keeps recent values within the cache budget without hiding evicted keys', async () => {
        const { backend } = recordingBackend()
        const reads = vi.spyOn(backend, 'read')
        const hydrations = vi.spyOn(backend, 'hydrate')
        const keyspace = new PluginDeviceKeyspace('a', backend)
        const value = 'x'.repeat(PLUGIN_DEVICE_CACHE_BYTES / 4)
        for (const key of ['one', 'two', 'three']) await keyspace.setItem('string', key, value)
        await expect(keyspace.getItem('string', 'three')).resolves.toBe(value)
        expect(reads).not.toHaveBeenCalled()
        expect(hydrations).toHaveBeenCalledOnce()
        await expect(keyspace.keys('string')).resolves.toEqual(['one', 'three', 'two'])
        await expect(keyspace.getItem('string', 'one')).resolves.toBe(value)
        await expect(keyspace.getItem('string', 'two')).resolves.toBe(value)
        expect(reads).toHaveBeenCalledTimes(2)
        // One oversized value must not evict a different small setting.
        await keyspace.setItem('json', 'settings', '{"enabled":true}')
        await keyspace.setItem('string', 'large', 'x'.repeat(PLUGIN_DEVICE_CACHE_BYTES))
        await keyspace.setItem('json', 'binary', new Uint8Array(PLUGIN_DEVICE_CACHE_BYTES / 2))
        reads.mockRejectedValueOnce(new Error('Unexpected read of cached settings'))
        await expect(keyspace.getItem('json', 'settings')).resolves.toBe('{"enabled":true}')
        await expect(keyspace.getItem('string', 'two')).resolves.toBe(value)
        await expect(keyspace.keys('string')).resolves.toEqual(['large', 'one', 'three', 'two'])
    })

    it('does not accumulate replacement costs for the same cached key', async () => {
        const { backend } = recordingBackend()
        const reads = vi.spyOn(backend, 'read')
        const keyspace = new PluginDeviceKeyspace('a', backend)
        const value = 'x'.repeat(PLUGIN_DEVICE_CACHE_BYTES / 8)
        for (let index = 0; index < 8; index += 1) await keyspace.setItem('string', 'key', value)
        await expect(keyspace.getItem('string', 'key')).resolves.toBe(value)
        expect(reads).not.toHaveBeenCalled()
    })

    it('answers for one owner and one space only', async () => {
        const { backend, rows } = recordingBackend([
            { owner: 'plugin-a', space: 'string', key: 'shared', value: 'a string' },
            { owner: 'plugin-b', space: 'string', key: 'shared', value: 'b string' },
            { owner: 'plugin-a', space: 'json', key: 'shared', value: '"a json"' },
        ])
        const a = new PluginDeviceKeyspace('plugin-a', backend)
        const b = new PluginDeviceKeyspace('plugin-b', backend)

        await expect(a.getItem('string', 'shared')).resolves.toBe('a string')
        await expect(a.getItem('json', 'shared')).resolves.toBe('"a json"')
        await expect(a.keys('string')).resolves.toEqual(['shared'])

        await a.clear('string')
        await expect(a.getItem('string', 'shared')).resolves.toBeNull()
        await expect(a.getItem('json', 'shared')).resolves.toBe('"a json"')
        await expect(b.getItem('string', 'shared')).resolves.toBe('b string')
        expect(rows().map((row) => `${row.owner}/${row.space}`)).toEqual([
            'plugin-b/string',
            'plugin-a/json',
        ])
    })

    /** Invariant 12, for the device file. */
    it('settles a write only once the store has kept it', async () => {
        let release: (() => void) | null = null
        const gate = new Promise<void>((resolve) => {
            release = resolve
        })
        const { backend, rows } = recordingBackend()
        const gated: PluginDeviceBackend = {
            ...backend,
            async write(owner, mutations) {
                await gate
                await backend.write(owner, mutations)
            },
        }
        const keyspace = new PluginDeviceKeyspace('plugin-a', gated)

        let settled = false
        const write = keyspace.setItem('string', 'token', 'value').then(() => {
            settled = true
        })
        await Promise.resolve()
        await Promise.resolve()
        await Promise.resolve()
        expect(settled).toBe(false)
        expect(rows()).toEqual([])

        release?.()
        await write
        expect(settled).toBe(true)
        await expect(keyspace.getItem('string', 'token')).resolves.toBe('value')
    })

    it('loads uncached keys on demand past the hydration limit and keeps small committed values', async () => {
        const { backend } = recordingBackend(
            [
                { owner: 'plugin-a', space: 'string', key: 'one', value: 'first' },
                { owner: 'plugin-a', space: 'string', key: 'two', value: 'second' },
            ],
            { complete: false },
        )
        const reads = vi.spyOn(backend, 'read')
        const keyspace = new PluginDeviceKeyspace('plugin-a', backend)

        await expect(keyspace.getItem('string', 'one')).resolves.toBe('first')
        await expect(keyspace.keys('string')).resolves.toEqual(['one', 'two'])
        expect(reads).toHaveBeenCalledTimes(1)

        await keyspace.setItem('string', 'three', 'third')
        await expect(keyspace.getItem('string', 'three')).resolves.toBe('third')
        expect(reads).toHaveBeenCalledTimes(1)
        await expect(keyspace.getItem('string', 'two')).resolves.toBe('second')
        expect(reads).toHaveBeenCalledTimes(2)
        await expect(keyspace.keys('string')).resolves.toEqual(['one', 'three', 'two'])
    })

    it.each(['write', 'invalidate'] as const)('does not cache an old partial read after a newer %s', async action => {
        const owner = `partial-read-${action}`
        const { backend } = recordingBackend([{ owner, space: 'json', key: 'settings', value: 'old' }], { complete: false })
        const read = backend.read.bind(backend)
        let started!: () => void, release!: () => void
        const reading = new Promise<void>(resolve => { started = resolve })
        const gate = new Promise<void>(resolve => { release = resolve })
        let first = true
        backend.read = async (...args) => {
            const value = await read(...args)
            if (first) { first = false; started(); await gate }
            return value
        }
        const keyspace = new PluginDeviceKeyspace(owner, backend)
        try {
            const pending = keyspace.getItem('json', 'settings')
            await reading
            if (action === 'invalidate') keyspace.invalidate()
            await keyspace.setItem('json', 'settings', 'new')
            release()
            expect(await pending).toBe('old')
            expect(await keyspace.getItem('json', 'settings')).toBe('new')
        } finally { keyspace.dispose() }
    })

    it('keeps the browser backend inside the same owner and space boundary', async () => {
        const backend = createBrowserPluginDeviceBackend()
        const a = new PluginDeviceKeyspace('plugin-a', backend)
        const b = new PluginDeviceKeyspace('plugin-b', backend)

        await a.setItem('string', 'shared', 'a value')
        await b.setItem('string', 'shared', 'b value')
        await a.setItem('json', 'shared', '{"kept":true}')

        await expect(a.getItem('string', 'shared')).resolves.toBe('a value')
        await expect(b.getItem('string', 'shared')).resolves.toBe('b value')
        await a.clear('string')
        await expect(a.getItem('string', 'shared')).resolves.toBeNull()
        await expect(a.getItem('json', 'shared')).resolves.toBe('{"kept":true}')
        await expect(b.getItem('string', 'shared')).resolves.toBe('b value')
    })
})


describe('native plugin device observations', () => {
    it('keeps small JSON settings cached in an incomplete keyspace without hiding other keys or remote changes', async () => {
        const owner = 'mixed-cached-observations'
        const { backend, rows } = recordingBackend([
            { owner, space: 'json', key: 'settings', value: '{"enabled":true}' },
            { owner, space: 'json', key: 'binary', value: Uint8Array.of(0, 80, 76, 83, 1) },
        ], { complete: false })
        let sequence = 'epoch:0', available = true
        const read = backend.read.bind(backend)
        backend.sequence = async () => { if (!available) throw new Error('Unexpected native query'); return sequence }
        backend.read = async (...args) => { if (!available) throw new Error('Unexpected native read'); return read(...args) }
        const write = backend.write.bind(backend)
        backend.write = async (owner, mutations, observations) => {
            if (observations?.some(value => value.sequence !== sequence)) throw new Error('PLUGIN_STORAGE_STALE')
            await write(owner, mutations)
            return mutations.flatMap(value => value.type === 'clear' ? [] : [{ space: value.space, key: value.key, sequence }])
        }
        const keyspace = new PluginDeviceKeyspace(owner, backend)
        const storage = new SafeLocalPluginStorage(owner, keyspace)
        try {
            expect(await storage.getItem('settings')).toEqual({ enabled: true })
            available = false
            expect(await storage.getItem('settings')).toEqual({ enabled: true })
            expect(await storage.keys()).toEqual(['binary', 'settings'])
            available = true
            rows()[0].value = '{"enabled":false}'; sequence = 'epoch:1'
            await expect(storage.setItem('settings', { enabled: 'stale' })).rejects.toThrow('PLUGIN_STORAGE_STALE')
            advancePluginStorageRemoteChanges([JSON.stringify(['plugin-local', owner, 'json', 'settings'])])
            expect(await storage.getItem('settings')).toEqual({ enabled: false })
            await storage.setItem('settings', { enabled: 'fresh' })
            available = false
            expect(await storage.getItem('settings')).toEqual({ enabled: 'fresh' })
        } finally { keyspace.dispose() }
    })

    it('reuses hydrated values with their original tokens until invalidated, without read IPC', async () => {
        const owner = 'native-cached-observations'
        const { backend, rows } = recordingBackend([{ owner, space: 'json', key: 'settings', value: 'old' }])
        let sequence = 'epoch:0'
        backend.sequence = vi.fn(async () => sequence)
        const hydrate = backend.hydrate.bind(backend)
        backend.hydrate = async owner => {
            const loaded = await hydrate(owner)
            return { ...loaded, entries: loaded.entries.map(entry => ({ ...entry, sequence })) }
        }
        const reads = vi.spyOn(backend, 'read')
        const write = backend.write.bind(backend)
        backend.write = async (owner, mutations, observations) => {
            if (observations?.some(value => value.sequence !== sequence)) throw new Error('PLUGIN_STORAGE_STALE')
            await write(owner, mutations)
            return mutations.flatMap(value => value.type === 'clear' ? [] : [{ space: value.space, key: value.key, sequence }])
        }
        const storage = new PluginDeviceKeyspace(owner, backend)
        const reload = vi.fn(); setPluginStorageReloadHandler(reload)
        await storage.keys('json')
        advancePluginStorageRemoteChanges([JSON.stringify(['plugin-local', owner, 'json', 'settings'])])
        expect(reload).not.toHaveBeenCalled()
        await expect(storage.getItem('json', 'settings')).resolves.toBe('old')
        rows()[0].value = 'remote'; sequence = 'epoch:1'
        // The native commit can precede its renderer notification.
        await expect(storage.getItem('json', 'settings')).resolves.toBe('old')
        expect(reads).not.toHaveBeenCalled()
        expect(backend.sequence).not.toHaveBeenCalled()
        await expect(storage.setItem('json', 'settings', 'stale')).rejects.toThrow('PLUGIN_STORAGE_STALE')
        expect(rows()[0].value).toBe('remote')
        advancePluginStorageRemoteChanges([JSON.stringify(['plugin-local', owner, 'json', 'settings'])])
        await expect(storage.getItem('json', 'settings')).resolves.toBe('remote')
        await storage.setItem('json', 'settings', 'fresh')
        await expect(storage.getItem('json', 'settings')).resolves.toBe('fresh')
        expect(reads).not.toHaveBeenCalled()
        expect(backend.sequence).not.toHaveBeenCalled()
        storage.dispose()
        setPluginStorageReloadHandler(() => {})
    })
    it('captures before reading, keeps enumeration blind, and forwards stale tokens to the transaction', async () => {
        const owner = 'conditional-wrapper'
        const { backend, rows } = recordingBackend([{ owner, space: 'json', key: 'settings', value: 'old' }])
        let sequence = 'epoch:0'
        backend.sequence = async () => sequence
        const write = vi.spyOn(backend, 'write')
        const storage = new PluginDeviceKeyspace(owner, backend)
        const reload = vi.fn(); setPluginStorageReloadHandler(reload)
        await storage.keys('json')
        await storage.setItem('json', 'blind', 'accepted')
        expect(write.mock.calls[0][2]).toEqual([])
        await expect(storage.getItem('json', 'settings')).resolves.toBe('old')
        rows()[0].value = 'remote'; sequence = 'epoch:1'
        advancePluginStorageRemoteChanges([JSON.stringify(['plugin-local', owner, 'json', 'settings'])])
        write.mockRejectedValueOnce({ code: 'PLUGIN_STORAGE_STALE' })
        await expect(storage.setItem('json', 'settings', 'stale')).rejects.toThrow('PLUGIN_STORAGE_STALE')
        expect(write.mock.calls.at(-1)?.[2]).toEqual([{ space: 'json', key: 'settings', sequence: 'epoch:0' }])
        await expect(storage.getItem('json', 'settings')).resolves.toBe('remote')
        await storage.setItem('json', 'settings', 'fresh')
        expect(write.mock.calls.at(-1)?.[2]).toEqual([{ space: 'json', key: 'settings', sequence: 'epoch:1' }])
        expect(rows().find(row => row.key === 'settings')?.value).toBe('fresh')
        expect(reload).toHaveBeenCalledOnce()
        storage.dispose()
    })

    it('reloads a written-only key when a receive precedes the write IPC response', async () => {
        const owner = 'write-ipc-race'
        const { backend, rows } = recordingBackend()
        let sequence = 'epoch:0'
        backend.sequence = async () => sequence
        const write = backend.write.bind(backend)
        let respond!: () => void
        const response = new Promise<void>(resolve => { respond = resolve })
        backend.write = async (...args) => {
            await write(...args)
            const committed = sequence
            await response
            return [{ space: 'json', key: 'settings', sequence: committed }]
        }
        const storage = new PluginDeviceKeyspace(owner, backend)
        const reload = vi.fn(); setPluginStorageReloadHandler(reload)
        const writing = storage.setItem('json', 'settings', 'local')
        await vi.waitFor(() => expect(rows()).toHaveLength(1))
        rows()[0].value = 'remote'; sequence = 'epoch:1'
        rows().push({ owner, space: 'json', key: 'added', value: 'new' })
        advancePluginStorageRemoteChanges([JSON.stringify(['plugin-local', owner, 'json', 'settings']), JSON.stringify(['plugin-local', owner, 'json', 'added'])])
        respond(); await writing
        expect(reload).toHaveBeenCalledOnce()
        await expect(storage.getItem('json', 'settings')).resolves.toBe('remote')
        await expect(storage.keys('json')).resolves.toEqual(['added', 'settings'])
        storage.dispose()
    })
})
