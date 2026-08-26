import { describe, expect, it } from 'vitest'

import {
    NativeFileJobActivationCommittedError,
    runNativeBlockRisuSaveRestore,
    runNativeBlockRisuSaveExport,
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
                    if (command === 'native_file_job_start') {
                        return { jobId: 'job-1', warningCodes: ['cleanup-failed'] }
                    }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(4)
        expect(result.warningCodes).toEqual(['cleanup-failed'])
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

    it('does not start a native job when cancellation arrives during the flush', async () => {
        const controller = new AbortController()
        const commands: string[] = []

        await expect(runNativeBlockRisuSaveRestore(
            {
                revision: 2,
                flushPendingData: async () => controller.abort(),
                refreshActiveWorkingSet: async () => undefined,
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )).rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual([])
    })

    it('forgets a committed job and exposes recovery state when refreshing fails', async () => {
        const commands: string[] = []
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'b'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        const promise = runNativeBlockRisuSaveRestore(
            {
                revision: 8,
                flushPendingData: async () => undefined,
                refreshActiveWorkingSet: async () => {
                    throw new Error('refresh unavailable')
                },
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') return status('succeeded', committed)
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        await expect(promise).rejects.toEqual(expect.objectContaining({
            name: 'NativeFileJobActivationCommittedError',
            code: 'activation-committed-refresh-failed',
            committedRevision: 9,
            recoveryRequired: true,
        } satisfies Partial<NativeFileJobActivationCommittedError>))
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('keeps committed success when terminal acknowledgement fails', async () => {
        const commands: string[] = []
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'c'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        const result = await runNativeBlockRisuSaveRestore(
            {
                revision: 8,
                flushPendingData: async () => undefined,
                refreshActiveWorkingSet: async () => undefined,
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') return status('succeeded', committed)
                    if (command === 'native_file_job_forget') {
                        throw { code: 'store-error', message: 'terminal acknowledgement failed' }
                    }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual({
            ...committed,
            warningCodes: ['cleanup-failed'],
        })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('acknowledges terminal failure even when translating it to an exception', async () => {
        const commands: string[] = []
        const failed = status('failed')
        failed.error = { code: 'corrupt-input', message: 'invalid gzip data' }

        await expect(runNativeBlockRisuSaveRestore(
            {
                revision: 3,
                flushPendingData: async () => undefined,
                refreshActiveWorkingSet: async () => undefined,
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') return failed
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )).rejects.toMatchObject({ code: 'corrupt-input' })
        expect(commands.at(-1)).toBe('native_file_job_forget')
    })

    it('preserves structured native command errors at the facade boundary', async () => {
        await expect(runNativeBlockRisuSaveRestore(
            {
                revision: 1,
                flushPendingData: async () => undefined,
                refreshActiveWorkingSet: async () => undefined,
            },
            { type: 'desktopPath', path: 'C:\\missing\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async () => {
                    throw { code: 'invalid-source', message: 'desktop source is unavailable' }
                },
                wait: async () => undefined,
            },
        )).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'invalid-source',
            message: 'desktop source is unavailable',
        })
    })

    it('exports a pinned revision to a native destination without file chunks in IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const observed: NativeFileJobStatus[] = []
        let revision = 11
        const runtime = {
            get revision() { return revision },
            flushPendingData: async (reason: string) => {
                calls.push([`flush:${reason}`, undefined])
                revision = 12
            },
        }
        const running: NativeFileJobStatus = {
            jobId: 'export-1',
            kind: 'export-block-risu-save',
            state: 'running',
            phase: 'writing-export',
            progress: {
                completedBytes: 64,
                completedItems: 2,
            },
        }
        const succeeded: NativeFileJobStatus = {
            jobId: 'export-1',
            kind: 'export-block-risu-save',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 512,
                totalBytes: 512,
                completedItems: 4,
                totalItems: 4,
            },
            result: {
                revision: 12,
                sourceBytes: 256,
                sourceSha256: 'd'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            },
        }
        const statuses = [running, succeeded]

        const result = await runNativeBlockRisuSaveExport(
            runtime,
            'C:\\chosen\\backup.risudat',
            { omitAccount: true, onStatus: (value) => observed.push(value) },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'export-1' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual(succeeded.result)
        expect(observed).toEqual([running, succeeded])
        expect(calls).toEqual([
            ['flush:native-block-risu-save-export', undefined],
            ['native_file_job_start', {
                request: {
                    kind: 'export-block-risu-save',
                    destination: 'C:\\chosen\\backup.risudat',
                    expectedRevision: 12,
                    omitAccount: true,
                },
            }],
            ['native_file_job_status', { jobId: 'export-1' }],
            ['native_file_job_status', { jobId: 'export-1' }],
            ['native_file_job_forget', { jobId: 'export-1' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('keeps the JavaScript facade bounded when native reports a 10 GiB export', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const collectGarbage = (globalThis as typeof globalThis & { gc?: () => void }).gc
        collectGarbage?.()
        const heapBefore = process.memoryUsage().heapUsed
        const tenGiB = 10 * 1024 * 1024 * 1024
        const terminal: NativeFileJobStatus = {
            jobId: 'large-export',
            kind: 'export-block-risu-save',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: tenGiB * 2,
                totalBytes: tenGiB * 2,
                completedItems: 50_007,
                totalItems: 50_007,
            },
            result: {
                revision: 15,
                sourceBytes: tenGiB,
                sourceSha256: 'e'.repeat(64),
                characterCount: 50_000,
                presetCount: 7,
                warningCodes: [],
            },
        }

        const result = await runNativeBlockRisuSaveExport(
            {
                revision: 15,
                flushPendingData: async () => undefined,
            },
            'C:\\chosen\\ten-gib.risudat',
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') {
                        return { jobId: 'large-export' }
                    }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        collectGarbage?.()
        const heapDelta = Math.max(0, process.memoryUsage().heapUsed - heapBefore)
        console.info(`[native-file-job-heap] declared=10GiB heapDelta=${heapDelta}`)
        expect(result.sourceBytes).toBe(tenGiB)
        expect(JSON.stringify(calls).length).toBeLessThan(512)
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
        if (collectGarbage) expect(heapDelta).toBeLessThan(16 * 1024 * 1024)
    })

    it('cancels a native export without acknowledging it before terminal cleanup', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        let statusCount = 0

        const promise = runNativeBlockRisuSaveExport(
            {
                revision: 6,
                flushPendingData: async () => undefined,
            },
            'C:\\chosen\\backup.risudat',
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'export-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? {
                                  jobId: 'export-1',
                                  kind: 'export-block-risu-save',
                                  state: 'running',
                                  phase: 'writing-export',
                                  progress: { completedBytes: 1, completedItems: 0 },
                              }
                            : {
                                  jobId: 'export-1',
                                  kind: 'export-block-risu-save',
                                  state: 'cancelled',
                                  phase: 'complete',
                                  progress: { completedBytes: 1, completedItems: 0 },
                              }
                    }
                    if (command === 'native_file_job_cancel') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => controller.abort(),
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
