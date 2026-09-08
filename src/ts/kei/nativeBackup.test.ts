import { describe, expect, it, vi } from 'vitest'

import { nativePersistentRevisionLease } from '../storage/nativePersistentExport'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import type { PersistentRevisionLease } from '../storage/persistentDataStore'
import { runNativeKeiBackupJob, tryNativeKeiBackup } from './nativeBackup'

function nativeRuntime() {
    const release = vi.fn(async () => undefined)
    const acquireRevision = vi.fn(
        async (revision: number): Promise<PersistentRevisionLease> => ({
            revision,
            [nativePersistentRevisionLease]: 'lease-7',
            readRoot: vi.fn(),
            queryPresets: vi.fn(),
            readPreset: vi.fn(),
            queryCharacters: vi.fn(),
            readCharacter: vi.fn(),
            queryConversations: vi.fn(),
            readConversation: vi.fn(),
            readConversationMetadata: vi.fn(),
            readConversationWindow: vi.fn(),
            queryPluginStorage: vi.fn(),
            readPluginStorage: vi.fn(),
            readAssetAlias: vi.fn(),
            readAssetAliasesByKeys: vi.fn(),
            listAssetAliases: vi.fn(),
            readAssetRepositoryAuthority: vi.fn(),
            readAssetOwnerHead: vi.fn(),
            readColdPayloadAuthority: vi.fn(),
            readColdAlias: vi.fn(),
            listColdAliases: vi.fn(),
            release,
        }) as PersistentRevisionLease,
    )
    const flushPendingData = vi.fn(async () => undefined)
    const runtime = {
        revision: 7,
        store: { acquireRevision },
        flushPendingData,
    } as unknown as PersistentDataRuntime
    return { runtime, acquireRevision, flushPendingData, release }
}

describe('tryNativeKeiBackup', () => {
    it('uploads a pinned revision without passing database or body bytes over IPC', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
            revision: 7,
            bytes: 1234,
            status: 503,
        }))

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => true,
                    invoke,
                },
            ),
        ).resolves.toBe(true)

        expect(harness.flushPendingData).toHaveBeenCalledWith('kei-auto-backup')
        expect(harness.acquireRevision).toHaveBeenCalledWith(7)
        expect(invoke).toHaveBeenCalledWith('pds_kei_backup_upload', {
            lease: 'lease-7',
            url: 'https://kei.example/autobackup/save',
            expectedAccountId: 'account-1',
            token: 'secret-token',
        })
        expect(invoke.mock.calls[0][1]).not.toHaveProperty('database')
        expect(invoke.mock.calls[0][1]).not.toHaveProperty('body')
        expect(harness.release).toHaveBeenCalledOnce()
    })

    it('keeps the existing path when native persistence is unavailable', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn()

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => false,
                    invoke,
                },
            ),
        ).resolves.toBe(false)

        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.acquireRevision).not.toHaveBeenCalled()
        expect(invoke).not.toHaveBeenCalled()
    })

    it('falls back after releasing a non-native revision lease', async () => {
        const harness = nativeRuntime()
        const release = vi.fn(async () => undefined)
        harness.acquireRevision.mockResolvedValueOnce({
            revision: 7,
            readRoot: vi.fn(),
            queryPresets: vi.fn(),
            readPreset: vi.fn(),
            queryCharacters: vi.fn(),
            readCharacter: vi.fn(),
            queryConversations: vi.fn(),
            readConversation: vi.fn(),
            readConversationMetadata: vi.fn(),
            readConversationWindow: vi.fn(),
            queryPluginStorage: vi.fn(),
            readPluginStorage: vi.fn(),
            readAssetAlias: vi.fn(),
            readAssetAliasesByKeys: vi.fn(),
            listAssetAliases: vi.fn(),
            readAssetRepositoryAuthority: vi.fn(),
            readAssetOwnerHead: vi.fn(),
            readColdPayloadAuthority: vi.fn(),
            readColdAlias: vi.fn(),
            listColdAliases: vi.fn(),
            release,
        })
        const invoke = vi.fn()

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => true,
                    invoke,
                },
            ),
        ).resolves.toBe(false)

        expect(release).toHaveBeenCalledOnce()
        expect(invoke).not.toHaveBeenCalled()
    })

    it('releases the lease and preserves the upload error', async () => {
        const harness = nativeRuntime()
        const uploadError = new Error('offline')
        const invoke = vi.fn(async () => {
            throw uploadError
        })

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => true,
                    invoke,
                },
            ),
        ).rejects.toBe(uploadError)

        expect(harness.release).toHaveBeenCalledOnce()
    })
})

