import { createHash } from 'node:crypto'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { PluginDeviceMutation } from './pluginDeviceKeyspace'

const native = vi.hoisted(() => ({
    invoke: vi.fn(), values: new Map<string, string>(), android: false,
    parts: new Map<string, Uint8Array>(), uploads: new Map<string, { owner: string; length: number; bytes: number[] }>(), failFinish: false,
}))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: 'tauri', os: native.android ? 'android' : 'windows' })))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('./plugins.svelte', () => ({ pluginStorageStore: { invalidate: vi.fn() } }))
vi.mock('../storage/persistentDataStoreFactory', () => ({ getPersistentDataStore: vi.fn() }))

import { SafeLocalPluginStorage } from './pluginSafeClass'
import { invalidatePluginDeviceKeyspaces } from './pluginDeviceKeyspace'
import { deletePluginDataItems } from './pluginDataInventory'
import { pluginDeviceParts, pluginDeviceValueSize, readNativePluginDeviceValue, writeNativePluginDeviceValues, PLUGIN_DEVICE_PART_BYTES } from './pluginDeviceTransfer'
import { ANDROID_COMMIT_CHUNK_BYTES, ANDROID_LARGE_COMMIT_SIZE } from '../storage/androidCommitTransport'

beforeEach(() => {
    native.android = false
    native.values.clear()
    native.parts.clear()
    native.uploads.clear()
    native.failFinish = false
    invalidatePluginDeviceKeyspaces()
    native.invoke.mockReset().mockImplementation(async (command: string, args: {
        owner: string; key?: string; mutations?: PluginDeviceMutation[];
        byteLength?: number; uploadId?: string; offset?: number; data?: number[]; hash?: string;
    }) => {
        if (command === 'pds_plugin_device_sequence') return 'epoch:0'
        if (command === 'native_asset_bytes_probe') return false
        if (command === 'pds_open_plugin_device_part') {
            // The native side refuses an upload past two per plugin or eight in all.
            const owned = [...native.uploads.values()].filter(upload => upload.owner === args.owner).length
            if (owned >= 2 || native.uploads.size >= 8) throw new Error('plugin structure part uploads are full')
            const uploadId = `upload-${native.uploads.size}-${Math.random()}`
            native.uploads.set(uploadId, { owner: args.owner, length: args.byteLength!, bytes: [] })
            return uploadId
        }
        if (command === 'pds_append_plugin_device_part') {
            const upload = native.uploads.get(args.uploadId!)!
            if (args.offset !== upload.bytes.length) throw new Error('out-of-order part piece')
            upload.bytes.push(...args.data!)
            return upload.bytes.length
        }
        if (command === 'pds_finish_plugin_device_part') {
            if (native.failFinish) throw new Error('synthetic part failure')
            const upload = native.uploads.get(args.uploadId!)!
            native.uploads.delete(args.uploadId!)
            const bytes = Uint8Array.from(upload.bytes)
            const hash = createHash('sha256').update(bytes).digest('hex')
            native.parts.set(hash, bytes)
            return hash
        }
        if (command === 'pds_abort_plugin_device_part') { native.uploads.delete(args.uploadId!); return }
        if (command === 'pds_read_plugin_device_part') {
            const part = native.parts.get(args.hash!)
            if (!part) throw { code: 'validation', message: 'plugin structure part is missing' }
            return part.slice(args.offset!, args.offset! + 1024 * 1024).buffer
        }
        if (command === 'pds_read_plugin_device_value') {
            const value = native.values.get(args.key!)
            return value ?? null
        }
        if (command === 'pds_hydrate_plugin_device_storage') {
            return { complete: true, byteSize: 0, entries: [...native.values].map(([key, value]) =>
                ({ space: 'json', key, value })) }
        }
        if (command === 'pds_write_plugin_device_values') {
            for (const mutation of args.mutations ?? []) {
                if (mutation.type === 'set') native.values.set(mutation.key, mutation.value)
                else if (mutation.type === 'delete') native.values.delete(mutation.key)
                else native.values.clear()
            }
            return
        }
        throw new Error(`Unexpected native command ${command}`)
    })
})

