import { describe, expect, it, vi } from 'vitest'
import { RevisionConflictError } from './persistentDataStore'
import type { ColdAlias, PersistentRoot } from './persistentDataStore'
import { createCompleteColdPayloadStore } from './coldPayloadRepository'

function alias(overrides: Partial<ColdAlias> = {}): ColdAlias {
    return {
        key: 'cold-1',
        objectHash: 'ab'.repeat(32),
        size: 3,
        metadata: { source: 'fixture' },
        ...overrides,
    }
}

function harness(initial?: ColdAlias) {
    let revision = 4
    let current = initial
    const objects = new Map<string, Uint8Array>()
    if (initial?.objectHash) objects.set(initial.objectHash, Uint8Array.of(1, 2, 3))
    const catalog = {
        readRoot: vi.fn(async () => ({ revision, value: {} as PersistentRoot })),
        readColdAlias: vi.fn(async (key: string) => current?.key === key
            ? { revision, value: structuredClone(current) }
            : null),
        listColdAliases: vi.fn(async () => ({
            revision,
            value: current ? [structuredClone(current)] : [],
        })),
        commitColdAlias: vi.fn(async (next: ColdAlias, expectedRevision: number) => {
            if (expectedRevision !== revision) {
                throw new RevisionConflictError(expectedRevision, revision)
            }
            current = structuredClone(next)
            revision += 1
            return { revision }
        }),
        deleteColdAlias: vi.fn(async (key: string, expectedRevision: number) => {
            if (expectedRevision !== revision) {
                throw new RevisionConflictError(expectedRevision, revision)
            }
            if (current?.key === key) current = undefined
            revision += 1
            return { revision }
        }),
    }
    const cas = {
        prepare: vi.fn(async (data: Uint8Array) => {
            const contentHash = 'cd'.repeat(32)
            objects.set(contentHash, data.slice())
            return {
                contentHash,
                byteSize: data.byteLength,
                physicalKey: `assets-v2/objects/cd/${'cd'.repeat(31)}`,
                deduplicated: false,
            }
        }),
        readObject: vi.fn(async (hash: string) => objects.get(hash)?.slice() ?? null),
        readObjectRange: vi.fn(),
        statObject: vi.fn(async (hash: string) => objects.get(hash)?.byteLength ?? null),
    }
    const legacyValues = new Map<string, Uint8Array>()
    const legacy = {
        read: vi.fn(async (key: string) => legacyValues.get(key)?.slice() ?? null),
        write: vi.fn(),
        list: vi.fn(async () => [...legacyValues.keys()].sort()),
        remove: vi.fn(),
    }
    const store = createCompleteColdPayloadStore({ catalog, cas, legacy })
    return { catalog, cas, legacy, legacyValues, objects, store, current: () => current }
}

describe('revisioned cold payload repository', () => {
    it('reads exact bytes from the immutable object selected by its typed alias', async () => {
        const fixture = harness(alias())

        expect(await fixture.store.read('cold-1')).toEqual(Uint8Array.of(1, 2, 3))
        expect(fixture.legacy.read).not.toHaveBeenCalled()
    })

    it('fails closed on a missing alias instead of probing the legacy namespace', async () => {
        const fixture = harness()
        fixture.legacyValues.set('cold-1', Uint8Array.of(9))

        expect(await fixture.store.read('cold-1')).toBeNull()
        expect(fixture.legacy.read).not.toHaveBeenCalled()
    })

    it('uses legacy bytes only for an explicit null-hash alias and verifies the size', async () => {
        const fixture = harness(alias({ objectHash: null }))
        fixture.legacyValues.set('cold-1', Uint8Array.of(7, 8, 9))

        expect(await fixture.store.read('cold-1')).toEqual(Uint8Array.of(7, 8, 9))
        fixture.legacyValues.set('cold-1', Uint8Array.of(7))
        await expect(fixture.store.read('cold-1')).rejects.toThrow('legacy size mismatch')
    })

    it('rejects a missing or wrong-sized immutable object without legacy rollback', async () => {
        const fixture = harness(alias())
        fixture.objects.delete('ab'.repeat(32))
        fixture.legacyValues.set('cold-1', Uint8Array.of(9, 9, 9))

        await expect(fixture.store.read('cold-1')).rejects.toThrow('immutable object is missing')
        expect(fixture.legacy.read).not.toHaveBeenCalled()

        fixture.objects.set('ab'.repeat(32), Uint8Array.of(1))
        await expect(fixture.store.read('cold-1')).rejects.toThrow('size mismatch')
    })

    it('prepares owned bytes before publishing one alias and retains opaque metadata', async () => {
        const fixture = harness(alias())
        const source = Uint8Array.of(4, 5, 6)
        const pending = fixture.store.write('cold-1', source)
        source[0] = 99

        await pending
        expect(fixture.cas.prepare).toHaveBeenCalledWith(Uint8Array.of(4, 5, 6))
        expect(fixture.current()).toEqual(alias({
            objectHash: 'cd'.repeat(32),
            metadata: { source: 'fixture' },
        }))
    })

    it('retries a revision conflict without preparing the immutable bytes again', async () => {
        const fixture = harness()
        fixture.catalog.commitColdAlias
            .mockRejectedValueOnce(new RevisionConflictError(4, 5))
            .mockImplementationOnce(async (next: ColdAlias) => {
                return { revision: 6, value: next }
            })

        await fixture.store.write('cold-1', Uint8Array.of(1, 2, 3))

        expect(fixture.cas.prepare).toHaveBeenCalledTimes(1)
        expect(fixture.catalog.commitColdAlias).toHaveBeenCalledTimes(2)
    })

    it('lists and removes only revisioned aliases without enumerating or deleting payload files', async () => {
        const fixture = harness(alias())

        expect(await fixture.store.list()).toEqual(['cold-1'])
        expect(fixture.legacy.list).not.toHaveBeenCalled()
        await fixture.store.remove('cold-1')
        expect(fixture.current()).toBeUndefined()
        expect(fixture.legacy.remove).not.toHaveBeenCalled()
        expect(fixture.objects.get('ab'.repeat(32))).toEqual(Uint8Array.of(1, 2, 3))
    })

    it.each(['', 'nul\0key'])('rejects an invalid logical key %j before storage work', async (key) => {
        const fixture = harness()

        await expect(fixture.store.write(key, Uint8Array.of(1))).rejects.toThrow(TypeError)
        expect(fixture.cas.prepare).not.toHaveBeenCalled()
    })
})
