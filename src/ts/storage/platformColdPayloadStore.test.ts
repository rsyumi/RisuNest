import { describe, expect, test, vi } from 'vitest'
import {
    createKeyValueColdPayloadStore,
    createLegacyNodeColdPayloadStore,
    createLegacyOpfsColdPayloadStore,
    createLegacyTauriColdPayloadStore,
    createRootedColdPayloadStoreFactory,
    generatedColdPayloadKey,
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

describe('rooted cold payload storage', () => {
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

    test('isolates generated roots and distinguishes missing from zero bytes', async () => {
        const backend = memoryBackend()
        const factory = createRootedColdPayloadStoreFactory({
            legacy: createKeyValueColdPayloadStore(backend, {
                key: (id) => `coldstorage/${id}`, prefix: 'coldstorage/', suffix: '',
            }),
            generatedBackend: backend,
        })
        const first = factory.open({ kind: 'generation', id: 'first_1' })
        const second = factory.open({ kind: 'generation', id: 'second-2' })

        await first.write('same', new Uint8Array())
        await second.write('same', new Uint8Array([2]))
        expect(await first.read('same')).toEqual(new Uint8Array())
        expect(await first.read('missing')).toBeNull()
        expect(await second.read('same')).toEqual(new Uint8Array([2]))
        expect(await first.list()).toEqual(['same'])
        expect(await second.list()).toEqual(['same'])
        await first.remove('same')
        expect(await second.read('same')).toEqual(new Uint8Array([2]))
    })

    test('lists sorted logical keys without reading payload bytes and copies writes', async () => {
        const backend = memoryBackend()
        const factory = createRootedColdPayloadStoreFactory({
            legacy: createKeyValueColdPayloadStore(backend, {
                key: (id) => `coldstorage/${id}`, prefix: 'coldstorage/', suffix: '',
            }),
            generatedBackend: backend,
        })
        const store = factory.open({ kind: 'generation', id: 'stage' })
        const source = new Uint8Array([1])
        await store.write('z', source)
        source[0] = 9
        await store.write('a', new Uint8Array([2]))
        backend.read.mockClear()

        expect(await store.list()).toEqual(['a', 'z'])
        expect(backend.read).not.toHaveBeenCalled()
        expect(await store.read('z')).toEqual(new Uint8Array([1]))
    })

    test('validates generated identifiers and uses UTF-8 key hex', () => {
        expect(generatedColdPayloadKey('safe_1', '한글')).toBe(
            'blobstore/generations/safe_1/coldstorage/ed959ceab880.bin',
        )
        expect(() => generatedColdPayloadKey('../escape', 'key')).toThrow(TypeError)
    })
})
