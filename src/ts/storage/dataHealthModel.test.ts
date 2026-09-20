import { describe, expect, it, vi } from 'vitest'
import type {
    DataHealthResult,
    RepairCandidate,
    RepairPreview,
} from './dataHealth'
import {
    createDataHealthModel,
    type DataHealthDependencies,
} from './dataHealthModel'

function result(
    overrides: Partial<DataHealthResult> = {},
): DataHealthResult {
    return {
        revision: 3,
        scannedAt: 1,
        depth: 'quick',
        counts: { blocking: 0, degraded: 0, informational: 0 },
        items: [],
        omitted: 0,
        ...overrides,
    }
}

function deepPage(
    completedObjects: number,
    complete: boolean,
): DataHealthResult {
    return result({
        depth: 'deep',
        deep: {
            cursor: `hash-${completedObjects}`,
            completedObjects,
            totalObjects: 3,
            completedBytes: completedObjects,
            totalBytes: 3,
            complete,
        },
    })
}

function preview(selection: string[]): RepairPreview {
    return {
        selected: candidates().filter((candidate) => selection.includes(candidate.id)),
        answered: selection.length,
        remaining: 0,
        droppedReferences: selection.length,
        droppedAliases: 0,
        discarding: [],
        tables: ['characters'],
        proposesSnapshot: false,
    }
}

function candidates(): RepairCandidate[] {
    return [
        {
            id: '0:drop-reference',
            action: {
                action: 'drop-reference',
                owner: { kind: 'character', id: 'char-1' },
                sourcePath: '$.image',
                occurrence: 0,
            },
            finding: 0,
            preferred: true,
            discards: true,
        },
        {
            id: '0:adopt-stored-payload',
            action: { action: 'adopt-stored-payload', kind: 'asset', key: 'assets/a.png' },
            finding: 0,
            preferred: false,
            discards: false,
        },
    ]
}

function harness(overrides: Partial<DataHealthDependencies> = {}) {
    const deps: DataHealthDependencies = {
        getResult: vi.fn().mockResolvedValue(null),
        scan: vi.fn().mockResolvedValue(result()),
        deepScan: vi.fn().mockResolvedValue(deepPage(3, true)),
        cancel: vi.fn().mockResolvedValue(undefined),
        planRepair: vi.fn().mockResolvedValue(candidates()),
        previewRepair: vi.fn(async (selection: string[]) => preview(selection)),
        applyRepair: vi.fn().mockResolvedValue({ result: result() }),
        listJournals: vi.fn().mockResolvedValue([]),
        undoRepair: vi.fn().mockResolvedValue({ result: result(), skipped: [] }),
        ...overrides,
    }
    return { deps, model: createDataHealthModel(deps) }
}

describe('createDataHealthModel', () => {
    it('shows the last diagnosis without scanning again', async () => {
        const stored = result({
            items: [
                {
                    code: 'record-invalid',
                    severity: 'blocking',
                    owner: { kind: 'character', id: 'char-1' },
                    locator: null,
                    target: null,
                    detail: 'record JSON is invalid',
                },
            ],
            counts: { blocking: 1, degraded: 0, informational: 0 },
        })
        const { deps, model } = harness({
            getResult: vi.fn().mockResolvedValue(stored),
        })
        await model.load()
        expect(deps.scan).not.toHaveBeenCalled()
        expect(model.snapshot().groups).toHaveLength(1)
        expect(model.snapshot().result).toEqual(stored)
    })

    it('keeps asking for deep pages until the pass reports it finished', async () => {
        const deepScan = vi
            .fn()
            .mockResolvedValueOnce(deepPage(0, false))
            .mockResolvedValueOnce(deepPage(2, false))
            .mockResolvedValueOnce(deepPage(3, true))
        const { model } = harness({ deepScan })
        await model.deepScan(false)
        expect(deepScan.mock.calls.map(([resume]) => resume)).toEqual([
            false,
            true,
            true,
        ])
        expect(model.snapshot().running).toBeNull()
        expect(model.snapshot().resumable).toBe(false)
        expect(model.snapshot().deepFraction).toBe(1)
    })

    it('stops between pages when the screen cancels, and offers to continue', async () => {
        const deepScan = vi.fn(async () => deepPage(1, false))
        const cancel = vi.fn(async () => {})
        const { model } = harness({ deepScan, cancel })
        const running = model.deepScan(false)
        await Promise.resolve()
        await model.cancel()
        await running
        expect(cancel).toHaveBeenCalledOnce()
        expect(deepScan.mock.calls.length).toBeLessThanOrEqual(2)
        expect(model.snapshot().running).toBeNull()
        expect(model.snapshot().resumable).toBe(true)
    })

    it('ends quietly when the native scan reports the stop it was asked for', async () => {
        const deepScan = vi
            .fn()
            .mockRejectedValue({ message: 'data-health-scan-cancelled' })
        const { model } = harness({ deepScan })
        await expect(model.deepScan(false)).resolves.toBeUndefined()
        expect(model.snapshot().failed).toBe(false)
        expect(model.snapshot().running).toBeNull()
    })

    it('reports a real failure to the caller and marks the screen', async () => {
        const { model } = harness({
            scan: vi.fn().mockRejectedValue(new Error('disk is full')),
        })
        await expect(model.quickScan()).rejects.toThrow('disk is full')
        expect(model.snapshot().failed).toBe(true)
        expect(model.snapshot().running).toBeNull()
    })

    it('refuses a second scan while one is running', async () => {
        let release = () => {}
        const scan = vi.fn(
            () =>
                new Promise<DataHealthResult>((resolve) => {
                    release = () => resolve(result())
                }),
        )
        const { deps, model } = harness({ scan })
        const running = model.quickScan()
        await Promise.resolve()
        await model.deepScan(false)
        expect(deps.deepScan).not.toHaveBeenCalled()
        release()
        await running
        expect(scan).toHaveBeenCalledOnce()
    })
})

