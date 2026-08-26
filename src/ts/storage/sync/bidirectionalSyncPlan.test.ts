import { describe, expect, it } from 'vitest'

import { encodeLogicalRecordKey } from './logicalRecordKey'
import {
    LOGICAL_MANIFEST_SCHEMA,
    hashLogicalManifest,
    type LogicalManifest,
    type LogicalManifestRecord,
} from './logicalManifest'
import { planBidirectionalSync } from './bidirectionalSyncPlan'

const hashes = {
    baseRoot: '11'.repeat(32),
    localRoot: '22'.repeat(32),
    remoteRoot: '33'.repeat(32),
    localPreset: '44'.repeat(32),
    remoteCharacter: '55'.repeat(32),
    sharedDependency: '66'.repeat(32),
}

const keys = {
    root: encodeLogicalRecordKey({ kind: 'root' }),
    preset: encodeLogicalRecordKey({ kind: 'preset', presetId: 'preset-a' }),
    character: encodeLogicalRecordKey({ kind: 'character', characterId: 'character-a' }),
}

function live(
    key: string,
    objectHash: string,
    dependencies: string[] = [],
): LogicalManifestRecord {
    return { key, state: 'live', objectHash, dependencies: [...dependencies].sort() }
}

function tombstone(key: string, sequence: string): LogicalManifestRecord {
    return { key, state: 'tombstone', deletedGenerationSequence: sequence }
}

function manifest(input: {
    generation: string
    sequence: string
    sourceRevision: number
    records: LogicalManifestRecord[]
    parentGeneration?: string | null
    sizes?: Record<string, number>
}): LogicalManifest {
    const objectHashes = new Set<string>()
    for (const record of input.records) {
        if (record.state === 'tombstone') continue
        objectHashes.add(record.objectHash)
        for (const dependency of record.dependencies) objectHashes.add(dependency)
    }
    return {
        schema: LOGICAL_MANIFEST_SCHEMA,
        libraryId: 'library-a',
        generation: input.generation,
        generationSequence: input.sequence,
        parentGeneration: input.parentGeneration ?? null,
        sourceRevision: input.sourceRevision,
        records: [...input.records].sort((left, right) => left.key.localeCompare(right.key)),
        objects: [...objectHashes]
            .sort()
            .map((hash) => ({ hash, size: input.sizes?.[hash] ?? 10 })),
    }
}

async function plan(input: {
    base: LogicalManifest
    local: LogicalManifest
    remote: LogicalManifest
    expectedLocalRevision?: number
    expectedRemoteGeneration?: string
}) {
    return planBidirectionalSync({
        baseManifestHash: await hashLogicalManifest(input.base),
        base: input.base,
        local: input.local,
        remote: input.remote,
        expectedLocalRevision: input.expectedLocalRevision ?? input.local.sourceRevision,
        expectedRemoteGeneration: input.expectedRemoteGeneration ?? input.remote.generation,
    })
}

