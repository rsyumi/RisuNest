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
} from './pluginDeviceKeyspace'

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
    rows: Array<{ owner: string; space: PluginDeviceSpace; key: string; value: string }> = [],
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

    it('keeps writes durable but drops a cache that grows beyond its byte budget', async () => {
        const { backend } = recordingBackend()
        const reads = vi.spyOn(backend, 'read')
        const hydrations = vi.spyOn(backend, 'hydrate')
        const keyspace = new PluginDeviceKeyspace('a', backend)
        const value = 'x'.repeat(PLUGIN_DEVICE_CACHE_BYTES / 4)
        for (const key of ['one', 'two', 'three']) await keyspace.setItem('string', key, value)
        await expect(keyspace.getItem('string', 'three')).resolves.toBe(value)
        expect(reads).toHaveBeenCalledOnce()
        expect(hydrations).toHaveBeenCalledOnce()
        await expect(keyspace.keys('string')).resolves.toEqual(['one', 'three', 'two'])
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

    it('reads key by key when the keyspace is past the hydration limit', async () => {
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
        expect(reads).toHaveBeenCalledTimes(2)
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
