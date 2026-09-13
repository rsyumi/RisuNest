import { doingChat, reserveGeneration } from "../process/generationState";
import { isLibraryFileOperationReserved } from "./libraryFileOperation";
import { get } from 'svelte/store'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import {
    cancelActiveNativeFileOperation,
    dismissNativeFileOperationOutcome,
    NativeFileOperationBusyError,
    nativeFileOperation,
    nativeFileOperationOutcome,
    runExternalAndroidNativeFileOperation,
    runSharedNativeFileOperation,
    type NativeFileOperationOutcome,
    type SharedNativeFileOperationContext,
} from './nativeFileJobManager'
import {
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    type NativeFileJobStage,
    type NativeFileJobStatus,
} from './nativeFileJobs'

function status(patch: Partial<NativeFileJobStatus> = {}): NativeFileJobStatus {
    return {
        jobId: 'restore-1',
        kind: 'restore-legacy-local-backup',
        state: 'running',
        phase: 'reading-source',
        progress: { completedBytes: 64, totalBytes: 128, completedItems: 0 },
        ...patch,
    }
}

describe('renderer-lifetime native file job manager', () => {
    beforeEach(() => {
        dismissNativeFileOperationOutcome()
    })

    it('coalesces remounted callers onto one operation and exposes shared progress', async () => {
        let finish!: (value: string) => void
        const operation = vi.fn(async ({ onStatus }: SharedNativeFileOperationContext) => {
            onStatus(status({ kind: 'restore-block-risu-save' }))
            return await new Promise<string>((resolve) => finish = resolve)
        })

        const first = runSharedNativeFileOperation('import', 'risu-save-import', operation)
        const second = runSharedNativeFileOperation('import', 'risu-save-import', operation)

        expect(first).toBe(second)
        expect(operation).toHaveBeenCalledOnce()
        expect(get(nativeFileOperation)?.status?.progress.completedBytes).toBe(64)
        expect(get(nativeFileOperation)?.presentation).toBe('inline')

        finish('done')
        await expect(first).resolves.toBe('done')
        expect(get(nativeFileOperation)).toBeNull()
    })

    it('cancels only through the explicit manager action', async () => {
        let observedSignal!: AbortSignal
        const promise = runSharedNativeFileOperation('export', 'lossless-backup-export', async ({ signal }) => {
            observedSignal = signal
            return await new Promise<never>((_resolve, reject) => {
                signal.addEventListener('abort', () => reject(signal.reason))
            })
        })

        expect(observedSignal.aborted).toBe(false)
        expect(get(nativeFileOperation)?.cancelRequested).toBe(false)
        cancelActiveNativeFileOperation()
        expect(get(nativeFileOperation)?.cancelRequested).toBe(true)
        await expect(promise).rejects.toBeDefined()
        expect(observedSignal.aborted).toBe(true)
    })

    it('runs external Android open events FIFO instead of joining an unrelated active operation', async () => {
        let releaseActive!: () => void
        let releaseFirstEvent!: () => void
        const calls: string[] = []
        const active = runSharedNativeFileOperation('import', 'active-import', async () => {
            calls.push('active')
            await new Promise<void>((resolve) => releaseActive = resolve)
            return 'active'
        })
        const firstEvent = runExternalAndroidNativeFileOperation('import', async () => {
            calls.push('first-event')
            await new Promise<void>((resolve) => releaseFirstEvent = resolve)
            return 'first-event'
        })
        const secondEvent = runExternalAndroidNativeFileOperation('import', async () => {
            calls.push('second-event')
            return 'second-event'
        })

        expect(firstEvent).not.toBe(active)
        expect(secondEvent).not.toBe(active)
        expect(calls).toEqual(['active'])

        releaseActive()
        await expect(active).resolves.toBe('active')
        await Promise.resolve()
        expect(calls).toEqual(['active', 'first-event'])

        releaseFirstEvent()
        await expect(firstEvent).resolves.toBe('first-event')
        await expect(secondEvent).resolves.toBe('second-event')
        expect(calls).toEqual(['active', 'first-event', 'second-event'])
    })

    it('rejects a different operation instead of returning the active result', async () => {
        let finish!: () => void
        const first = runSharedNativeFileOperation(
            'import',
            'lossless-backup-import',
            () => new Promise<void>((resolve) => finish = resolve),
        )
        const conflicting = vi.fn(async () => 'wrong-result')

        await expect(runSharedNativeFileOperation(
            'import',
            'risu-save-import',
            conflicting,
        )).rejects.toBeInstanceOf(NativeFileOperationBusyError)
        expect(conflicting).not.toHaveBeenCalled()

        finish()
        await first
    })

    it('exposes the source and presentation of a dialog operation while it runs', async () => {
        let finish!: () => void
        const promise = runSharedNativeFileOperation(
            'import',
            'legacy-local-backup-import',
            async ({ setSource, onStatus }) => {
                setSource({ name: 'risu-backup.bin', bytes: 4096 })
                onStatus(status())
                await new Promise<void>((resolve) => finish = resolve)
                return { warningCodes: [] }
            },
            { presentation: 'dialog', format: 'local-backup' },
        )

        const running = get(nativeFileOperation)
        expect(running?.presentation).toBe('dialog')
        expect(running?.format).toBe('local-backup')
        expect(running?.source).toEqual({ name: 'risu-backup.bin', bytes: 4096 })
        expect(running?.status?.jobId).toBe('restore-1')
        expect(running?.observedStages).toEqual(['reading-archive'])
        expect(typeof running?.startedAt).toBe('number')
        expect(get(nativeFileOperationOutcome)).toBeNull()

        finish()
        await promise
    })

    it('records each distinct stage in order, from detail when present and from the phase otherwise', async () => {
        const detail = (stage: NativeFileJobStage) => ({
            stage,
            stageCompleted: 0,
            stageUnit: 'items' as const,
            counts: {
                entriesRead: 0, assets: 0, inlays: 0, coldStorage: 0, pocketMedia: 0, pocketMetadata: 0,
                skipped: 0, attachmentsPrepared: 0, characters: 0, presets: 0, blocks: 0,
            },
        })
        await runSharedNativeFileOperation(
            'import',
            'risu-save-import',
            async ({ onStatus }) => {
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'queued' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'reading-source' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'reading-source' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'staging-database' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'awaiting-activation', state: 'waitingForInput' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'activating-database' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'complete', state: 'succeeded' }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'complete', state: 'succeeded', detail: detail('refreshing-app') }))
                onStatus(status({ kind: 'restore-block-risu-save', phase: 'complete', state: 'succeeded', detail: detail('reloading-plugins') }))
                return { warningCodes: [] }
            },
            { presentation: 'dialog', format: 'risu-save' },
        )
        expect(get(nativeFileOperationOutcome)?.observedStages).toEqual([
            'reading-database', 'finalizing-staging', 'awaiting-activation', 'activating',
            'refreshing-app', 'reloading-plugins',
        ])
        expect(get(nativeFileOperationOutcome)?.format).toBe('risu-save')
    })

    it('publishes a dialog outcome carrying the final status when the operation succeeds', async () => {
        const terminal = status({
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 128, totalBytes: 128, completedItems: 3 },
            result: {
                revision: 7,
                sourceBytes: 128,
                sourceSha256: 'abc',
                characterCount: 2,
                presetCount: 1,
                warningCodes: ['cleanup-failed'],
            },
        })

        const value = await runSharedNativeFileOperation(
            'import',
            'legacy-local-backup-import',
            async ({ setSource, onStatus }) => {
                setSource({ name: 'risu-backup.bin', bytes: 128 })
                onStatus(status())
                onStatus(terminal)
                return { mode: 'native', warningCodes: ['partial-destination-may-remain'] }
            },
            { presentation: 'dialog' },
        )

        expect(value.mode).toBe('native')
        expect(get(nativeFileOperation)).toBeNull()
        const outcome = get(nativeFileOperationOutcome)
        expect(outcome).toMatchObject({
            kind: 'import',
            state: 'succeeded',
            source: { name: 'risu-backup.bin', bytes: 128 },
            partialWritesPossible: false,
        })
        expect(outcome?.status).toEqual(terminal)
        expect(outcome?.result).toEqual(terminal.result)
        expect(outcome?.warningCodes).toEqual(['partial-destination-may-remain', 'cleanup-failed'])
        expect(outcome?.finishedAt).toBeGreaterThanOrEqual(outcome?.startedAt ?? Infinity)

        dismissNativeFileOperationOutcome()
        expect(get(nativeFileOperationOutcome)).toBeNull()
    })

    it('keeps inline operations and cancelled pickers out of the outcome store', async () => {
        await runSharedNativeFileOperation('import', 'risu-save-import', async ({ onStatus }) => {
            onStatus(status({ state: 'succeeded', phase: 'complete' }))
            return { warningCodes: [] }
        })
        expect(get(nativeFileOperationOutcome)).toBeNull()

        await runSharedNativeFileOperation(
            'import',
            'risu-save-import',
            async () => null,
            { presentation: 'dialog' },
        )
        expect(get(nativeFileOperationOutcome)).toBeNull()
    })

    it('maps cancellation and failure kinds onto the dialog outcome', async () => {
        const cases: Array<{
            error: unknown
            expected: Partial<NativeFileOperationOutcome>
        }> = [
            {
                error: Object.assign(new DOMException('cancelled', 'AbortError'), {
                    warningCodes: ['partial-destination-may-remain'],
                }),
                expected: { state: 'cancelled', warningCodes: ['partial-destination-may-remain'] },
            },
            {
                error: new NativeFileJobActivationCommittedError(9, new Error('refresh exploded')),
                expected: {
                    state: 'failed',
                    error: {
                        code: 'activation-committed-refresh-failed',
                        message: 'refresh exploded',
                        recoveryRequired: true,
                    },
                },
            },
            {
                error: new NativeFileJobError('unsupported-format', 'not a backup'),
                expected: {
                    state: 'failed',
                    error: { code: 'unsupported-format', message: 'not a backup', recoveryRequired: false },
                },
            },
            {
                error: new TypeError('boom'),
                expected: {
                    state: 'failed',
                    error: { code: 'import-error', message: 'boom', recoveryRequired: false },
                },
            },
        ]

        for (const testCase of cases) {
            dismissNativeFileOperationOutcome()
            await expect(runSharedNativeFileOperation(
                'import',
                'legacy-local-backup-import',
                async ({ onStatus }) => {
                    onStatus(status())
                    throw testCase.error
                },
                { presentation: 'dialog' },
            )).rejects.toBe(testCase.error)
            expect(get(nativeFileOperation)).toBeNull()
            expect(get(nativeFileOperationOutcome)).toMatchObject({ kind: 'import', ...testCase.expected })
            expect(get(nativeFileOperationOutcome)?.status?.jobId).toBe('restore-1')
        }
    })

    it('records partial writes for a cancelled dialog operation', async () => {
        await expect(runSharedNativeFileOperation(
            'import',
            'legacy-local-backup-import',
            async ({ setPartialWritesPossible, signal }) => {
                setPartialWritesPossible(true)
                expect(get(nativeFileOperation)?.partialWritesPossible).toBe(true)
                cancelActiveNativeFileOperation()
                expect(signal.aborted).toBe(true)
                throw new DOMException('cancelled', 'AbortError')
            },
            { presentation: 'dialog' },
        )).rejects.toBeInstanceOf(DOMException)

        expect(get(nativeFileOperationOutcome)).toMatchObject({
            state: 'cancelled',
            partialWritesPossible: true,
        })
    })

    it('clears a stale outcome when the next dialog operation starts', async () => {
        await runSharedNativeFileOperation(
            'import',
            'legacy-local-backup-import',
            async () => ({ warningCodes: [] }),
            { presentation: 'dialog' },
        )
        expect(get(nativeFileOperationOutcome)?.state).toBe('succeeded')

        let finish!: () => void
        const next = runSharedNativeFileOperation(
            'import',
            'risu-save-import',
            () => new Promise<null>((resolve) => finish = () => resolve(null)),
            { presentation: 'dialog' },
        )
        expect(get(nativeFileOperationOutcome)).toBeNull()
        finish()
        await next
        expect(get(nativeFileOperationOutcome)).toBeNull()
    })
})

