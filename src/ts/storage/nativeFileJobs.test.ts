import { describe, expect, it } from 'vitest'

import {
    runNativeBlockRisuSaveRestore,
    type NativeFileJobStatus,
} from './nativeFileJobs'

function status(
    state: NativeFileJobStatus['state'],
    result?: NativeFileJobStatus['result'],
): NativeFileJobStatus {
    return {
        jobId: 'job-1',
        kind: 'restore-block-risu-save',
        state,
        phase: state === 'succeeded' ? 'complete' : 'reading-source',
        progress: {
            completedBytes: state === 'queued' ? 0 : 128,
            totalBytes: 128,
            completedItems: 0,
        },
        result,
    }
}

describe('native file jobs', () => {
    it('restores from a descriptor without sending file or database bytes through IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const statuses = [
            status('running'),
            status('succeeded', {
                revision: 4,
                sourceBytes: 128,
                sourceSha256: 'a'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }),
        ]
        const refreshed: number[] = []
        const runtime = {
            revision: 3,
            flushPendingData: async (reason: string) => {
                calls.push([`flush:${reason}`, undefined])
            },
            refreshActiveWorkingSet: async (revision: number) => {
                refreshed.push(revision)
            },
        }

        const result = await runNativeBlockRisuSaveRestore(
            runtime,
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(4)
        expect(refreshed).toEqual([4])
        expect(calls).toEqual([
            ['flush:native-block-risu-save-restore', undefined],
            ['native_file_job_start', {
                request: {
                    kind: 'restore-block-risu-save',
                    source: { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
                    expectedRevision: 3,
                },
            }],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['native_file_job_forget', { jobId: 'job-1' }],
        ])
        expect(calls.some(([command]) => command.includes('read'))).toBe(false)
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('explicit abort requests native cancellation and waits for terminal cleanup', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        let statusCount = 0
        const promise = runNativeBlockRisuSaveRestore(
            {
                revision: 7,
                flushPendingData: async () => undefined,
                refreshActiveWorkingSet: async () => {
                    throw new Error('cancelled restore must not refresh')
                },
            },
            { type: 'androidSpool', token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557' },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1 ? status('running') : status('cancelled')
                    }
                    if (command === 'native_file_job_cancel') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => {
                    controller.abort()
                },
            },
        )

        await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })
})
