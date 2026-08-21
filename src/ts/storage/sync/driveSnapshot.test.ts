import { describe, expect, it, vi } from 'vitest'

import { createDriveSnapshotAdapter } from './driveSnapshot'

describe('createDriveSnapshotAdapter', () => {
    it('exposes only explicit snapshot operations', async () => {
        const snapshots = [
            { id: 'snapshot-2', createdAt: 2, label: 'Second' },
            { id: 'snapshot-1', createdAt: 1, label: 'First' },
        ] as const
        const operations = {
            createSnapshot: vi.fn(async () => undefined),
            listSnapshots: vi.fn(async () => snapshots),
            restoreSnapshot: vi.fn(async (_id: string) => undefined),
            pushDelta: vi.fn(),
        }

        const adapter = createDriveSnapshotAdapter(operations)

        await adapter.createSnapshot()
        expect(await adapter.listSnapshots()).toBe(snapshots)
        await adapter.restoreSnapshot('snapshot-1')

        expect(operations.createSnapshot).toHaveBeenCalledOnce()
        expect(operations.listSnapshots).toHaveBeenCalledOnce()
        expect(operations.restoreSnapshot).toHaveBeenCalledWith('snapshot-1')
        expect(Object.keys(adapter).sort()).toEqual([
            'capability',
            'createSnapshot',
            'listSnapshots',
            'restoreSnapshot',
        ])
    })

    it('advertises snapshot-only capability values', () => {
        const adapter = createDriveSnapshotAdapter({
            createSnapshot: async () => undefined,
            listSnapshots: async () => [],
            restoreSnapshot: async () => undefined,
        })

        expect(adapter.capability).toEqual({
            kind: 'drive-snapshot',
            operations: ['create', 'list', 'restore'],
            automaticMerge: false,
        })
    })
})
