import { describe, expect, it, vi } from 'vitest'

import {
    NativeFileJobActivationCommittedError,
    runNativeOfficialAccountSnapshotRestore,
    runNativeBlockRisuSaveRestore,
    runNativeBlockRisuSaveExport,
    runNativeLosslessBackupExport,
    runNativeLosslessBackupRestore,
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

function restoreRuntime(
    revision: number,
    options: {
        capture?: () => void | Promise<void>
        refresh?: (revision: number) => void | Promise<void>
        acquire?: () => void | Promise<void>
        release?: () => void
    } = {},
) {
    return {
        capturePersistentMutationToken: async () => {
            await options.capture?.()
            return { revision, mutationGeneration: 1 }
        },
        acquireDestructiveReplacementFence: async () => {
            await options.acquire?.()
            return {
                refreshCommittedWorkingSet: async (committedRevision: number) => {
                    await options.refresh?.(committedRevision)
                },
                release: () => options.release?.(),
            }
        },
    }
}

describe('native file jobs', () => {
    it('restores an official snapshot without transferring its database bytes through IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const events: string[] = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-official-account-snapshot',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 12,
                    sourceBytes: 16_384,
                    sourceSha256: 'b'.repeat(64),
                    characterCount: 4,
                    presetCount: 2,
                    warningCodes: [],
                    recoveryPath: 'C:\\app\\persistent\\recovery\\risulossless-recovery-official.risulossless',
                }),
                kind: 'restore-official-account-snapshot',
            },
        ]

        const result = await runNativeOfficialAccountSnapshotRestore(
            restoreRuntime(11, {
                acquire: () => { events.push('fence-acquired') },
                refresh: () => { events.push('refreshed') },
                release: () => { events.push('fence-released') },
            }),
            {
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
            },
            { afterRefresh: () => { events.push('plugins-reloaded') } },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'official-restore' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_finalize') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toMatchObject({
            kind: 'activated',
            revision: 12,
            recoveryPath: expect.stringContaining('risulossless-recovery-official'),
        })
        expect(events).toEqual([
            'fence-acquired',
            'refreshed',
            'plugins-reloaded',
            'fence-released',
        ])
        expect(calls[0]).toEqual(['native_file_job_start', {
            request: {
                kind: 'restore-official-account-snapshot',
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
                expectedRevision: 11,
            },
        }])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
        expect(JSON.stringify(calls)).not.toContain('databaseBytes')
    })

    it('returns a missing official snapshot without taking the replacement fence', async () => {
        const acquire = vi.fn()
        const commands: string[] = []

        const result = await runNativeOfficialAccountSnapshotRestore(
            restoreRuntime(11, { acquire }),
            {
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'official-missing' }
                    if (command === 'native_file_job_status') return {
                        ...status('failed'),
                        kind: 'restore-official-account-snapshot',
                        state: 'failed',
                        phase: 'complete',
                        error: {
                            code: 'remote-missing',
                            message: 'No official account snapshot exists',
                        },
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual({ kind: 'missing' })
        expect(acquire).not.toHaveBeenCalled()
        expect(commands).not.toContain('native_file_job_finalize')
    })

    it('maps a legacy compatibility result without taking the replacement fence', async () => {
        const acquire = vi.fn()

        const result = await runNativeOfficialAccountSnapshotRestore(
            restoreRuntime(11, { acquire }),
            {
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    if (command === 'native_file_job_start') return { jobId: 'official-legacy' }
                    if (command === 'native_file_job_status') return {
                        ...status('failed'),
                        kind: 'restore-official-account-snapshot',
                        state: 'failed',
                        phase: 'complete',
                        error: {
                            code: 'compatibility-required',
                            message: 'Legacy snapshot requires preparation',
                        },
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual({ kind: 'compatibility-fallback' })
        expect(acquire).not.toHaveBeenCalled()
    })

    it('keeps unavailable lossless backup capability as a structured native error', async () => {
        const calls: string[] = []

        await expect(runNativeLosslessBackupExport(
            {
                revision: 5,
                flushPendingData: async () => undefined,
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risulossless' },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    calls.push(command)
                    throw {
                        code: 'capability-unavailable',
                        message: 'native lossless backup requires v2 asset and cold authority',
                    }
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
            },
        )).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'capability-unavailable',
        })
        expect(calls).toEqual(['native_file_job_start'])
    })

    it('restores a lossless package through the existing destructive replacement fence', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const events: string[] = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-lossless-backup',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 18,
                    sourceBytes: 4096,
                    sourceSha256: '8'.repeat(64),
                    characterCount: 3,
                    presetCount: 2,
                    warningCodes: [],
                    recoveryPath: 'C:\\app\\persistent\\recovery\\risulossless-recovery-123e4567-e89b-42d3-a456-426614174000.risulossless',
                }),
                kind: 'restore-lossless-backup',
            },
        ]

        const result = await runNativeLosslessBackupRestore(
            restoreRuntime(17, {
                acquire: () => { events.push('fence-acquired') },
                refresh: () => { events.push('refreshed') },
                release: () => { events.push('fence-released') },
            }),
            { type: 'androidSpool', token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557' },
            { afterRefresh: () => { events.push('plugins-reloaded') } },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'lossless-restore' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_finalize') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(18)
        expect(result.recoveryPath).toContain('risulossless-recovery-')
        expect(events).toEqual([
            'fence-acquired',
            'refreshed',
            'plugins-reloaded',
            'fence-released',
        ])
        expect(calls).toEqual([
            ['native_file_job_start', {
                request: {
                    kind: 'restore-lossless-backup',
                    source: {
                        type: 'androidSpool',
                        token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
                    },
                    expectedRevision: 17,
                },
            }],
            ['native_file_job_status', { jobId: 'lossless-restore' }],
            ['native_file_job_finalize', { jobId: 'lossless-restore' }],
            ['native_file_job_status', { jobId: 'lossless-restore' }],
            ['native_file_job_forget', { jobId: 'lossless-restore' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('retains a committed lossless restore when renderer refresh fails', async () => {
        const commands: string[] = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-lossless-backup',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 19,
                    sourceBytes: 4096,
                    sourceSha256: '9'.repeat(64),
                    characterCount: 3,
                    presetCount: 2,
                    warningCodes: [],
                    recoveryPath: 'C:\\app\\persistent\\recovery\\risulossless-recovery-123e4567-e89b-42d3-a456-426614174001.risulossless',
                }),
                kind: 'restore-lossless-backup',
            },
        ]

        await expect(runNativeLosslessBackupRestore(
            restoreRuntime(18, {
                refresh: () => { throw new Error('refresh failed') },
            }),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risulossless' },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'lossless-restore' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_finalize') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )).rejects.toMatchObject({
            name: 'NativeFileJobActivationCommittedError',
            committedRevision: 19,
            recoveryRequired: true,
        })
        expect(commands).not.toContain('native_file_job_forget')
    })

    it('exports a complete lossless package to a desktop destination without bytes in IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const terminal: NativeFileJobStatus = {
            jobId: 'lossless-export',
            kind: 'export-lossless-backup',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 8192,
                totalBytes: 8192,
                completedItems: 8,
                totalItems: 8,
            },
            result: {
                revision: 23,
                sourceBytes: 4096,
                sourceSha256: 'a'.repeat(64),
                characterCount: 3,
                presetCount: 2,
                warningCodes: [],
            },
        }

        const result = await runNativeLosslessBackupExport(
            {
                revision: 22,
                flushPendingData: async (reason) => {
                    calls.push([`flush:${reason}`, undefined])
                },
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risulossless' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'lossless-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
            },
        )

        expect(result).toEqual(terminal.result)
        expect(calls).toEqual([
            ['flush:native-lossless-backup-export', undefined],
            ['native_file_job_start', {
                request: {
                    kind: 'export-lossless-backup',
                    destination: 'C:\\chosen\\backup.risulossless',
                    expectedRevision: 22,
                },
            }],
            ['native_file_job_status', { jobId: 'lossless-export' }],
            ['native_file_job_forget', { jobId: 'lossless-export' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('hands a managed lossless export to Android SAF and cleans the native source', async () => {
        const events: string[] = []
        const observedStatuses: NativeFileJobStatus[] = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risulossless-123e4567-e89b-42d3-a456-426614174002.risulossless'
        const terminal: NativeFileJobStatus = {
            jobId: 'lossless-export',
            kind: 'export-lossless-backup',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 4096,
                completedItems: 8,
            },
            result: {
                revision: 22,
                sourceBytes: 4096,
                sourceSha256: 'b'.repeat(64),
                characterCount: 3,
                presetCount: 2,
                warningCodes: [],
                handoffPath,
            },
        }

        const result = await runNativeLosslessBackupExport(
            { revision: 22, flushPendingData: async () => undefined },
            { type: 'androidSaf', suggestedName: 'backup.risulossless' },
            { onStatus: (status) => observedStatuses.push(status) },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    events.push(`${command}:${JSON.stringify(args ?? {})}`)
                    if (command === 'native_file_job_start') return { jobId: 'lossless-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_lossless_handoff_cleanup') return undefined
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async (request) => {
                    events.push(`saf:${request.sourcePath}:${request.suggestedName}`)
                    request.onProgress?.({
                        requestId: 'android-export',
                        operation: 'destination-copy',
                        copiedBytes: 2048,
                        totalBytes: 4096,
                        token: null,
                    })
                    return {
                        bytes: 4096,
                        warningCodes: ['android-saf-provider-not-atomic'],
                    }
                },
            },
        )

        expect(result.handoffPath).toBeUndefined()
        expect(result.warningCodes).toEqual(['android-saf-provider-not-atomic'])
        expect(observedStatuses.at(-1)).toMatchObject({
            state: 'running',
            phase: 'publishing-destination',
            progress: { completedBytes: 2048, totalBytes: 4096 },
        })
        expect(events).toEqual([
            'native_file_job_start:{"request":{"kind":"export-lossless-backup","expectedRevision":22}}',
            'native_file_job_status:{"jobId":"lossless-export"}',
            `saf:${handoffPath}:backup.risulossless`,
            `native_lossless_handoff_cleanup:{"path":"${handoffPath.replaceAll('\\', '\\\\')}"}`,
            'native_file_job_forget:{"jobId":"lossless-export"}',
        ])
    })

    it('rejects a short Android SAF handoff and still cleans both native receipts', async () => {
        const commands: string[] = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risulossless-123e4567-e89b-42d3-a456-426614174003.risulossless'
        const terminal: NativeFileJobStatus = {
            jobId: 'lossless-export',
            kind: 'export-lossless-backup',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 4096, completedItems: 1 },
            result: {
                revision: 22,
                sourceBytes: 4096,
                sourceSha256: 'b'.repeat(64),
                characterCount: 0,
                presetCount: 0,
                warningCodes: [],
                handoffPath,
            },
        }

        await expect(runNativeLosslessBackupExport(
            { revision: 22, flushPendingData: async () => undefined },
            { type: 'androidSaf', suggestedName: 'backup.risulossless' },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'lossless-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_lossless_handoff_cleanup') return undefined
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 2048, warningCodes: [] }),
            },
        )).rejects.toMatchObject({ code: 'length-mismatch' })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_lossless_handoff_cleanup',
            'native_file_job_forget',
        ])
    })

    it('restores from a descriptor without sending file or database bytes through IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const statuses = [
            status('running'),
            {
                ...status('waitingForInput'),
                phase: 'awaiting-activation' as const,
            },
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
        const runtime = restoreRuntime(3, {
            capture: () => {
                calls.push(['capture:native-block-risu-save-restore', undefined])
            },
            refresh: (revision) => {
                refreshed.push(revision)
            },
        })

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
                    if (command === 'native_file_job_finalize') return 'requested'
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
            ['capture:native-block-risu-save-restore', undefined],
            ['native_file_job_start', {
                request: {
                    kind: 'restore-block-risu-save',
                    source: { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
                    expectedRevision: 3,
                },
            }],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['native_file_job_finalize', { jobId: 'job-1' }],
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
            restoreRuntime(7, {
                refresh: () => {
                    throw new Error('cancelled restore must not refresh')
                },
            }),
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
            restoreRuntime(2, { capture: () => controller.abort() }),
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

    it('discards an unclaimed Android source when cancellation arrives during mutation capture', async () => {
        const controller = new AbortController()
        const token = '2c4d33fe-2e29-4625-bb1e-c8d1084f9557'
        const commands: string[] = []
        const discarded: string[] = []

        await expect(runNativeLosslessBackupRestore(
            restoreRuntime(2, { capture: () => controller.abort() }),
            { type: 'androidSpool', token },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                discardAndroidSource: (sourceToken) => {
                    discarded.push(sourceToken)
                    return true
                },
            },
        )).rejects.toMatchObject({ name: 'AbortError' })
        expect(discarded).toEqual([token])
        expect(commands).toEqual([])
    })

    it('discards an unclaimed Android source when restore starts already cancelled', async () => {
        const controller = new AbortController()
        const token = '2c4d33fe-2e29-4625-bb1e-c8d1084f9557'
        const discarded: string[] = []
        controller.abort()

        await expect(runNativeLosslessBackupRestore(
            restoreRuntime(2),
            { type: 'androidSpool', token },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                discardAndroidSource: (sourceToken) => {
                    discarded.push(sourceToken)
                    return true
                },
            },
        )).rejects.toMatchObject({ name: 'AbortError' })
        expect(discarded).toEqual([token])
    })

    it('reports cleanup failure when an unclaimed Android source cannot be discarded', async () => {
        const controller = new AbortController()
        controller.abort()

        await expect(runNativeLosslessBackupRestore(
            restoreRuntime(2),
            {
                type: 'androidSpool',
                token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
            },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                discardAndroidSource: () => false,
            },
        )).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'cleanup-failed',
        })
    })

    it('normalizes Android source discard exceptions as cleanup failure', async () => {
        const controller = new AbortController()
        controller.abort()

        await expect(runNativeLosslessBackupRestore(
            restoreRuntime(2),
            {
                type: 'androidSpool',
                token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
            },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                discardAndroidSource: () => {
                    throw new Error('bridge unavailable')
                },
            },
        )).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'cleanup-failed',
        })
    })

    it('cancels staged data instead of activating when a live edit invalidates the token', async () => {
        const commands: string[] = []
        const statuses = [
            { ...status('waitingForInput'), phase: 'awaiting-activation' as const },
            status('cancelled'),
        ]

        await expect(runNativeBlockRisuSaveRestore(
            restoreRuntime(3, {
                acquire: () => {
                    throw new Error('mutation generation changed')
                },
            }),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_file_job_cancel') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )).rejects.toMatchObject({ code: 'revision-conflict' })

        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
        expect(commands).not.toContain('native_file_job_finalize')
    })

    it('holds the replacement fence through refresh, plugin reload, and acknowledgement', async () => {
        const events: string[] = []
        const observedPhases: string[] = []
        let statusCount = 0
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'f'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        await runNativeBlockRisuSaveRestore(
            restoreRuntime(8, {
                acquire: () => { events.push('fence-acquired') },
                refresh: () => { events.push('working-set-refreshed') },
                release: () => events.push('fence-released'),
            }),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            {
                afterRefresh: () => { events.push('plugins-reloaded') },
                onStatus: (status) => observedPhases.push(status.phase),
            },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? { ...status('waitingForInput'), phase: 'awaiting-activation' }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize') {
                        events.push('native-finalized')
                        return 'requested'
                    }
                    if (command === 'native_file_job_forget') {
                        events.push('terminal-acknowledged')
                        return true
                    }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(events).toEqual([
            'fence-acquired',
            'native-finalized',
            'working-set-refreshed',
            'plugins-reloaded',
            'terminal-acknowledged',
            'fence-released',
        ])
        expect(observedPhases).toContain('activating-database')
    })

    it('retains a committed job and exposes recovery state when refreshing fails', async () => {
        const commands: string[] = []
        let statusCount = 0
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'b'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        const promise = runNativeBlockRisuSaveRestore(
            restoreRuntime(8, {
                refresh: () => {
                    throw new Error('refresh unavailable')
                },
            }),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? { ...status('waitingForInput'), phase: 'awaiting-activation' }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize') return 'requested'
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
            'native_file_job_finalize',
            'native_file_job_status',
        ])
    })

    it('keeps committed success when terminal acknowledgement fails', async () => {
        const commands: string[] = []
        let statusCount = 0
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'c'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        const result = await runNativeBlockRisuSaveRestore(
            restoreRuntime(8),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? { ...status('waitingForInput'), phase: 'awaiting-activation' }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize') return 'requested'
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
            'native_file_job_finalize',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('acknowledges terminal failure even when translating it to an exception', async () => {
        const commands: string[] = []
        const failed = status('failed')
        failed.error = { code: 'corrupt-input', message: 'invalid gzip data' }

        await expect(runNativeBlockRisuSaveRestore(
            restoreRuntime(3),
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
            restoreRuntime(1),
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