describe('native JSON plugin values', () => {
    it('includes observation fields when routing a write across the Android IPC size boundary', async () => {
        native.android = true
        const observations = [{ space: 'string' as const, key: 'key', sequence: 'epoch:2' }]
        const mutation = { type: 'set' as const, space: 'string' as const, key: 'key', value: '' }
        const overhead = new TextEncoder().encode(JSON.stringify({ owner: 'owner', mutations: [mutation], observations })).length
        mutation.value = 'x'.repeat(ANDROID_LARGE_COMMIT_SIZE + 1 - overhead)
        let body = '', saved = ''
        native.invoke.mockImplementation(async (command, args) => {
            if (command === 'pds_commit_android_open') return { capacity: ANDROID_COMMIT_CHUNK_BYTES }
            if (command === 'pds_commit_android_chunk') { body += args.chunk; return new TextEncoder().encode(body).length }
            if (command === 'pds_plugin_write_android_finish') {
                const request = JSON.parse(body)
                expect(request.observations).toEqual(observations)
                saved = request.mutations[0].value
                return observations
            }
            if (command === 'pds_commit_android_cancel') return
            throw new Error('Oversized ordinary IPC write')
        })
        vi.stubGlobal('window', {})
        try { await expect(writeNativePluginDeviceValues('owner', [mutation], observations)).resolves.toEqual(observations) }
        finally { vi.unstubAllGlobals() }
        expect(saved).toBe(mutation.value)
    })

    it('counts encoded UTF-8 and splits only between complete code points', () => {
        for (const value of ['a가😀\"\n\u0000z', '😀😀😀', '', '한글'.repeat(20)]) {
            const size = new TextEncoder().encode(JSON.stringify(value)).length
            expect(pluginDeviceValueSize(value, size)).toBe(size)
            expect(() => pluginDeviceValueSize(value, size - 1)).toThrow('too large')
            const parts = [...pluginDeviceParts(value, 8)]
            expect(parts.map(({ part }) => part).join('')).toBe(value)
            for (const { part, byteLength } of parts) {
                expect(new TextEncoder().encode(part).length).toBe(byteLength)
                expect(pluginDeviceValueSize(part, 8)).toBeLessThanOrEqual(8)
            }
        }
        expect(() => pluginDeviceValueSize('\ud800')).toThrow('surrogate')
        expect(() => [...pluginDeviceParts('\udc00')]).toThrow('surrogate')
    })

    it('discards a changed ranged read before assembling the new value', async () => {
        let started = 0
        native.invoke.mockImplementation(async (command, args) => {
            if (command === 'pds_read_plugin_device_value') return { byteSize: ++started === 1 ? 12 : 7, version: String(started) }
            if (command === 'pds_read_plugin_device_range') {
                if (args.version === '1') return args.offset === 0
                    ? { type: 'value', value: 'old', nextOffset: 3 } : { type: 'changed' }
                return args.offset === 0 ? { type: 'value', value: '가', nextOffset: 3 }
                    : { type: 'value', value: '😀', nextOffset: 7 }
            }
            throw new Error(`Unexpected native command ${command}`)
        })
        await expect(readNativePluginDeviceValue('owner', 'string', 'key')).resolves.toBe('가😀')
    })

    it('commits a large value only after all parts arrive and aborts a failed transfer', async () => {
        const value = 'x'.repeat(PLUGIN_DEVICE_PART_BYTES - 3) + '😀가tail'
        let saved = 'before', received: string[] = [], fail = true
        native.invoke.mockImplementation(async (command, args) => {
            if (command === 'pds_open_plugin_device_write') { received = []; return { sessionId: 'session' } }
            if (command === 'pds_append_plugin_device_write') {
                if (fail && received.length) throw new Error('synthetic transport failure')
                received.push(args.part)
                return args.offset + new TextEncoder().encode(args.part).length
            }
            if (command === 'pds_write_plugin_device_values') {
                expect(args.mutations).toEqual([{ type: 'staged', sessionId: 'session' }])
                saved = received.join(''); return
            }
            if (command === 'pds_abort_plugin_device_write') { received = []; return }
            throw new Error(`Unexpected native command ${command}`)
        })
        const write = () => writeNativePluginDeviceValues('owner', [{ type: 'set', space: 'string', key: 'key', value }])
        await expect(write()).rejects.toThrow('synthetic transport failure')
        expect(saved).toBe('before')
        expect(received).toEqual([])
        fail = false
        await write()
        expect(saved).toBe(value)
        expect(received).toEqual([])
    })

    it('stages large values written at once within the native session limits, leaving room for sessions a reloaded page left open', async () => {
        const sessions = new Map<string, { owner: string; key: string; parts: string[] }>()
        const saved = new Map<string, string>()
        // A reloaded page left one session of this plugin and three of others open until they expire.
        sessions.set('left-owner', { owner: 'owner', key: 'old', parts: [] })
        for (let index = 0; index < 3; index++) sessions.set(`left-${index}`, { owner: `left-${index}`, key: 'old', parts: [] })
        native.invoke.mockImplementation(async (command, args) => {
            if (command === 'pds_open_plugin_device_write') {
                // The native side refuses a session past two per plugin or eight in all.
                const owned = [...sessions.values()].filter(session => session.owner === args.owner).length
                if (owned >= 2 || sessions.size >= 8) throw new Error('plugin device write sessions are full')
                const sessionId = `session-${sessions.size}-${Math.random()}`
                sessions.set(sessionId, { owner: args.owner, key: args.key, parts: [] })
                return { sessionId }
            }
            if (command === 'pds_append_plugin_device_write') {
                sessions.get(args.sessionId)!.parts.push(args.part)
                return args.offset + new TextEncoder().encode(args.part).length
            }
            if (command === 'pds_write_plugin_device_values') {
                for (const mutation of args.mutations) {
                    const session = sessions.get(mutation.sessionId)!
                    sessions.delete(mutation.sessionId)
                    saved.set(`${session.owner}/${session.key}`, session.parts.join(''))
                }
                return
            }
            if (command === 'pds_abort_plugin_device_write') { sessions.delete(args.sessionId); return }
            throw new Error(`Unexpected native command ${command}`)
        })
        const value = (name: string) => name + 'x'.repeat(PLUGIN_DEVICE_PART_BYTES)
        const writes = [['owner', 'a'], ['owner', 'b'], ['p1', 'v'], ['p2', 'v'], ['p3', 'v'], ['p4', 'v']]
        await Promise.all(writes.map(([owner, key]) => writeNativePluginDeviceValues(owner, [{ type: 'set', space: 'string', key, value: value(`${owner}/${key}`) }])))
        expect([...saved.keys()].sort()).toEqual(writes.map(([owner, key]) => `${owner}/${key}`).sort())
        for (const [name, stored] of saved) expect(stored === value(name)).toBe(true)
        expect(sessions.size).toBe(4)
    })

    it('preserves ordinary JSON and rejects unsupported values before the native boundary', async () => {
        const storage = new SafeLocalPluginStorage('native-json')
        for (const value of [{ nested: true }, [1, null], null, 42, 'text']) {
            await storage.setItem('value', value)
            await expect(storage.getItem('value')).resolves.toEqual(value)
        }
        native.invoke.mockClear()
        for (const value of [() => {}, Symbol('unsupported'), { nested: () => {} }, new WeakMap()]) {
            await expect(storage.setItem('value', value)).rejects.toBeInstanceOf(TypeError)
            await expect(storage.getItem('value')).resolves.toBe('text')
        }
        expect(native.invoke.mock.calls.some(([command]) => command === 'pds_write_plugin_device_values')).toBe(false)
        expect(native.values.get('value')).toBe('"text"')
    })

    it('invalidates the native hydrated cache after inventory deletion', async () => {
        const held = new SafeLocalPluginStorage('native-json')
        await held.setItem('value', { kept: true })
        await deletePluginDataItems([{ owner: 'native-json', space: 'json', key: 'value',
            valueType: 'json', byteSize: 13, automatic: false }], 'device')
        for (const wrapper of [held, new SafeLocalPluginStorage('native-json')]) {
            await expect(wrapper.getItem('value')).resolves.toBeNull()
            await expect(wrapper.keys()).resolves.toEqual([])
        }
    })
})

