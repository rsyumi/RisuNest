import { doingChat, reserveGeneration } from "../process/generationState";
import { isLibraryFileOperationReserved, registerLibraryFileOperationGate } from "./libraryFileOperation";
import { get } from 'svelte/store'
import { PayloadTooLargeError } from './nativePersistenceValue'
import { beforeEach, describe, expect, it, vi } from 'vitest'
const nativeInvoke = vi.hoisted(() => vi.fn())
vi.mock('@tauri-apps/api/core', () => ({invoke: nativeInvoke}))

import {
    cancelActiveNativeFileOperation,
    dismissNativeFileOperationOutcome,
    NativeFileOperationBusyError,
    nativeFileOperation,
    nativeFileOperationOutcome,
    recordRecoveredNativeFileOperation,
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
    it('allows generation during exact native-confirmed portable body retry and refuses mismatched receipts', async () => {
        const receipt = {jobId: 'portable', stagingId: 'stage', catalogSha256: 'a'.repeat(64), activationRevision: '8', bindingAuthority: '2', deviceSessionId: 'session'}
        const native = status({jobId: receipt.jobId, kind: 'restore-portable-backup', state: 'cancelled', phase: 'copying-missing-bodies', activationRevision: 8, activationAuthority: '2', deviceSessionId: 'session', restoreAdoptionConfirmed: true, portableBodyRetry: {stagingId: 'stage', catalogSha256: receipt.catalogSha256, pending: true, available: true, sourceRequired: false}})
        nativeInvoke.mockResolvedValue(native)
        await expect(runSharedNativeFileOperation('import', 'portable-retry', async () => {
            expect(isLibraryFileOperationReserved()).toBe(false)
            const generation = reserveGeneration()
            expect(generation).not.toBeNull()
            generation!.release()
            return 'bodies only'
        }, {format: 'library-backup', portableBodyOwner: receipt})).resolves.toBe('bodies only')
        nativeInvoke.mockResolvedValue({...native, restoreAdoptionConfirmed: false})
        await expect(runSharedNativeFileOperation('import', 'unconfirmed-retry', async () => 'unsafe', {portableBodyOwner: receipt})).rejects.toMatchObject({code: 'portable-body-retry-refused'})
        nativeInvoke.mockResolvedValue(native)
        await expect(runSharedNativeFileOperation('import', 'wrong-retry', async () => 'unsafe', {portableBodyOwner: {...receipt, catalogSha256: 'b'.repeat(64)}})).rejects.toMatchObject({code: 'portable-body-receipt-mismatch'})
    })
    it('releases only the adopted portable reservation while retaining body progress and cancellation', async () => {
        const receipt=status({jobId:'portable',kind:'restore-portable-backup',activationRevision:8,activationAuthority:'2',deviceSessionId:'session'})
        let finish!:()=>void
        const held=new Promise<void>(resolve=>{finish=resolve})
        let entered!:()=>void
        const ready=new Promise<void>(resolve=>{entered=resolve})
        const operation=runSharedNativeFileOperation('import','portable-body-progress',async context=> {
            expect(reserveGeneration()).toBeNull()
            context.setBlocking(false)
            expect(isLibraryFileOperationReserved()).toBe(true)
            nativeInvoke.mockResolvedValue({...receipt,restoreAdoptionConfirmed:false})
            await expect(context.releaseLibraryAfterPortableAdoption(receipt)).rejects.toMatchObject({code:'portable-adoption-receipt-mismatch'})
            expect(isLibraryFileOperationReserved()).toBe(true)
            nativeInvoke.mockResolvedValue({...receipt,restoreAdoptionConfirmed:true,phase:'copying-missing-bodies'})
            await context.releaseLibraryAfterPortableAdoption(receipt)
            expect(isLibraryFileOperationReserved()).toBe(false)
            const generation=reserveGeneration()
            expect(generation).not.toBeNull()
            generation!.release()
            context.onStatus({...receipt,phase:'copying-missing-bodies'})
            entered()
            await held
            expect(context.signal.aborted).toBe(true)
        },{format:'library-backup'})
        await ready
        expect(get(nativeFileOperation)?.status?.phase).toBe('copying-missing-bodies')
        cancelActiveNativeFileOperation()
        finish()
        await operation
    })
    it('permits foreground generation and sync only for a native-confirmed snapshot body owner', async () => {
        const receipt = {jobId: 'bodies', kind: 'snapshot-bodies' as const, stagingId: 'stage', activationRevision: '8', bindingAuthority: '2'}
        nativeInvoke.mockResolvedValue(status({jobId: 'bodies', kind: 'snapshot-bodies', snapshotStagingId: 'stage', activationRevision: 8, activationAuthority: '2', phase: 'copying-missing-bodies'}))
        doingChat.set(true)
        const unregister = registerLibraryFileOperationGate(() => new Promise<void>(() => {}))
        try {
            await expect(runSharedNativeFileOperation('import', 'snapshot-bodies', async () => {
                expect(isLibraryFileOperationReserved()).toBe(false)
                return 'settled'
            }, {format: 'library-backup', snapshotBodyOwner: receipt})).resolves.toBe('settled')
            nativeInvoke.mockResolvedValue(status({jobId: 'bodies', kind: 'restore-portable-backup'}))
            await expect(runSharedNativeFileOperation('import', 'wrong-body-owner', async () => 'unsafe', {format: 'library-backup', snapshotBodyOwner: receipt})).rejects.toMatchObject({code: 'snapshot-body-receipt-mismatch'})
            await expect(runSharedNativeFileOperation('import', 'replacement', async () => 'unsafe', {format: 'library-backup'})).rejects.toMatchObject({code: 'generation-active'})
        } finally {
            doingChat.set(false)
            unregister()
        }
    })
    it('waits for safe synchronization settlement and releases a cancelled wait', async () => {
        let releaseSync!: () => void
        const sync = new Promise<void>(resolve => { releaseSync = resolve })
        const unregister = registerLibraryFileOperationGate(() => sync)
        const work = vi.fn(async () => 'imported')
        try {
            const waiting = runSharedNativeFileOperation('import', 'synthetic-character', work, { format: 'content' })
            expect(isLibraryFileOperationReserved()).toBe(true)
            expect(work).not.toHaveBeenCalled()
            expect(get(nativeFileOperation)?.waitingForSync).toBe(true)
            cancelActiveNativeFileOperation()
            await expect(waiting).rejects.toMatchObject({ name: 'AbortError' })
            expect(isLibraryFileOperationReserved()).toBe(false)
            expect(work).not.toHaveBeenCalled()
            const next = runSharedNativeFileOperation('import', 'synthetic-character', work, { format: 'content' })
            expect(work).not.toHaveBeenCalled()
            releaseSync()
            await expect(next).resolves.toBe('imported')
            expect(work).toHaveBeenCalledOnce()
            expect(isLibraryFileOperationReserved()).toBe(false)
        } finally {
            unregister()
            releaseSync()
        }
    })

    beforeEach(() => {
        dismissNativeFileOperationOutcome()
    })

    it('coalesces remounted callers onto one operation and exposes shared progress', async () => {
        let finish!: (value: string) => void
        let previous!: SharedNativeFileOperationContext
        const operation = vi.fn(async (context: SharedNativeFileOperationContext) => {
            previous = context
            context.onStatus(status({ kind: 'restore-block-risu-save' }))
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
        const nextStatus = status({jobId:'next-restore'})
        await runSharedNativeFileOperation('import', 'next-restore', async context => {
            context.onStatus(nextStatus)
            previous.onStatus(status({state:'succeeded', phase:'complete'}))
            expect(get(nativeFileOperation)?.status).toEqual(nextStatus)
            return 'next result'
        }, {presentation:'dialog'})
        recordRecoveredNativeFileOperation(status({state:'cancelled', phase:'complete'}))
        expect(get(nativeFileOperationOutcome)?.status).toEqual(nextStatus)
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
                sourceFingerprintKind: 'whole-file-sha256' as const, sourceSha256: 'abc',
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
                error: { code: 'invalid-source', message: 'Android spool manifest is invalid' },
                expected: {
                    state: 'failed',
                    error: { code: 'invalid-source', message: 'Android spool manifest is invalid', recoveryRequired: false },
                },
            },
            {
                error: new PayloadTooLargeError('message', 70_000_000),
                expected: {
                    state: 'failed',
                    error: {
                        code: 'payload-too-large',
                        message: 'Cannot save message: 70000000 bytes exceed the native request limit',
                        recoveryRequired: false,
                        itemKind: 'message',
                    },
                },
            },
            {
                error: new NativeFileJobError('payload-too-large', 'synthetic native limit', [], 'compatible-database'),
                expected: {
                    state: 'failed',
                    error: { code: 'payload-too-large', message: 'synthetic native limit', recoveryRequired: false, itemKind: 'compatible-database' },
                },
            },
            {
                error: new NativeFileJobError('unsupported-projection', 'synthetic contract rejection', [], undefined,
                    { kind: 'chat', name: 'Synthetic chat', character: 'Synthetic' }),
                expected: {
                    state: 'failed',
                    error: { code: 'unsupported-projection', message: 'synthetic contract rejection', recoveryRequired: false,
                        owner: { kind: 'chat', name: 'Synthetic chat', character: 'Synthetic' } },
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
