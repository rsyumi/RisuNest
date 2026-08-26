import { describe, expect, it } from 'vitest'

import {
    planLogicalDeltaPull,
    type LogicalDeltaPullPlan,
} from './logicalDeltaPull'
import {
    hashLogicalManifest,
    type LogicalManifest,
    type LogicalManifestRecord,
} from './logicalManifest'

const hashes = {
    empty: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
    base: '1'.repeat(64),
    local: '2'.repeat(64),
    remote: '3'.repeat(64),
    payloadA: '4'.repeat(64),
    payloadB: '5'.repeat(64),
}

const keys = {
    root: 'r1:root',
    preset: 'r1:preset:WyIwIl0',
    character: 'r1:character:WyJjaGFyYWN0ZXItMSJd',
}

function live(key: string, objectHash: string, dependencies: string[] = []): LogicalManifestRecord {
    return { key, state: 'live', objectHash, dependencies }
}

function tombstone(key: string, sequence: string): LogicalManifestRecord {
    return { key, state: 'tombstone', deletedGenerationSequence: sequence }
}

function manifest(
    generation: string,
    sequence: string,
    records: LogicalManifestRecord[],
    sourceRevision: number,
): LogicalManifest {
    const objectHashes = new Set<string>()
    for (const record of records) {
        if (record.state === 'tombstone') continue
        objectHashes.add(record.objectHash)
        for (const dependency of record.dependencies) objectHashes.add(dependency)
    }
    return {
        schema: 'risunest.logical-manifest/v1',
        libraryId: 'library-1',
        generation,
        generationSequence: sequence,
        parentGeneration: sequence === '1' ? null : `generation-${Number(sequence) - 1}`,
        sourceRevision,
        records: [...records].sort((left, right) => left.key < right.key ? -1 : 1),
        objects: [...objectHashes].sort().map((hash) => ({ hash, size: 8 })),
    }
}

async function plan(input: {
    base: LogicalManifest
    local: LogicalManifest
    remote: LogicalManifest
    expectedLocalRevision?: number
}): Promise<LogicalDeltaPullPlan> {
    return planLogicalDeltaPull({
        baseManifestHash: await hashLogicalManifest(input.base),
        expectedLocalRevision: input.expectedLocalRevision ?? input.local.sourceRevision,
        ...input,
    })
}