describe('native structured plugin values', () => {
    it('keeps values beyond JSON as structure text with their bytes in device parts', async () => {
        const storage = new SafeLocalPluginStorage('native-structured')
        const bytes = new Uint8Array(70 * 1024).map((_, index) => index % 251)
        const map = new Map<unknown, unknown>([['bytes', bytes], ['when', new Date(5)]])
        map.set('self', map)
        const value = { map, big: 10n, missing: undefined, blob: new Blob([bytes.subarray(0, 3)], { type: 'text/plain' }) }
        await storage.setItem('value', value)
        expect(native.values.get('value')!.startsWith('#risunest-structured:1\n')).toBe(true)
        expect([...native.parts.values()].map(part => part.byteLength)).toEqual([70 * 1024 + 3])
        invalidatePluginDeviceKeyspaces('native-structured')
        const read = (await new SafeLocalPluginStorage('native-structured').getItem<typeof value>('value'))!
        expect(read.map.get('self')).toBe(read.map)
        expect(read.map.get('bytes')).toEqual(bytes)
        expect((read.map.get('when') as Date).getTime()).toBe(5)
        expect(read.big).toBe(10n)
        expect('missing' in read && read.missing === undefined).toBe(true)
        expect(read.blob.type).toBe('text/plain')
        expect(new Uint8Array(await read.blob.arrayBuffer())).toEqual(bytes.subarray(0, 3))

        await storage.setItem('plain', { a: [1] })
        await storage.setItem('undefined', undefined)
        expect([native.values.get('plain'), native.values.get('undefined')]).toEqual(['{"a":[1]}', 'null'])
    })

    it('leaves the stored value unchanged when a part fails to store', async () => {
        const storage = new SafeLocalPluginStorage('native-structured')
        await storage.setItem('value', 'kept')
        native.failFinish = true
        await expect(storage.setItem('value', new Uint8Array(4))).rejects.toThrow('synthetic part failure')
        expect(native.values.get('value')).toBe('"kept"')
        expect(native.uploads.size).toBe(0)
        await expect(storage.getItem('value')).resolves.toBe('kept')
    })

    it('stores structured values written at once within the native upload limits', async () => {
        const storage = new SafeLocalPluginStorage('native-concurrent')
        await Promise.all(['a', 'b', 'c'].map((key, index) => storage.setItem(key, new Uint8Array([index]))))
        await Promise.all(Array.from({ length: 9 }, (_, index) => new SafeLocalPluginStorage(`native-owner-${index}`).setItem('value', new Uint8Array(1))))
        await expect(storage.getItem('c')).resolves.toEqual(new Uint8Array([2]))
        expect(native.uploads.size).toBe(0)
    })

    it('commits writes to one key in call order even when an earlier one stores parts', async () => {
        const storage = new SafeLocalPluginStorage('native-order')
        await Promise.all([storage.setItem('value', new Uint8Array(1)), storage.setItem('value', 'later')])
        expect(native.values.get('value')).toBe('"later"')
        await expect(storage.getItem('value')).resolves.toBe('later')
    })

    it('fails only the key whose structure value cannot be read', async () => {
        const marker = '#risunest-structured:1\n'
        native.values.set('missing', marker + JSON.stringify({ parts: ['p:' + 'a'.repeat(64)], bytes: 1,
            nodes: [{ type: 'ArrayBuffer', offset: 0, byteLength: 1 }], root: { ref: 0 } }))
        native.values.set('future', marker + JSON.stringify({ parts: [], bytes: 0, nodes: [{ type: 'FutureType' }], root: { ref: 0 } }))
        native.values.set('fine', '{"ok":true}')
        const storage = new SafeLocalPluginStorage('native-unreadable')
        for (const key of ['missing', 'future']) {
            await expect(storage.getItem(key)).rejects.toThrow('getLocalPluginStorage cannot read this value')
        }
        await expect(storage.getItem('fine')).resolves.toEqual({ ok: true })
        await expect(storage.keys()).resolves.toEqual(['fine', 'future', 'missing'])
    })

    it('retries a part read that a busy store or the IPC failed, within a bound', async () => {
        await new SafeLocalPluginStorage('native-retry').setItem('value', new Uint8Array([7]))
        const answer = native.invoke.getMockImplementation()!
        const failures: unknown[] = [{ code: 'store-error', message: 'database is locked' }, 'IPC request failed']
        native.invoke.mockImplementation(async (command: string, args: unknown) => {
            if (command === 'pds_read_plugin_device_part' && failures.length > 0) throw failures.shift()
            return answer(command, args)
        })
        await expect(new SafeLocalPluginStorage('native-retry').getItem('value')).resolves.toEqual(new Uint8Array([7]))
        failures.push(...Array.from({ length: 3 }, () => ({ code: 'store-error', message: 'database is locked' })))
        await expect(new SafeLocalPluginStorage('native-retry').getItem('value')).rejects.toThrow('getLocalPluginStorage cannot read this value')
    })
})
