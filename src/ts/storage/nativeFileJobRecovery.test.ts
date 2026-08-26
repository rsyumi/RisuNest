import { describe, expect, it, vi } from 'vitest'

import {
    acknowledgeRecoveredNativeRestores,
    reconcileNativeFileJobsBeforeBootstrap,
    reconcileNativeRestoresBeforeBootstrap,
    shouldReconcileNativeFileJobs,
} from './nativeFileJobRecovery'
import type { NativeFileJobStatus } from './nativeFileJobs'

function restoreStatus(
    jobId: string,
    state: NativeFileJobStatus['state'],
    phase: NativeFileJobStatus['phase'],
): NativeFileJobStatus {
    return {
        jobId,
        kind: 'restore-block-risu-save',
        state,
        phase,
        progress: { completedBytes: 0, completedItems: 0 },
        result: state === 'succeeded' ? {
            revision: 2,
            sourceBytes: 128,
            sourceSha256: 'a'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        } : undefined,
    }
}

describe('native file job bootstrap reconciliation', () => {
    it('reconciles restore jobs only where desktop or Android SAF activation is supported', () => {
        expect(shouldReconcileNativeFileJobs(true, false, false)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, true, true)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, true, false)).toBe(false)
        expect(shouldReconcileNativeFileJobs(false, false, true)).toBe(false)
    })

    it('waits for an active restore, finalizes staged data, and retains success for plugin reload', async () => {
        const calls: string[] = []
        const statuses = [
            restoreStatus('restore-1', 'waitingForInput', 'awaiting-activation'),
            restoreStatus('restore-1', 'succeeded', 'complete'),
        ]

        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') {
                    return [restoreStatus('restore-1', 'running', 'reading-source')]
                }
                if (command === 'native_file_job_status') return statuses.shift()
                if (command === 'native_file_job_finalize') return 'requested'
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual(['restore-1'])
        expect(calls).toEqual([
            'native_file_job_list',
            'native_file_job_status',
            'native_file_job_finalize',
            'native_file_job_status',
        ])
        expect(calls).not.toContain('native_file_job_forget')
    })

    it('acknowledges failed restores immediately', async () => {
        const forgotten: string[] = []
        const failed = restoreStatus('restore-failed', 'failed', 'complete')
        failed.error = { code: 'corrupt-input', message: 'bad file' }

        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command, args) => {
                if (command === 'native_file_job_list') return [
                    failed,
                ]
                if (command === 'native_file_job_forget') {
                    forgotten.push(String(args?.jobId))
                    return true
                }
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual([])
        expect(forgotten).toEqual(['restore-failed'])
    })

    it('cancels and drains a nonterminal content preparation without restore finalization', async () => {
        const calls: string[] = []
        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('content-1', 'waitingForInput', 'awaiting-content-mapping'),
                    kind: 'prepare-content-import' as const,
                }]
                if (command === 'native_file_job_cancel') return 'requested'
                if (command === 'native_file_job_status') return {
                    ...restoreStatus('content-1', 'cancelled', 'complete'),
                    kind: 'prepare-content-import' as const,
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual([])
        expect(calls).toEqual([
            'native_file_job_list',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
        expect(calls).not.toContain('native_file_job_finalize')
    })

    it('forgets terminal content jobs without adding restore acknowledgement', async () => {
        const calls: string[] = []
        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('content-1', 'failed', 'complete'),
                    kind: 'prepare-content-import' as const,
                }]
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual([])
        expect(calls).toEqual(['native_file_job_list', 'native_file_job_forget'])
    })

    it.each([
        ['export-block-risu-save', 'export-1'],
        ['kei-backup-upload', 'kei-1'],
    ] as const)('returns without waiting for an active %s job and cleans it up in the background', async (kind, jobId) => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: string[] = []
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus(jobId, 'running', 'writing-export'),
                    kind,
                }]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus(jobId, 'succeeded', 'complete'),
                    kind,
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => pollingGate),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        expect(calls).toEqual(['native_file_job_list'])

        resumePolling()
        await vi.waitFor(() => {
            expect(calls).toEqual([
                'native_file_job_list',
                'native_file_job_status',
                'native_file_job_forget',
            ])
        })
    })

    it('cleans an abandoned Android lossless handoff before forgetting its terminal job', async () => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risulossless-123e4567-e89b-42d3-a456-426614174004.risulossless'
        const dependencies = {
            invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
                calls.push([command, args])
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('lossless-export', 'running', 'writing-export'),
                    kind: 'export-lossless-backup' as const,
                }]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus('lossless-export', 'succeeded', 'complete'),
                    kind: 'export-lossless-backup' as const,
                    result: {
                        ...restoreStatus('lossless-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }
                if (command === 'native_lossless_handoff_cleanup') return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => pollingGate),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        expect(calls).toEqual([['native_file_job_list', undefined]])

        resumePolling()
        await vi.waitFor(() => {
            expect(calls).toEqual([
                ['native_file_job_list', undefined],
                ['native_file_job_status', { jobId: 'lossless-export' }],
                ['native_lossless_handoff_cleanup', { path: handoffPath }],
                ['native_file_job_forget', { jobId: 'lossless-export' }],
            ])
        })
    })

    it('returns every publication job for late reconciliation without touching it early', async () => {
        const calls: string[] = []
        const publications: NativeFileJobStatus[] = [
            {
                jobId: 'publication-running',
                kind: 'official-publication-upload',
                state: 'running',
                phase: 'uploading-database',
                progress: { completedBytes: 64, completedItems: 0 },
            },
            {
                jobId: 'publication-succeeded',
                kind: 'official-publication-upload',
                state: 'succeeded',
                phase: 'complete',
                progress: { completedBytes: 128, completedItems: 1 },
            },
            {
                jobId: 'publication-failed',
                kind: 'official-publication-upload',
                state: 'failed',
                phase: 'complete',
                progress: { completedBytes: 64, completedItems: 0 },
            },
            {
                jobId: 'publication-cancelled',
                kind: 'official-publication-upload',
                state: 'cancelled',
                phase: 'complete',
                progress: { completedBytes: 0, completedItems: 0 },
            },
        ]

        const result = await reconcileNativeFileJobsBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return publications
                throw new Error(`Publication recovery touched ${command}`)
            }),
            wait: vi.fn(async () => {
                throw new Error('Publication recovery waited during bootstrap')
            }),
        })

        expect(result).toEqual({
            pendingRestoreAcknowledgements: [],
            pendingOfficialPublications: [
                'publication-running',
                'publication-succeeded',
                'publication-failed',
                'publication-cancelled',
            ],
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it('scans publications on Tauri targets where restore jobs are not supported', async () => {
        const calls: string[] = []
        const result = await reconcileNativeFileJobsBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [
                    restoreStatus('restore-unsupported', 'running', 'reading-source'),
                    {
                        jobId: 'publication-android',
                        kind: 'official-publication-upload',
                        state: 'running',
                        phase: 'uploading-database',
                        progress: { completedBytes: 0, completedItems: 0 },
                    },
                ]
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        }, { reconcileRestores: false })

        expect(result).toEqual({
            pendingRestoreAcknowledgements: [],
            pendingOfficialPublications: ['publication-android'],
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it('forgets committed restores only after the caller reports successful plugin loading', async () => {
        const calls: string[] = []
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                return true
            }),
            wait: vi.fn(async () => undefined),
        }

        await acknowledgeRecoveredNativeRestores(['restore-1'], dependencies)

        expect(calls).toEqual(['native_file_job_forget'])
        expect(dependencies.invoke).toHaveBeenCalledWith(
            'native_file_job_forget',
            { jobId: 'restore-1' },
        )
    })
})