describe('logical delta pull planner', () => {
    it('returns a zero-content no-op for identical logical state', async () => {
        const sharedRecords = [live(keys.root, hashes.base, [hashes.payloadA])]
        const base = manifest('generation-1', '1', sharedRecords, 1)
        const local = manifest('local-4', '4', sharedRecords, 4)
        const remote = manifest('generation-1', '1', sharedRecords, 1)
        const baseHash = await hashLogicalManifest(base)

        await expect(plan({ base, local, remote })).resolves.toEqual({
            kind: 'ready',
            expectedLocalRevision: 4,
            expectedBaseManifestHash: baseHash,
            expectedRemoteGeneration: 'generation-1',
            nextBaseGenerationSequence: '1',
            apply: [],
            preserveLocalKeys: [],
            candidateObjectHashes: [],
            nextBaseManifestHash: await hashLogicalManifest(remote),
        })
    })

    it('applies remote-only puts and deletes while preserving disjoint local changes', async () => {
        const base = manifest('generation-1', '1', [
            live(keys.root, hashes.base),
            live(keys.preset, hashes.base),
        ], 1)
        const local = manifest('local-7', '7', [
            live(keys.root, hashes.local),
            live(keys.preset, hashes.base),
        ], 7)
        const remote = manifest('generation-2', '2', [
            live(keys.root, hashes.base),
            tombstone(keys.preset, '2'),
            live(keys.character, hashes.remote, [hashes.payloadB, hashes.payloadA].sort()),
        ], 12)

        const result = await plan({ base, local, remote })

        expect(result).toEqual({
            kind: 'ready',
            expectedLocalRevision: 7,
            expectedBaseManifestHash: await hashLogicalManifest(base),
            expectedRemoteGeneration: 'generation-2',
            nextBaseGenerationSequence: '2',
            apply: [
                {
                    type: 'put',
                    key: keys.character,
                    objectHash: hashes.remote,
                    dependencies: [hashes.payloadA, hashes.payloadB],
                },
                {
                    type: 'delete',
                    key: keys.preset,
                    deletedGenerationSequence: '2',
                },
            ],
            preserveLocalKeys: [keys.root],
            candidateObjectHashes: [
                hashes.remote,
                hashes.payloadA,
                hashes.payloadB,
            ].sort(),
            nextBaseManifestHash: await hashLogicalManifest(remote),
        })
    })

    it('treats identical changes on both sides as converged', async () => {
        const base = manifest('generation-1', '1', [live(keys.root, hashes.base)], 1)
        const local = manifest('local-2', '2', [live(keys.root, hashes.remote)], 2)
        const remote = manifest('generation-2', '2', [live(keys.root, hashes.remote)], 8)

        const result = await plan({ base, local, remote })

        expect(result.kind).toBe('ready')
        if (result.kind === 'ready') {
            expect(result.apply).toEqual([])
            expect(result.preserveLocalKeys).toEqual([])
            expect(result.candidateObjectHashes).toEqual([])
        }
    })

    it('reports divergent tombstone generations as a conflict', async () => {
        const base = manifest('generation-1', '1', [live(keys.preset, hashes.base)], 1)
        const local = manifest('local-2', '2', [tombstone(keys.preset, '2')], 2)
        const remote = manifest('generation-3', '3', [tombstone(keys.preset, '3')], 8)

        await expect(plan({ base, local, remote })).resolves.toEqual({
            kind: 'conflict',
            expectedLocalRevision: 2,
            expectedBaseManifestHash: await hashLogicalManifest(base),
            conflicts: [{ key: keys.preset, type: 'delete-edit' }],
        })
    })

    it.each(['local', 'remote'] as const)(
        'rejects omission of a common-base tombstone from the %s descendant',
        async (side) => {
            const base = manifest('generation-1', '1', [tombstone(keys.preset, '1')], 1)
            const local = manifest('local-2', '2', [tombstone(keys.preset, '1')], 2)
            const remote = manifest('generation-2', '2', [tombstone(keys.preset, '1')], 8)
            if (side === 'local') local.records = []
            else remote.records = []

            await expect(plan({ base, local, remote })).rejects.toThrow('retain tombstone')
        },
    )

    it.each(['local', 'remote'] as const)(
        'rejects resurrection of a common-base tombstone by the %s descendant',
        async (side) => {
            const base = manifest('generation-1', '1', [tombstone(keys.preset, '1')], 1)
            const local = manifest('local-2', '2', [tombstone(keys.preset, '1')], 2)
            const remote = manifest('generation-2', '2', [tombstone(keys.preset, '1')], 8)
            const resurrected = live(keys.preset, hashes.local)
            const descendant = side === 'local' ? local : remote
            descendant.records = [resurrected]
            descendant.objects = [{ hash: hashes.local, size: 8 }]

            await expect(plan({ base, local, remote })).rejects.toThrow('retain tombstone')
        },
    )

    it('rejects a reused remote generation ID with different content', async () => {
        const base = manifest('shared-generation', '1', [live(keys.root, hashes.base)], 1)
        const local = manifest('local-2', '2', [live(keys.root, hashes.base)], 2)
        const remote = manifest('shared-generation', '2', [live(keys.root, hashes.remote)], 8)

        await expect(plan({ base, local, remote })).rejects.toThrow('generation ID')
    })

    it('rejects a reused local generation ID with different content', async () => {
        const base = manifest('shared-generation', '1', [live(keys.root, hashes.base)], 1)
        const local = manifest('shared-generation', '2', [live(keys.root, hashes.local)], 2)
        const remote = manifest('remote-2', '2', [live(keys.root, hashes.base)], 8)

        await expect(plan({ base, local, remote })).rejects.toThrow('generation ID')
    })

    it('rejects one generation ID reused for divergent local and remote content', async () => {
        const base = manifest('generation-1', '1', [live(keys.root, hashes.base)], 1)
        const local = manifest('shared-generation', '2', [live(keys.root, hashes.local)], 2)
        const remote = manifest('shared-generation', '3', [live(keys.root, hashes.remote)], 8)

        await expect(plan({ base, local, remote })).rejects.toThrow('generation ID')
    })

    it.each(['local', 'remote'] as const)(
        'rejects a common-base generation ID reused at a different sequence by %s',
        async (side) => {
            const records = [live(keys.root, hashes.base)]
            const base = manifest('shared-generation', '1', records, 1)
            const local = manifest('local-2', '2', records, 2)
            const remote = manifest('remote-2', '2', records, 8)
            if (side === 'local') local.generation = base.generation
            else remote.generation = base.generation

            await expect(plan({ base, local, remote })).rejects.toThrow('generation ID')
        },
    )

    it('transfers an absent zero-byte object instead of classifying it as a no-op', async () => {
        const base = manifest('generation-1', '1', [], 1)
        const local = manifest('local-2', '2', [], 2)
        const remote = manifest('generation-2', '2', [live(keys.root, hashes.empty)], 8)
        remote.objects[0].size = 0

        const result = await plan({ base, local, remote })

        expect(result.kind).toBe('ready')
        if (result.kind === 'ready') {
            expect(result.apply).toEqual([{
                type: 'put',
                key: keys.root,
                objectHash: hashes.empty,
                dependencies: [],
            }])
            expect(result.candidateObjectHashes).toEqual([hashes.empty])
        }
    })

    it('returns the full changed-record graph for target-side transfer selection', async () => {
        const sharedCharacter = live(keys.character, hashes.remote, [hashes.payloadA])
        const base = manifest('generation-1', '1', [
            live(keys.root, hashes.base),
            sharedCharacter,
        ], 1)
        const local = manifest('local-2', '2', [
            live(keys.root, hashes.base),
            sharedCharacter,
        ], 2)
        const remote = manifest('generation-3', '3', [
            live(keys.root, hashes.remote, [hashes.payloadA, hashes.payloadB].sort()),
            sharedCharacter,
        ], 8)

        const result = await plan({ base, local, remote })

        expect(result.kind).toBe('ready')
        if (result.kind === 'ready') {
            expect(result.apply).toEqual([{
                type: 'put',
                key: keys.root,
                objectHash: hashes.remote,
                dependencies: [hashes.payloadA, hashes.payloadB].sort(),
            }])
            expect(result.candidateObjectHashes).toEqual([
                hashes.remote,
                hashes.payloadA,
                hashes.payloadB,
            ].sort())
        }
    })

    it('treats different tombstone sequences as divergent record identities', async () => {
        const base = manifest('generation-1', '1', [tombstone(keys.preset, '1')], 1)
        const local = manifest('local-2', '2', [tombstone(keys.preset, '2')], 2)
        const remote = manifest('generation-3', '3', [tombstone(keys.preset, '3')], 3)

        const result = await plan({ base, local, remote })

        expect(result.kind).toBe('conflict')
        if (result.kind === 'conflict') {
            expect(result.conflicts).toEqual([{ key: keys.preset, type: 'delete-edit' }])
        }
    })

    it('reports sorted live-live and delete-edit conflicts without a partial plan', async () => {
        const base = manifest('generation-1', '1', [
            live(keys.root, hashes.base),
            live(keys.preset, hashes.base),
        ], 1)
        const local = manifest('local-3', '3', [
            live(keys.root, hashes.local),
            tombstone(keys.preset, '3'),
        ], 3)
        const remote = manifest('generation-2', '2', [
            live(keys.root, hashes.remote),
            live(keys.preset, hashes.remote),
            live(keys.character, hashes.remote),
        ], 9)

        await expect(plan({ base, local, remote })).resolves.toEqual({
            kind: 'conflict',
            expectedLocalRevision: 3,
            expectedBaseManifestHash: await hashLogicalManifest(base),
            conflicts: [
                { key: keys.preset, type: 'delete-edit' },
                { key: keys.root, type: 'live-live' },
            ].sort((left, right) => left.key < right.key ? -1 : 1),
        })
    })

    it.each(['local', 'remote'] as const)(
        'rejects implicit deletion of a base-live record by the %s descendant',
        async (side) => {
            const baseRecord = live(keys.root, hashes.base)
            const base = manifest('generation-1', '1', [baseRecord], 1)
            const local = manifest('local-2', '2', [baseRecord], 2)
            const remote = manifest('generation-2', '2', [baseRecord], 8)
            if (side === 'local') {
                local.records = []
                local.objects = []
            } else {
                remote.records = []
                remote.objects = []
            }

            await expect(plan({ base, local, remote })).rejects.toThrow('tombstone')
        },
    )

    it('rejects a stale or mismatched common-base identity before planning', async () => {
        const base = manifest('generation-1', '1', [], 1)
        const local = manifest('local-1', '1', [], 4)
        const remote = manifest('generation-2', '2', [], 8)

        await expect(planLogicalDeltaPull({
            baseManifestHash: 'f'.repeat(64),
            base,
            local,
            remote,
            expectedLocalRevision: 4,
        })).rejects.toThrow('base manifest hash')
        await expect(plan({ base, local, remote, expectedLocalRevision: 3 })).rejects.toThrow(
            'local revision',
        )
    })

    it('rejects different libraries and inconsistent sizes for the same object hash', async () => {
        const base = manifest('generation-1', '1', [live(keys.root, hashes.base)], 1)
        const local = manifest('local-1', '1', [live(keys.root, hashes.base)], 1)
        const remoteLibrary = manifest('generation-2', '2', [live(keys.root, hashes.base)], 2)
        remoteLibrary.libraryId = 'another-library'
        await expect(plan({ base, local, remote: remoteLibrary })).rejects.toThrow(
            'different libraries',
        )

        const remoteSize = manifest('generation-2', '2', [live(keys.root, hashes.base)], 2)
        remoteSize.objects[0].size = 9
        await expect(plan({ base, local, remote: remoteSize })).rejects.toThrow('object size')
    })
})