describe('planBidirectionalSync', () => {
    it('returns explicit stale preconditions before producing mutations', async () => {
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: [live(keys.root, hashes.baseRoot)],
        })
        const local = manifest({
            generation: 'local',
            sequence: '2',
            sourceRevision: 2,
            records: [live(keys.root, hashes.baseRoot)],
        })
        const remote = manifest({
            generation: 'remote',
            sequence: '2',
            sourceRevision: 8,
            records: [live(keys.root, hashes.baseRoot)],
        })

        await expect(plan({
            base,
            local,
            remote,
            expectedLocalRevision: 1,
            expectedRemoteGeneration: 'older-remote',
        })).resolves.toEqual({
            kind: 'stale',
            expectedLocalRevision: 1,
            actualLocalRevision: 2,
            expectedRemoteGeneration: 'older-remote',
            actualRemoteGeneration: 'remote',
            failures: ['remote-generation', 'local-revision'],
            contentBytes: 0,
        })
    })

    it('plans disjoint local and remote record changes in opposite directions', async () => {
        const baseRecords = [
            live(keys.root, hashes.baseRoot),
            live(keys.preset, hashes.sharedDependency),
            live(keys.character, hashes.sharedDependency),
        ]
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: baseRecords,
        })
        const local = manifest({
            generation: 'local',
            sequence: '2',
            sourceRevision: 2,
            records: [
                live(keys.root, hashes.baseRoot),
                live(keys.preset, hashes.localPreset),
                live(keys.character, hashes.sharedDependency),
            ],
            sizes: { [hashes.localPreset]: 13 },
        })
        const remote = manifest({
            generation: 'remote',
            sequence: '3',
            sourceRevision: 9,
            records: [
                live(keys.root, hashes.baseRoot),
                live(keys.preset, hashes.sharedDependency),
                live(keys.character, hashes.remoteCharacter),
            ],
            sizes: { [hashes.remoteCharacter]: 17 },
        })

        const result = await plan({ base, local, remote })

        expect(result).toMatchObject({
            kind: 'ready',
            expectedLocalRevision: 2,
            expectedRemoteGeneration: 'remote',
            localApply: [{
                type: 'put',
                key: keys.character,
                objectHash: hashes.remoteCharacter,
                dependencies: [],
            }],
            remoteApply: [{
                type: 'put',
                key: keys.preset,
                objectHash: hashes.localPreset,
                dependencies: [],
            }],
            uploadObjects: [{ hash: hashes.localPreset, size: 13 }],
            downloadObjects: [{ hash: hashes.remoteCharacter, size: 17 }],
            contentBytes: { upload: 13, download: 17, total: 30 },
            isNoOp: false,
        })
    })

    it('reports same-record and delete-versus-edit conflicts without apply operations', async () => {
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: [
                live(keys.root, hashes.baseRoot),
                live(keys.preset, hashes.sharedDependency),
            ],
        })
        const local = manifest({
            generation: 'local',
            sequence: '2',
            sourceRevision: 2,
            records: [
                live(keys.root, hashes.localRoot),
                tombstone(keys.preset, '2'),
            ],
        })
        const remote = manifest({
            generation: 'remote',
            sequence: '3',
            sourceRevision: 7,
            records: [
                live(keys.root, hashes.remoteRoot),
                live(keys.preset, hashes.localPreset),
            ],
        })

        const result = await plan({ base, local, remote })

        expect(result).toMatchObject({
            kind: 'conflict',
            conflicts: [
                { key: keys.preset, type: 'delete-vs-edit' },
                { key: keys.root, type: 'same-record' },
            ].sort((left, right) => left.key.localeCompare(right.key)),
            replacementAllowed: false,
            requiredBackup: 'complete-lossless-package',
            contentBytes: 0,
        })
        expect(result).not.toHaveProperty('localApply')
        expect(result).not.toHaveProperty('remoteApply')
    })

    it('treats divergent tombstone generations as an explicit same-record conflict', async () => {
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: [live(keys.preset, hashes.baseRoot)],
        })
        const local = manifest({
            generation: 'local',
            sequence: '2',
            sourceRevision: 2,
            records: [tombstone(keys.preset, '2')],
        })
        const remote = manifest({
            generation: 'remote',
            sequence: '3',
            sourceRevision: 8,
            records: [tombstone(keys.preset, '3')],
        })

        await expect(plan({ base, local, remote })).resolves.toMatchObject({
            kind: 'conflict',
            conflicts: [{ key: keys.preset, type: 'same-record' }],
            replacementAllowed: false,
            contentBytes: 0,
        })
    })

    it('requires both descendants to retain or tombstone a live common-base record', async () => {
        const liveBase = manifest({
            generation: 'live-base',
            sequence: '1',
            sourceRevision: 1,
            records: [live(keys.preset, hashes.baseRoot)],
        })
        const retained = manifest({
            generation: 'live-retained',
            sequence: '2',
            sourceRevision: 2,
            records: liveBase.records,
        })
        const omitted = manifest({
            generation: 'live-omitted',
            sequence: '2',
            sourceRevision: 2,
            records: [],
        })

        await expect(plan({ base: liveBase, local: omitted, remote: retained }))
            .rejects.toThrow(`base record ${keys.preset}`)
        await expect(plan({ base: liveBase, local: retained, remote: omitted }))
            .rejects.toThrow(`base record ${keys.preset}`)
    })

    it('requires both descendants to retain a common-base tombstone', async () => {
        const base = manifest({
            generation: 'tombstone-base',
            sequence: '1',
            sourceRevision: 1,
            records: [tombstone(keys.preset, '1')],
        })
        const retained = manifest({
            generation: 'tombstone-retained',
            sequence: '2',
            sourceRevision: 2,
            records: base.records,
        })
        const omitted = manifest({
            generation: 'tombstone-omitted',
            sequence: '2',
            sourceRevision: 2,
            records: [],
        })

        await expect(plan({ base, local: omitted, remote: retained }))
            .rejects.toThrow(`retain tombstone ${keys.preset}`)
        await expect(plan({ base, local: retained, remote: omitted }))
            .rejects.toThrow(`retain tombstone ${keys.preset}`)
    })

    it.each(['local', 'remote'] as const)(
        'rejects resurrection of a common-base tombstone by the %s descendant',
        async (side) => {
            const base = manifest({
                generation: 'tombstone-base',
                sequence: '1',
                sourceRevision: 1,
                records: [tombstone(keys.preset, '1')],
            })
            const local = manifest({
                generation: 'local',
                sequence: '2',
                sourceRevision: 2,
                records: side === 'local'
                    ? [live(keys.preset, hashes.localRoot)]
                    : base.records,
            })
            const remote = manifest({
                generation: 'remote',
                sequence: '2',
                sourceRevision: 8,
                records: side === 'remote'
                    ? [live(keys.preset, hashes.remoteRoot)]
                    : base.records,
            })

            await expect(plan({ base, local, remote }))
                .rejects.toThrow(`retain tombstone ${keys.preset}`)
        },
    )

    it('rejects one generation ID bound to different manifest hashes', async () => {
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: [live(keys.root, hashes.baseRoot)],
        })
        const local = manifest({
            generation: 'reused-generation',
            sequence: '2',
            sourceRevision: 2,
            records: [live(keys.root, hashes.localRoot)],
        })
        const remote = manifest({
            generation: 'reused-generation',
            sequence: '3',
            sourceRevision: 8,
            records: [live(keys.root, hashes.remoteRoot)],
        })

        await expect(plan({ base, local, remote }))
            .rejects.toThrow('generation ID is bound to different manifest hashes')
    })

    it('rejects reuse of the common-base generation ID at a later sequence', async () => {
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: [live(keys.root, hashes.baseRoot)],
        })
        const local = manifest({
            generation: 'base',
            sequence: '2',
            sourceRevision: 2,
            records: [live(keys.root, hashes.localRoot)],
        })
        const remote = manifest({
            generation: 'remote',
            sequence: '2',
            sourceRevision: 8,
            records: [live(keys.root, hashes.baseRoot)],
        })

        await expect(plan({ base, local, remote }))
            .rejects.toThrow('generation ID is bound to different manifest hashes')
    })

    it('accounts identical logical state as a zero-content no-op', async () => {
        const records = [
            live(keys.root, hashes.baseRoot, [hashes.sharedDependency]),
            tombstone(keys.preset, '1'),
        ]
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records,
        })
        const local = manifest({
            generation: 'local',
            sequence: '2',
            sourceRevision: 2,
            records,
        })
        const remote = manifest({
            generation: 'remote',
            sequence: '3',
            sourceRevision: 8,
            records,
        })

        await expect(plan({ base, local, remote })).resolves.toMatchObject({
            kind: 'ready',
            localApply: [],
            remoteApply: [],
            uploadObjects: [],
            downloadObjects: [],
            contentBytes: { upload: 0, download: 0, total: 0 },
            isNoOp: true,
        })
    })

    it('rejects a common-base hash that does not identify the supplied base', async () => {
        const base = manifest({
            generation: 'base',
            sequence: '1',
            sourceRevision: 1,
            records: [live(keys.root, hashes.baseRoot)],
        })

        await expect(planBidirectionalSync({
            baseManifestHash: 'ff'.repeat(32),
            base,
            local: base,
            remote: base,
            expectedLocalRevision: 1,
            expectedRemoteGeneration: 'base',
        })).rejects.toThrow('common base')
    })
})