describe('runNativeKeiBackupJob', () => {
    it('starts a lease-owned job, releases the handoff lease, and waits for success', async () => {
        const harness = nativeRuntime()
        const warn = vi.fn()
        const invoke = vi.fn()
            .mockResolvedValueOnce({ jobId: 'kei-job-1', warningCodes: [] })
            .mockResolvedValueOnce({
                jobId: 'kei-job-1',
                kind: 'kei-backup-upload',
                state: 'running',
                phase: 'writing-export',
                progress: {
                    completedBytes: 10,
                    completedItems: 1,
                },
            })
            .mockResolvedValueOnce({
                jobId: 'kei-job-1',
                kind: 'kei-backup-upload',
                state: 'succeeded',
                phase: 'complete',
                progress: {
                    completedBytes: 20,
                    totalBytes: 20,
                    completedItems: 2,
                    totalItems: 2,
                },
                result: {
                    revision: 7,
                    sourceBytes: 20,
                    sourceSha256: 'a'.repeat(64),
                    characterCount: 1,
                    presetCount: 2,
                    warningCodes: ['cleanup-failed'],
                },
            })
            .mockResolvedValueOnce(true)
        const wait = vi.fn(async () => undefined)

        await expect(runNativeKeiBackupJob(
            {
                runtime: harness.runtime,
                url: 'https://kei.example/autobackup/save',
                accountId: 'account-1',
                token: 'secret-token',
            },
            { isTauri: () => true, invoke, wait, warn },
        )).resolves.toBe(true)

        expect(harness.flushPendingData).toHaveBeenCalledWith('kei-auto-backup')
        expect(invoke.mock.calls).toEqual([
            ['native_file_job_start', {
                request: {
                    kind: 'kei-backup-upload',
                    lease: 'lease-7',
                    expectedRevision: 7,
                    url: 'https://kei.example/autobackup/save',
                    expectedAccountId: 'account-1',
                    token: 'secret-token',
                },
            }],
            ['native_file_job_status', { jobId: 'kei-job-1' }],
            ['native_file_job_status', { jobId: 'kei-job-1' }],
            ['native_file_job_forget', { jobId: 'kei-job-1' }],
        ])
        expect(harness.release).toHaveBeenCalledOnce()
        expect(wait).toHaveBeenCalledOnce()
        expect(warn).toHaveBeenCalledWith('cleanup-failed')
    })

    it('falls back only when native capability is unavailable before a job is accepted', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn().mockRejectedValueOnce({
            code: 'capability-unavailable',
            message: 'native jobs disabled',
        })

        await expect(runNativeKeiBackupJob(
            {
                runtime: harness.runtime,
                url: 'https://kei.example/autobackup/save',
                accountId: 'account-1',
                token: 'secret-token',
            },
            { isTauri: () => true, invoke, wait: vi.fn() },
        )).resolves.toBe(false)

        expect(harness.release).toHaveBeenCalledOnce()
    })

    it('does not materialize the JavaScript fallback when native workers are busy', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn().mockRejectedValueOnce({
            code: 'job-capacity',
            message: 'native file job concurrency limit reached',
        })

        await expect(runNativeKeiBackupJob(
            {
                runtime: harness.runtime,
                url: 'https://kei.example/autobackup/save',
                accountId: 'account-1',
                token: 'secret-token',
            },
            { isTauri: () => true, invoke, wait: vi.fn() },
        )).rejects.toMatchObject({
            name: 'NativeKeiBackupJobError',
            code: 'job-capacity',
        })

        expect(harness.release).toHaveBeenCalledOnce()
    })

    it('does not expose a fallback after an accepted job fails', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn()
            .mockResolvedValueOnce({ jobId: 'kei-job-1', warningCodes: [] })
            .mockResolvedValueOnce({
                jobId: 'kei-job-1',
                kind: 'kei-backup-upload',
                state: 'failed',
                phase: 'complete',
                progress: { completedBytes: 10, completedItems: 1 },
                error: { code: 'transport-failed', message: 'connection reset' },
            })
            .mockResolvedValueOnce(true)

        await expect(runNativeKeiBackupJob(
            {
                runtime: harness.runtime,
                url: 'https://kei.example/autobackup/save',
                accountId: 'account-1',
                token: 'secret-token',
            },
            { isTauri: () => true, invoke, wait: vi.fn() },
        )).rejects.toMatchObject({
            name: 'NativeKeiBackupJobError',
            code: 'transport-failed',
            message: 'connection reset',
        })

        expect(harness.release).toHaveBeenCalledOnce()
        expect(invoke).toHaveBeenLastCalledWith('native_file_job_forget', {
            jobId: 'kei-job-1',
        })
    })
})
