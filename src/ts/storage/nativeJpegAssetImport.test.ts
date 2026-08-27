import { describe, expect, it, vi } from 'vitest'

vi.mock('./persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: vi.fn(),
    capturePersistentMutationToken: vi.fn(),
}))

import {
    importNativeJpegAsset,
    type NativeJpegAssetImportDependencies,
} from './nativeJpegAssetImport'
import type { NativeFileJobActivationCommittedError } from './nativeFileJobs'

function dependencies(statuses: unknown[]): NativeJpegAssetImportDependencies {
    const invoke = vi.fn(async (command: string) => {
        if (command === 'native_file_job_start') return { jobId: 'jpeg-job', warningCodes: [] }
        if (command === 'native_file_job_status') return statuses.shift()
        if (command === 'native_file_job_cancel') return { outcome: 'requested' }
        if (command === 'native_file_job_forget') return true
        throw new Error(`Unexpected command: ${command}`)
    })
    return {
        invoke,
        wait: vi.fn(async () => undefined),
        captureMutationToken: vi.fn(async () => ({ revision: 7, mutationGeneration: 3 })),
        acquireFence: vi.fn(async () => ({
            refreshCommittedWorkingSet: vi.fn(async () => undefined),
            release: vi.fn(),
        })),
    }
}

describe('native JPEG asset import', () => {
    it('sends an explicit current-character destination and refreshes the committed revision', async () => {
        const deps = dependencies([{
            jobId: 'jpeg-job',
            kind: 'import-jpeg-asset',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 4,
                totalBytes: 4,
                completedItems: 1,
                totalItems: 1,
            },
            result: {
                revision: 8,
                sourceBytes: 4,
                sourceSha256: 'a'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
            },
        }])

        const result = await importNativeJpegAsset({
            source: { type: 'desktopPath', path: 'C:\\chosen\\portrait.jpeg' },
            displayName: 'portrait.jpeg',
            destination: { kind: 'current-character-image', characterId: 'character-1' },
        }, {}, deps)

        expect(deps.invoke).toHaveBeenNthCalledWith(1, 'native_file_job_start', {
            request: {
                kind: 'import-jpeg-asset',
                source: { type: 'desktopPath', path: 'C:\\chosen\\portrait.jpeg' },
                displayName: 'portrait.jpeg',
                destination: { kind: 'current-character-image', characterId: 'character-1' },
                expectedRevision: 7,
            },
        })
        const fence = await vi.mocked(deps.acquireFence).mock.results[0].value
        expect(fence.refreshCommittedWorkingSet).toHaveBeenCalledWith(8)
        expect(fence.release).toHaveBeenCalledOnce()
        expect(result).toEqual({
            revision: 8,
            logicalId: `assets/${'a'.repeat(64)}.jpeg`,
        })
    })

    it('cancels and drains without refreshing the prior character image', async () => {
        const deps = dependencies([
            {
                jobId: 'jpeg-job',
                kind: 'import-jpeg-asset',
                state: 'running',
                phase: 'reading-source',
                progress: { completedBytes: 1, completedItems: 0 },
            },
            {
                jobId: 'jpeg-job',
                kind: 'import-jpeg-asset',
                state: 'cancelled',
                phase: 'complete',
                progress: { completedBytes: 1, completedItems: 0 },
            },
        ])
        const controller = new AbortController()
        vi.mocked(deps.wait).mockImplementationOnce(async () => controller.abort())

        await expect(importNativeJpegAsset({
            source: { type: 'desktopPath', path: 'C:\\chosen\\portrait.jpg' },
            displayName: 'portrait.jpg',
            destination: { kind: 'current-character-image', characterId: 'character-1' },
        }, { signal: controller.signal }, deps)).rejects.toMatchObject({ name: 'AbortError' })

        expect(deps.invoke).toHaveBeenCalledWith('native_file_job_cancel', { jobId: 'jpeg-job' })
        const fence = await vi.mocked(deps.acquireFence).mock.results[0].value
        expect(fence.refreshCommittedWorkingSet).not.toHaveBeenCalled()
        expect(fence.release).toHaveBeenCalledOnce()
    })

    it('preserves the prior image when native activation reports a revision conflict', async () => {
        const deps = dependencies([{
            jobId: 'jpeg-job',
            kind: 'import-jpeg-asset',
            state: 'failed',
            phase: 'complete',
            progress: { completedBytes: 4, completedItems: 0 },
            error: { code: 'revision-conflict', message: 'revision changed' },
        }])

        await expect(importNativeJpegAsset({
            source: { type: 'androidSpool', token: 'spool-token' },
            displayName: 'portrait.jpeg',
            destination: { kind: 'current-character-image', characterId: 'character-1' },
        }, {}, deps)).rejects.toMatchObject({ code: 'revision-conflict' })

        const fence = await vi.mocked(deps.acquireFence).mock.results[0].value
        expect(fence.refreshCommittedWorkingSet).not.toHaveBeenCalled()
        expect(fence.release).toHaveBeenCalledOnce()
    })

    it('retains committed success for recovery when refreshing the renderer fails', async () => {
        const deps = dependencies([{
            jobId: 'jpeg-job',
            kind: 'import-jpeg-asset',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 4, completedItems: 1 },
            result: {
                revision: 8,
                sourceBytes: 4,
                sourceSha256: 'b'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
            },
        }])
        vi.mocked(deps.acquireFence).mockResolvedValueOnce({
            refreshCommittedWorkingSet: vi.fn(async () => {
                throw new Error('refresh unavailable')
            }),
            release: vi.fn(),
        })

        await expect(importNativeJpegAsset({
            source: { type: 'desktopPath', path: 'C:\\chosen\\portrait.jpg' },
            displayName: 'portrait.jpg',
            destination: { kind: 'current-character-image', characterId: 'character-1' },
        }, {}, deps)).rejects.toEqual(expect.objectContaining({
            name: 'NativeFileJobActivationCommittedError',
            code: 'activation-committed-refresh-failed',
            committedRevision: 8,
            recoveryRequired: true,
        } satisfies Partial<NativeFileJobActivationCommittedError>))

        expect(deps.invoke).not.toHaveBeenCalledWith('native_file_job_forget', {
            jobId: 'jpeg-job',
        })
        const acquiredFence = await vi.mocked(deps.acquireFence).mock.results[0].value
        expect(acquiredFence.release).toHaveBeenCalledOnce()
    })
})
