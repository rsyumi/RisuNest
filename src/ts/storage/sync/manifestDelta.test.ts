import { describe, expect, it, vi } from 'vitest'

import {
    planManifestDelta,
    publishManifestDelta,
    type ManifestRecord,
    type SyncManifest,
} from './manifestDelta'

const baseRecord: ManifestRecord = {
    revision: 1,
    tombstone: false,
    hash: 'record-a-v1',
    size: 10,
    blobHashes: ['blob-b', 'blob-a'],
}

function manifest(
    generation: string,
    records: SyncManifest['records'],
    blobs: SyncManifest['blobs'] = {},
): SyncManifest {
    return { generation, records, blobs }
}

describe('planManifestDelta', () => {
    it('produces an empty deterministic commit for identical manifests', () => {
        const shared = manifest('shared', { 'record-a': baseRecord }, {
            'blob-a': { size: 2 },
            'blob-b': { size: 3 },
        })

        expect(planManifestDelta({ base: shared, local: shared, remote: shared })).toEqual({
            kind: 'ready',
            commit: {
                expectedGeneration: 'shared',
                uploadRecordKeys: [],
                uploadBlobHashes: [],
                tombstoneKeys: [],
                nextRecords: {
                    'record-a': {
                        revision: 1,
                        tombstone: false,
                        hash: 'record-a-v1',
                        size: 10,
                        blobHashes: ['blob-a', 'blob-b'],
                    },
                },
                nextBlobs: {
                    'blob-a': { size: 2 },
                    'blob-b': { size: 3 },
                },
            },
        })
    })

    it('uploads one locally changed record and only its missing referenced blobs', () => {
        const changed: ManifestRecord = {
            revision: 2,
            tombstone: false,
            hash: 'record-a-v2',
            size: 12,
            blobHashes: ['blob-c', 'blob-a'],
        }
        const base = manifest('base', { 'record-a': baseRecord }, {
            'blob-a': { size: 2 },
            'blob-b': { size: 3 },
        })
        const local = manifest('local', { 'record-a': changed }, {
            'blob-a': { size: 2 },
            'blob-c': { size: 4 },
        })
        const remote = manifest('remote-7', { 'record-a': baseRecord }, {
            'blob-a': { size: 2 },
            'blob-b': { size: 3 },
        })

        expect(planManifestDelta({ base, local, remote })).toEqual({
            kind: 'ready',
            commit: {
                expectedGeneration: 'remote-7',
                uploadRecordKeys: ['record-a'],
                uploadBlobHashes: ['blob-c'],
                tombstoneKeys: [],
                nextRecords: {
                    'record-a': {
                        revision: 2,
                        tombstone: false,
                        hash: 'record-a-v2',
                        size: 12,
                        blobHashes: ['blob-a', 'blob-c'],
                    },
                },
                nextBlobs: {
                    'blob-a': { size: 2 },
                    'blob-b': { size: 3 },
                    'blob-c': { size: 4 },
                },
            },
        })
    })

    it('rejects a locally changed live record with missing blob metadata', () => {
        const localRecord: ManifestRecord = {
            revision: 1,
            tombstone: false,
            hash: 'local-record',
            size: 5,
            blobHashes: ['blob-missing'],
        }

        expect(() => planManifestDelta({
            base: manifest('base', {}),
            local: manifest('local', { 'record-a': localRecord }),
            remote: manifest('remote', {}),
        })).toThrowError('Manifest delta input is missing metadata for blob: blob-missing')
    })

    it('rejects a preserved remote-only live record with missing blob metadata', () => {
        const remoteRecord: ManifestRecord = {
            revision: 3,
            tombstone: false,
            hash: 'remote-record',
            size: 7,
            blobHashes: ['blob-remote-missing'],
        }

        expect(() => planManifestDelta({
            base: manifest('base', {}),
            local: manifest('local', {}),
            remote: manifest('remote', { 'record-z': remoteRecord }),
        })).toThrowError('Manifest delta input is missing metadata for blob: blob-remote-missing')
    })

    it('uses remote metadata when a local record omits it', () => {
        const changed: ManifestRecord = {
            revision: 2,
            tombstone: false,
            hash: 'record-a-v2',
            size: 12,
            blobHashes: ['blob-shared'],
        }
        const plan = planManifestDelta({
            base: manifest('base', {}),
            local: manifest('local', { 'record-a': changed }),
            remote: manifest('remote', {}, { 'blob-shared': { size: 9 } }),
        })

        expect(plan.kind).toBe('ready')
        if (plan.kind === 'ready') {
            expect(plan.commit.nextBlobs).toEqual({ 'blob-shared': { size: 9 } })
            expect(plan.commit.uploadBlobHashes).toEqual([])
        }
    })

    it('propagates only an explicit local tombstone', () => {
        const base = manifest('base', { 'record-a': baseRecord })
        const local = manifest('local', {
            'record-a': { revision: 2, tombstone: true },
        })
        const remote = manifest('remote', { 'record-a': baseRecord })

        const plan = planManifestDelta({ base, local, remote })

        expect(plan).toEqual({
            kind: 'ready',
            commit: {
                expectedGeneration: 'remote',
                uploadRecordKeys: [],
                uploadBlobHashes: [],
                tombstoneKeys: ['record-a'],
                nextRecords: {
                    'record-a': { revision: 2, tombstone: true },
                },
                nextBlobs: {},
            },
        })
    })

    it('does not infer deletion from an omitted local record', () => {
        const base = manifest('base', { 'record-a': baseRecord })
        const local = manifest('local', {})
        const remote = manifest('remote', { 'record-a': baseRecord }, {
            'blob-a': { size: 2 },
            'blob-b': { size: 3 },
        })

        const plan = planManifestDelta({ base, local, remote })

        expect(plan.kind).toBe('ready')
        if (plan.kind === 'ready') {
            expect(plan.commit.uploadRecordKeys).toEqual([])
            expect(plan.commit.tombstoneKeys).toEqual([])
            expect(plan.commit.nextRecords).toHaveProperty('record-a')
        }
    })

    it('preserves a remote-only change without uploading it', () => {
        const remoteRecord: ManifestRecord = {
            revision: 8,
            tombstone: false,
            hash: 'record-a-remote',
            size: 20,
            blobHashes: ['blob-remote'],
        }
        const base = manifest('base', { 'record-a': baseRecord })
        const local = manifest('local', { 'record-a': baseRecord })
        const remote = manifest('remote', { 'record-a': remoteRecord }, {
            'blob-remote': { size: 9 },
        })

        const plan = planManifestDelta({ base, local, remote })

        expect(plan).toEqual({
            kind: 'ready',
            commit: {
                expectedGeneration: 'remote',
                uploadRecordKeys: [],
                uploadBlobHashes: [],
                tombstoneKeys: [],
                nextRecords: {
                    'record-a': {
                        revision: 8,
                        tombstone: false,
                        hash: 'record-a-remote',
                        size: 20,
                        blobHashes: ['blob-remote'],
                    },
                },
                nextBlobs: { 'blob-remote': { size: 9 } },
            },
        })
    })

    it('merges disjoint local and remote changes in sorted order', () => {
        const base = manifest('base', {})
        const local = manifest('local', {
            'record-z': {
                revision: 1,
                tombstone: false,
                hash: 'local',
                size: 1,
                blobHashes: [],
            },
        })
        const remote = manifest('remote', {
            'record-a': {
                revision: 9,
                tombstone: false,
                hash: 'remote',
                size: 2,
                blobHashes: [],
            },
        })

        const plan = planManifestDelta({ base, local, remote })

        expect(plan.kind).toBe('ready')
        if (plan.kind === 'ready') {
            expect(plan.commit.uploadRecordKeys).toEqual(['record-z'])
            expect(Object.keys(plan.commit.nextRecords)).toEqual(['record-a', 'record-z'])
        }
    })

    it('ignores device-local revision differences for equal record state', () => {
        const base = manifest('base', { 'record-a': baseRecord })
        const local = manifest('local', {
            'record-a': { ...baseRecord, revision: 20 },
        })
        const remote = manifest('remote', {
            'record-a': { ...baseRecord, revision: 30 },
        }, {
            'blob-a': { size: 2 },
            'blob-b': { size: 3 },
        })

        const plan = planManifestDelta({ base, local, remote })

        expect(plan.kind).toBe('ready')
        if (plan.kind === 'ready') {
            expect(plan.commit.uploadRecordKeys).toEqual([])
            expect(plan.commit.tombstoneKeys).toEqual([])
        }
    })

    it('compares the complete sorted blob hash list', () => {
        const base = manifest('base', { 'record-a': baseRecord }, {
            'blob-a': { size: 2 },
            'blob-b': { size: 3 },
        })
        const local = manifest('local', {
            'record-a': {
                ...baseRecord,
                revision: 2,
                blobHashes: ['blob-a', 'blob-b', 'blob-b'],
            },
        }, base.blobs)
        const remote = manifest('remote', { 'record-a': baseRecord }, base.blobs)

        const plan = planManifestDelta({ base, local, remote })

        expect(plan.kind).toBe('ready')
        if (plan.kind === 'ready') {
            expect(plan.commit.uploadRecordKeys).toEqual(['record-a'])
            expect(plan.commit.nextRecords['record-a']).toEqual({
                revision: 2,
                tombstone: false,
                hash: 'record-a-v1',
                size: 10,
                blobHashes: ['blob-a', 'blob-b', 'blob-b'],
            })
        }
    })

    it('returns sorted conflicts for divergent changes to the same records', () => {
        const base = manifest('base', {
            'record-z': baseRecord,
            'record-a': baseRecord,
        })
        const local = manifest('local', {
            'record-z': { ...baseRecord, revision: 2, hash: 'local-z' },
            'record-a': { ...baseRecord, revision: 2, hash: 'local-a' },
        })
        const remote = manifest('remote', {
            'record-z': { ...baseRecord, revision: 3, hash: 'remote-z' },
            'record-a': { ...baseRecord, revision: 3, hash: 'remote-a' },
        })

        expect(planManifestDelta({ base, local, remote })).toEqual({
            kind: 'conflict',
            keys: ['record-a', 'record-z'],
        })
    })
})

describe('publishManifestDelta', () => {
    it('forwards one compare-and-swap and returns stale unchanged', async () => {
        const plan = planManifestDelta({
            base: manifest('base', {}),
            local: manifest('local', {}),
            remote: manifest('remote-4', {}),
        })
        expect(plan.kind).toBe('ready')
        if (plan.kind !== 'ready') {
            throw new Error('Expected a ready plan')
        }
        const stale = { kind: 'stale', actualGeneration: 'remote-5' } as const
        const compareAndSwap = vi.fn(async () => stale)

        const result = await publishManifestDelta(plan, { compareAndSwap })

        expect(result).toBe(stale)
        expect(compareAndSwap).toHaveBeenCalledOnce()
        expect(compareAndSwap).toHaveBeenCalledWith(plan.commit)
    })
})
