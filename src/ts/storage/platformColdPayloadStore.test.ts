import { describe, expect, test, vi } from 'vitest'
import {
    createGatedColdPayloadStore,
    createKeyValueColdPayloadStore,
    createLegacyNodeColdPayloadStore,
    createLegacyBrowserOpfsColdPayloadStore,
    createLegacyOpfsColdPayloadStore,
    createLegacyTauriColdPayloadStore,
} from './platformColdPayloadStore'

function memoryBackend(initial: Record<string, Uint8Array> = {}) {
    const values = new Map(Object.entries(initial).map(([key, value]) => [key, value.slice()]))
    return {
        values,
        write: vi.fn(async (key: string, value: Uint8Array) => void values.set(key, value.slice())),
        read: vi.fn(async (key: string) => values.get(key)?.slice() ?? null),
        keys: vi.fn(async () => [...values.keys()]),
        remove: vi.fn(async (key: string) => void values.delete(key)),
    }
}

describe('platform cold payload storage', () => {
    test('uses the exact flat browser OPFS legacy names', async () => {
        const values = new Map<string, Uint8Array>()
        const directory = {
            async getFileHandle(name: string, options?: { create?: boolean }) {
                if (!values.has(name) && !options?.create) throw new DOMException('', 'NotFoundError')
                values.set(name, values.get(name) ?? new Uint8Array())
                return {
                    async getFile() {
                        return new File([values.get(name)!.slice().buffer as ArrayBuffer], name)
                    },
                    async createWritable() {
                        return {
                            async write(value: ArrayBuffer) { values.set(name, new Uint8Array(value).slice()) },
                            async close() {},
                        }
                    },
                }
            },
            async *entries() {
                for (const name of values.keys()) yield [name, {}]
            },
            async removeEntry(name: string) { values.delete(name) },
        } as unknown as FileSystemDirectoryHandle
        const store = createLegacyBrowserOpfsColdPayloadStore(directory)

        await store.write('same', new Uint8Array([3]))

        expect([...values.keys()]).toEqual(['coldstorage_same.json'])
        expect(await store.read('same')).toEqual(new Uint8Array([3]))
        expect(await store.list()).toEqual(['same'])
        await store.remove('same')
        expect(await store.read('same')).toBeNull()
    })

    test('does not touch OPFS until a cold payload is actually used', async () => {
        let resolutions = 0
        const store = createLegacyBrowserOpfsColdPayloadStore(async () => {
            resolutions += 1
            throw new DOMException('', 'SecurityError')
        })

        expect(resolutions).toBe(0)
        await expect(store.read('any')).rejects.toBeInstanceOf(DOMException)
        expect(resolutions).toBe(1)
        await expect(store.list()).rejects.toBeInstanceOf(DOMException)
        expect(resolutions).toBe(2)
    })


    test('gates only cold writes and removals', async () => {
        const backend = memoryBackend()
        const legacy = createLegacyNodeColdPayloadStore(backend)
        const events: string[] = []
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) { events.push('lock'); return operation() },
            async runKeyedWrite<T>(_key: string, operation: () => Promise<T>) { return operation() },
            async runTransition<T>(operation: () => Promise<T>) { return operation() },
        }
        const store = createGatedColdPayloadStore(legacy, gate)

        await store.write('same', new Uint8Array([7]))
        expect(events).toEqual(['lock'])
        expect(await legacy.read('same')).toEqual(new Uint8Array([7]))

        events.length = 0
        expect(await store.read('same')).toEqual(new Uint8Array([7]))
        expect(await store.list()).toEqual(['same'])
        expect(events).toEqual([])

        await store.remove('same')
        expect(events).toEqual(['lock'])
        expect(await legacy.read('same')).toBeNull()
    })

    test('owns cold bytes before a deferred write gate proceeds', async () => {
        const backend = memoryBackend()
        const legacy = createLegacyNodeColdPayloadStore(backend)
        let release!: () => void
        const blocked = new Promise<void>((resolve) => { release = resolve })
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) { await blocked; return operation() },
            async runKeyedWrite<T>(_key: string, operation: () => Promise<T>) { return operation() },
            async runTransition<T>(operation: () => Promise<T>) { return operation() },
        }
        const store = createGatedColdPayloadStore(legacy, gate)
        const source = new Uint8Array([1])
        const pending = store.write('same', source)
        source[0] = 9
        release()

        await pending
        expect(await legacy.read('same')).toEqual(new Uint8Array([1]))
    })

    test('preserves exact legacy Tauri, Node, and OPFS names', async () => {
        const backend = memoryBackend()
        const tauri = createLegacyTauriColdPayloadStore(backend)
        const node = createLegacyNodeColdPayloadStore(backend)
        const opfs = createLegacyOpfsColdPayloadStore(backend)

        await tauri.write('same', new Uint8Array([1]))
        await node.write('same', new Uint8Array([2]))
        await opfs.write('same', new Uint8Array([3]))
        expect([...backend.values.keys()].sort()).toEqual([
            'coldstorage/same', 'coldstorage/same.json', 'coldstorage_same.json',
        ])
    })


    test('lists sorted logical keys without reading payload bytes and copies writes', async () => {
        const backend = memoryBackend()
        const store = createKeyValueColdPayloadStore(backend, {
            key: (id) => `coldstorage/${id}`, prefix: 'coldstorage/', suffix: '',
        })
        const source = new Uint8Array([1])
        await store.write('z', source)
        source[0] = 9
        await store.write('a', new Uint8Array([2]))
        backend.read.mockClear()

        expect(await store.list()).toEqual(['a', 'z'])
        expect(backend.read).not.toHaveBeenCalled()
        expect(await store.read('z')).toEqual(new Uint8Array([1]))
    })

})
