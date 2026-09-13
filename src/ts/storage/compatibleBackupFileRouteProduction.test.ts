import { beforeEach, describe, expect, it, vi } from 'vitest'

import type {
    NativeFileJobOptions,
    NativeFileJobStatus,
} from './nativeFileJobs'

const mocks = vi.hoisted(() => ({
    platform: 'desktop' as 'desktop' | 'android' | 'web',
    save: vi.fn(),
    runExport: vi.fn(),
    assertAvailable: vi.fn(),
    resume: vi.fn(),
    manager: vi.fn(),
    managedStatus: vi.fn(),
    managedController: new AbortController(),
    runtime: { revision: 4, flushPendingData: vi.fn() },
}))

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: mocks.save }))
vi.mock('../platform', () => ({
    get isTauri() {
        return mocks.platform !== 'web'
    },
    get isTauriAndroid() {
        return mocks.platform === 'android'
    },
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => mocks.runtime,
}))
vi.mock('./sync/serverSyncProduction', () => ({
    getServerSyncController: () => ({
        assertFileOperationAvailable: mocks.assertAvailable,
    }),
    resumeServerSyncAfterBackup: mocks.resume,
}))
vi.mock('./nativeFileJobManager', () => ({
    runSharedNativeFileOperation: mocks.manager,
}))
vi.mock('./nativeFileJobs', async (original) => ({
    ...(await original<object>()),
    runNativeCompatibleLocalBackupExport: mocks.runExport,
}))

import { exportCompatibilityBackupFromSystemPicker } from './compatibleBackupFileRouteProduction.svelte'

const result = {
    revision: 4,
    sourceBytes: 1024,
    sourceSha256: 'a'.repeat(64),
    characterCount: 1,
    presetCount: 0,
    warningCodes: [],
}

describe('compatibility backup production picker', () => {
    beforeEach(() => {
        vi.resetAllMocks()
        mocks.platform = 'desktop'
        mocks.managedController = new AbortController()
        mocks.save.mockResolvedValue('C:\\picked\\backup.bin')
        mocks.runExport.mockResolvedValue(result)
        mocks.manager.mockImplementation(async (_kind, _key, operation) =>
            operation({
                signal: mocks.managedController.signal,
                onStatus: mocks.managedStatus,
            }),
        )
    })

    it.each(['risuai', 'pocketrisu'] as const)(
        'routes the %s picker and completed report',
        async (target) => {
            const report = {
                target,
                preserved: [],
                converted: [],
                excluded: [],
            }
            const status: NativeFileJobStatus = {
                jobId: 'synthetic-export',
                kind: 'export-compatible-local-backup',
                state: 'succeeded',
                phase: 'complete',
                progress: { completedBytes: 1024, completedItems: 1 },
                result,
                compatibilityReport: report,
            }
            mocks.runExport.mockImplementation(
                async (
                    _runtime,
                    _target,
                    _destination,
                    options: NativeFileJobOptions,
                ) => {
                    options.onStatus?.(status)
                    return result
                },
            )
            const onStatus = vi.fn()
            const onReport = vi.fn()
            const exported = await exportCompatibilityBackupFromSystemPicker(
                target,
                { onStatus, onReport },
            )
            expect(mocks.assertAvailable).toHaveBeenCalledOnce()
            expect(mocks.save).toHaveBeenCalledWith({
                defaultPath: `${target}-backup.bin`,
                filters: [
                    {
                        name:
                            target === 'risuai'
                                ? 'RisuAI Backup'
                                : 'PocketRisu Backup',
                        extensions: ['bin'],
                    },
                ],
            })
            expect(mocks.manager).toHaveBeenCalledWith(
                'export',
                `compatibility-backup-export:${target}`,
                expect.any(Function),
                { format: 'library-backup' },
            )
            expect(mocks.runExport).toHaveBeenCalledWith(
                mocks.runtime,
                target,
                { type: 'desktopPath', path: 'C:\\picked\\backup.bin' },
                expect.objectContaining({ signal: expect.any(AbortSignal) }),
            )
            expect(mocks.managedStatus).toHaveBeenCalledWith(status)
            expect(onStatus).toHaveBeenCalledWith(status)
            expect(onReport).toHaveBeenCalledWith(report)
            expect(exported).toEqual({ ...result, compatibilityReport: report })
            expect(mocks.resume).toHaveBeenCalledOnce()
        },
    )

    it('returns null on picker cancellation without starting a native job', async () => {
        mocks.save.mockResolvedValue(null)
        expect(
            await exportCompatibilityBackupFromSystemPicker('risuai'),
        ).toBeNull()
        expect(mocks.runExport).not.toHaveBeenCalled()
        expect(mocks.resume).toHaveBeenCalledOnce()
    })

    it('passes an Android SAF destination without opening the desktop picker', async () => {
        mocks.platform = 'android'
        await exportCompatibilityBackupFromSystemPicker('pocketrisu')
        expect(mocks.save).not.toHaveBeenCalled()
        expect(mocks.runExport).toHaveBeenCalledWith(
            mocks.runtime,
            'pocketrisu',
            { type: 'androidSaf', suggestedName: 'pocketrisu-backup.bin' },
            expect.any(Object),
        )
        expect(mocks.resume).toHaveBeenCalledOnce()
    })

    it('bridges manager cancellation while the system picker is open', async () => {
        mocks.save.mockImplementation(async () => {
            mocks.managedController.abort()
            return 'C:\\picked\\backup.bin'
        })
        await expect(
            exportCompatibilityBackupFromSystemPicker('risuai'),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.runExport).not.toHaveBeenCalled()
        expect(mocks.resume).toHaveBeenCalledOnce()
    })

    it('bridges the caller cancellation signal into the native job and resumes after failure', async () => {
        const external = new AbortController()
        mocks.runExport.mockImplementation(
            async (
                _runtime,
                _target,
                _destination,
                options: NativeFileJobOptions,
            ) => {
                external.abort()
                expect(options.signal?.aborted).toBe(true)
                throw new DOMException('synthetic cancellation', 'AbortError')
            },
        )
        await expect(
            exportCompatibilityBackupFromSystemPicker('risuai', {
                signal: external.signal,
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.resume).toHaveBeenCalledOnce()
    })

    it('does not substitute a different exporter on the web', async () => {
        mocks.platform = 'web'
        await expect(
            exportCompatibilityBackupFromSystemPicker('risuai'),
        ).rejects.toMatchObject({ code: 'native-required' })
        expect(mocks.manager).not.toHaveBeenCalled()
        expect(mocks.save).not.toHaveBeenCalled()
        expect(mocks.runExport).not.toHaveBeenCalled()
    })

    it('rejects a busy synchronization operation before opening a picker', async () => {
        mocks.assertAvailable.mockImplementation(() => {
            throw new Error('synthetic sync busy')
        })
        await expect(
            exportCompatibilityBackupFromSystemPicker('risuai'),
        ).rejects.toThrow('synthetic sync busy')
        expect(mocks.manager).not.toHaveBeenCalled()
        expect(mocks.save).not.toHaveBeenCalled()
    })
})
