import { describe, expect, test, vi } from 'vitest'
import {
    createKeyValueColdPayloadStore,
    createLegacyNodeColdPayloadStore,
    createLegacyOpfsColdPayloadStore,
    createLegacyTauriColdPayloadStore,
    createRootedColdPayloadStoreFactory,
    createGatedResolvingColdPayloadStore,
    createPlatformRootedColdPayloadStoreFactory,
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
    test('shares the generated backend with the platform blob factory seam', async () => {
        const backend = memoryBackend()
        const legacy = createLegacyNodeColdPayloadStore(backend)
        const factory = await createPlatformRootedColdPayloadStoreFactory(legacy, async () => backend)
        const generated = factory.open({ kind: 'generation', id: 'shared' })

        await generated.write('zero', new Uint8Array())

        expect(backend.values.get(generatedColdPayloadKey('shared', 'zero'))).toEqual(new Uint8Array())
        expect(await legacy.read('zero')).toBeNull()
    })

    test('gates only cold writes and refreshes before selecting one active root', async () => {
        const backend = memoryBackend()
        const factory = createRootedColdPayloadStoreFactory({
            legacy: createLegacyNodeColdPayloadStore(backend),
            generatedBackend: backend,
        })
        const events: string[] = []
        let root = { kind: 'generation' as const, id: 'old' }
        const resolver = {
            refresh: async () => { events.push('refresh'); root = { kind: 'generation', id: 'current' } },
            getActiveColdRoot: () => { events.push(`resolve:${root.id}`); return root },
        }
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) { events.push('lock'); return operation() },
            async runMigration<T>(operation: () => Promise<T>) { return operation() },
        }
        const store = createGatedResolvingColdPayloadStore(factory, resolver, gate)

        await store.write('same', new Uint8Array([7]))
        expect(events).toEqual(['lock', 'refresh', 'resolve:current'])
        expect(await factory.open({ kind: 'generation', id: 'current' }).read('same')).toEqual(new Uint8Array([7]))

        events.length = 0
        expect(await store.read('same')).toEqual(new Uint8Array([7]))
        expect(await store.list()).toEqual(['same'])
        expect(events).toEqual(['resolve:current', 'resolve:current'])

        events.length = 0
        await store.remove('same')
        expect(events).toEqual(['lock', 'refresh', 'resolve:current'])
    })

    test('owns cold bytes before a deferred write gate proceeds', async () => {
        const backend = memoryBackend()
        const factory = createRootedColdPayloadStoreFactory({
            legacy: createLegacyNodeColdPayloadStore(backend),
            generatedBackend: backend,
        })
        let release!: () => void
        const blocked = new Promise<void>((resolve) => { release = resolve })
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) { await blocked; return operation() },
            async runMigration<T>(operation: () => Promise<T>) { return operation() },
        }
        const store = createGatedResolvingColdPayloadStore(factory, {
            async refresh() {},
            getActiveColdRoot() { return { kind: 'generation', id: 'current' } },
        }, gate)
        const source = new Uint8Array([1])
        const pending = store.write('same', source)
        source[0] = 9
        release()

        await pending
        expect(await factory.open({ kind: 'generation', id: 'current' }).read('same')).toEqual(new Uint8Array([1]))
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
