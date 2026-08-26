import { describe, expect, it, vi } from 'vitest'

import {
    acknowledgeRecoveredNativeRestores,
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

describe('native restore bootstrap reconciliation', () => {
    it('reconciles persisted jobs on desktop and Android without widening to other targets', () => {
        expect(shouldReconcileNativeFileJobs(true, false)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, true)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, false)).toBe(false)
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

    it('returns without waiting for an active export and cleans it up in the background', async () => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: string[] = []
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('export-1', 'running', 'writing-export'),
                    kind: 'export-block-risu-save',
                }]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus('export-1', 'succeeded', 'complete'),
                    kind: 'export-block-risu-save',
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