describe('repairing from the model', () => {
    const damaged = result({
        items: [
            {
                code: 'reference-missing',
                severity: 'degraded',
                owner: { kind: 'character', id: 'char-1' },
                locator: { sourcePath: '$.image', occurrence: 0 },
                target: { kind: 'asset', key: 'assets/a.png' },
                detail: 'reference has no target in this library',
            },
        ],
        counts: { blocking: 0, degraded: 1, informational: 0 },
    })

    it('preselects the fixed choice and previews it', async () => {
        const { deps, model } = harness({
            getResult: vi.fn().mockResolvedValue(damaged),
        })
        await model.load()
        await model.loadRepairs()
        expect(model.snapshot().selection).toEqual(['0:drop-reference'])
        expect(deps.previewRepair).toHaveBeenCalledWith(['0:drop-reference'])
        expect(model.snapshot().preview?.answered).toBe(1)
    })

    it('replaces the other answer to the same finding rather than adding to it', async () => {
        const { model } = harness({ getResult: vi.fn().mockResolvedValue(damaged) })
        await model.load()
        await model.loadRepairs()
        await model.toggle('0:adopt-stored-payload')
        expect(model.snapshot().selection).toEqual(['0:adopt-stored-payload'])
        await model.toggle('0:adopt-stored-payload')
        expect(model.snapshot().selection).toEqual([])
        expect(model.snapshot().preview).toBeNull()
    })

    it('applies the selection and shows the diagnosis of the repaired library', async () => {
        const repaired = result({ revision: 4 })
        const applyRepair = vi.fn().mockResolvedValue({ result: repaired })
        const listJournals = vi.fn().mockResolvedValue([
            {
                id: 'repair-1',
                createdAt: 2,
                fromRevision: 3,
                toRevision: 4,
                changes: 1,
                heldObjects: 1,
                current: true,
            },
        ])
        const { model } = harness({
            getResult: vi.fn().mockResolvedValue(damaged),
            applyRepair,
            listJournals,
        })
        await model.load()
        await model.loadRepairs()
        await model.apply(true)
        expect(applyRepair).toHaveBeenCalledWith(['0:drop-reference'], true)
        expect(model.snapshot().result).toEqual(repaired)
        expect(model.snapshot().journals).toHaveLength(1)
        expect(model.snapshot().repairing).toBe(false)
    })

    it('reports the records an undo left alone', async () => {
        const undoRepair = vi
            .fn()
            .mockResolvedValue({ result: damaged, skipped: ['characters:char-2'] })
        const { model } = harness({
            getResult: vi.fn().mockResolvedValue(damaged),
            undoRepair,
        })
        await model.load()
        await model.loadRepairs()
        await model.undo('repair-1')
        expect(undoRepair).toHaveBeenCalledWith('repair-1')
        expect(model.snapshot().skipped).toEqual(['characters:char-2'])
    })

    it('reads the repair choices for the diagnosis a scan just produced', async () => {
        const { deps, model } = harness({
            scan: vi.fn().mockResolvedValue(damaged),
        })
        await model.quickScan()
        expect(deps.planRepair).toHaveBeenCalledOnce()
        expect(model.snapshot().selection).toEqual(['0:drop-reference'])
        expect(model.snapshot().preview?.answered).toBe(1)
    })

    it('reads the repair choices once a deep scan has finished its last page', async () => {
        const deepScan = vi
            .fn()
            .mockResolvedValueOnce({ ...damaged, depth: 'deep', deep: { cursor: 'a', completedObjects: 1, totalObjects: 2, completedBytes: 1, totalBytes: 2, complete: false } })
            .mockResolvedValueOnce({ ...damaged, depth: 'deep', deep: { cursor: 'b', completedObjects: 2, totalObjects: 2, completedBytes: 2, totalBytes: 2, complete: true } })
        const { deps, model } = harness({ deepScan })
        await model.deepScan(false)
        expect(deps.planRepair).toHaveBeenCalledOnce()
        expect(model.snapshot().candidates).toHaveLength(2)
    })

    it('selects one answer per finding for select all and clears it again', async () => {
        const { deps, model } = harness({ getResult: vi.fn().mockResolvedValue(damaged) })
        await model.load()
        await model.loadRepairs()
        await model.toggle('0:drop-reference')
        expect(model.snapshot().selection).toEqual([])
        await model.setAll(true)
        expect(model.snapshot().selection).toEqual(['0:drop-reference'])
        expect(deps.previewRepair).toHaveBeenLastCalledWith(['0:drop-reference'])
        await model.setAll(false)
        expect(model.snapshot().selection).toEqual([])
        expect(model.snapshot().preview).toBeNull()
    })

    it('never asks for a repair while a scan is running', async () => {
        let release = () => {}
        const { deps, model } = harness({
            scan: vi.fn(
                () =>
                    new Promise<DataHealthResult>((resolve) => {
                        release = () => resolve(damaged)
                    }),
            ),
        })
        const running = model.quickScan()
        await Promise.resolve()
        await model.loadRepairs()
        expect(deps.planRepair).not.toHaveBeenCalled()
        release()
        await running
    })
})
