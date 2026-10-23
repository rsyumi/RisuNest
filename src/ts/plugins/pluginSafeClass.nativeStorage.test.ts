import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { PluginDeviceMutation, PluginDeviceValue } from './pluginDeviceKeyspace'

const native = vi.hoisted(() => ({ invoke: vi.fn(), values: new Map<string, PluginDeviceValue>(), android: false }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: 'tauri', os: native.android ? 'android' : 'windows' })))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('./plugins.svelte', () => ({ pluginStorageStore: { invalidate: vi.fn() } }))
vi.mock('../storage/persistentDataStoreFactory', () => ({ getPersistentDataStore: vi.fn() }))

import { SafeLocalPluginStorage } from './pluginSafeClass'
import { invalidatePluginDeviceKeyspaces } from './pluginDeviceKeyspace'
import { deletePluginDataItems, readPluginDataValue } from './pluginDataInventory'
import { pluginDeviceParts, pluginDeviceValueSize, readNativePluginDeviceValue, writeNativePluginDeviceValues, PLUGIN_DEVICE_PART_BYTES } from './pluginDeviceTransfer'
import { ANDROID_COMMIT_CHUNK_BYTES, ANDROID_LARGE_COMMIT_SIZE } from '../storage/androidCommitTransport'

beforeEach(() => {
    native.android = false
    native.values.clear()
    invalidatePluginDeviceKeyspaces()
    native.invoke.mockReset().mockImplementation(async (command: string, args: {
        owner: string; key?: string; mutations?: PluginDeviceMutation[];
    }) => {
        if (command === 'pds_plugin_device_sequence') return 'epoch:0'
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
    it.each([false, true])('persists binary parts and reads fresh buffers through native IPC (android=%s)', async android => {
        native.android = android
        const sessions = new Map<string, { key: string; bytes: Uint8Array; offset: number }>()
        const previous = native.invoke.getMockImplementation()!
        let fail = false
        native.invoke.mockImplementation(async (command, args, options) => {
            if (command === 'native_asset_bytes_probe') return true
            if (command === 'pds_open_plugin_device_binary_write') {
                sessions.set('binary-session', { key: args.key, bytes: new Uint8Array(args.byteLength), offset: 0 })
                return { sessionId: 'binary-session' }
            }
            if (command === 'pds_append_plugin_device_bytes_raw') {
                if (fail) throw new Error('synthetic binary disk failure')
                const session = sessions.get(options.headers['x-risunest-upload-id'])!
                const offset = Number(options.headers['x-risunest-offset'])
                expect(offset).toBe(session.offset)
                session.bytes.set(args, offset)
                session.offset += args.byteLength
                return session.offset
            }
            if (command === 'pds_write_plugin_device_values' && args.mutations[0].type === 'staged') {
                const session = sessions.get(args.mutations[0].sessionId)!
                expect(session.offset).toBe(session.bytes.length)
                native.values.set(session.key, session.bytes)
                return []
            }
            if (command === 'pds_abort_plugin_device_write') { sessions.delete(args.sessionId); return }
            if (command === 'pds_hydrate_plugin_device_storage') return { complete: false, byteSize: 0, entries: [] }
            if (command === 'pds_read_plugin_device_value') {
                const bytes = native.values.get(args.key)
                return bytes instanceof Uint8Array ? { binary: true, byteSize: bytes.length, version: 'binary-version' } : bytes ?? null
            }
            if (command === 'pds_read_plugin_device_bytes') return (native.values.get(args.key) as Uint8Array).slice(args.offset, args.offset + 256 * 1024).buffer
            return previous(command, args)
        })
        const storage = new SafeLocalPluginStorage('native-binary')
        const bytes = new Uint8Array(256 * 1024 + 3).map((_, index) => index % 256)
        await storage.setItem('binary', bytes)
        expect(native.values.get('binary')).toBeInstanceOf(Uint8Array)
        expect(sessions.size).toBe(0)
        const description = JSON.parse((await readPluginDataValue({ owner: 'native-binary', space: 'json', key: 'binary', valueType: 'json', byteSize: 0, automatic: false }))!)
        expect(description).toEqual({ type: 'Uint8Array', byteSize: (native.values.get('binary') as Uint8Array).byteLength })
        invalidatePluginDeviceKeyspaces('native-binary')
        const first = await storage.getItem<Uint8Array>('binary')
        expect(first).toEqual(bytes)
        structuredClone(first, { transfer: [first!.buffer] })
        expect(await storage.getItem('binary')).toEqual(bytes)
        fail = true
        await expect(storage.setItem('binary', Uint8Array.of(9))).rejects.toThrow('synthetic binary disk failure')
        expect(sessions.size).toBe(0)
        expect(await storage.getItem('binary')).toEqual(bytes)
        await storage.removeItem('binary')
        expect(await storage.getItem('binary')).toBeNull()
    })

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

    it('preserves ordinary JSON and rejects unsupported values before the native boundary', async () => {
        const storage = new SafeLocalPluginStorage('native-json')
        for (const value of [{ nested: true }, [1, null], null, 42, 'text']) {
            await storage.setItem('value', value)
            await expect(storage.getItem('value')).resolves.toEqual(value)
        }
        const cycle: { self?: unknown } = {}; cycle.self = cycle
        native.invoke.mockClear()
        for (const value of [undefined, () => {}, Symbol('unsupported'), cycle]) {
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
