import { describe, expect, it, vi } from 'vitest'
import type { DataHealthResult } from './dataHealth'
import { createDataHealthModel } from './dataHealthModel'

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

function harness(overrides: Partial<Parameters<typeof createDataHealthModel>[0]> = {}) {
    const deps = {
        getResult: vi.fn().mockResolvedValue(null),
        scan: vi.fn().mockResolvedValue(result()),
        deepScan: vi.fn().mockResolvedValue(deepPage(3, true)),
        cancel: vi.fn().mockResolvedValue(undefined),
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