describe('common library file admission', () => {
    it('blocks backup and restore during generation without waiting or cancelling it', async () => {
        const generation = reserveGeneration()!;
        try {
            for (const kind of ['export', 'import'] as const) {
                const work=vi.fn(async()=>1);
                await expect(
                    runSharedNativeFileOperation(kind, kind, work, {
                        format: 'library-backup',
                    }),
                ).rejects.toMatchObject({ code: 'generation-active' })
                expect(work).not.toHaveBeenCalled(); expect(get(doingChat)).toBe(true);
            }
        } finally { generation.release() }
    });
    it('retains admission until actual settlement after a cancel request', async () => {
        let finish!: () => void;
        const backup = runSharedNativeFileOperation(
            'export',
            'backup',
            () =>
                new Promise<void>((r) => {
                    finish = r
                }),
            { format: 'library-backup' },
        )
        expect(reserveGeneration()).toBeNull();
        cancelActiveNativeFileOperation(); expect(isLibraryFileOperationReserved()).toBe(true);
        await expect(
            runSharedNativeFileOperation('import', 'restore', async () => 1, {
                format: 'library-backup',
            }),
        ).rejects.toBeInstanceOf(NativeFileOperationBusyError)
        finish(); await backup; expect(isLibraryFileOperationReserved()).toBe(false);
        const generation=reserveGeneration();expect(generation).not.toBeNull();generation?.release();
    });
    it('releases admission on a synchronous callback failure and rejects reentrant different jobs', async () => {
        await expect(
            runSharedNativeFileOperation(
                'export',
                'backup',
                () => {
                    throw new Error('capture failed')
                },
                { format: 'library-backup' },
            ),
        ).rejects.toThrow('capture failed')
        expect(isLibraryFileOperationReserved()).toBe(false);
        await runSharedNativeFileOperation(
            'export',
            'backup',
            async () => {
                await expect(
                    runSharedNativeFileOperation(
                        'import',
                        'other',
                        async () => 1,
                    ),
                ).rejects.toBeInstanceOf(NativeFileOperationBusyError)
                return 1
            },
            { format: 'library-backup' },
        )
    });
});

it('does not queue a known backup delivered by Android while another file job runs', async () => {
    let finish!: () => void;
    const active=runSharedNativeFileOperation('import','other',()=>new Promise<void>(r=>{finish=r}));
    const backup=vi.fn(async()=>1);
    await expect(
        runExternalAndroidNativeFileOperation('export', backup, {
            format: 'library-backup',
        }),
    ).rejects.toBeInstanceOf(NativeFileOperationBusyError)
    finish();await active;await Promise.resolve();expect(backup).not.toHaveBeenCalled();
});
