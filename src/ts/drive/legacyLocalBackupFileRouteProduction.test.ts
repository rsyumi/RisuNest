import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    android: false,
    open: vi.fn(),
    save: vi.fn(),
    pickAndroidSource: vi.fn(),
    restore: vi.fn(),
    export: vi.fn(),
    reloadPlugins: vi.fn(async () => undefined),
    describeSource: vi.fn(async (path: string) => ({ name: `described:${path}`, bytes: 512 })),
    runtime: {
        revision: 3,
        flushPendingData: vi.fn(async () => undefined),
        capturePersistentMutationToken: vi.fn(async () => ({ revision: 3, mutationGeneration: 1 })),
        acquireDestructiveReplacementFence: vi.fn(async () => ({
            refreshCommittedWorkingSet: vi.fn(async () => undefined),
            release: vi.fn(),
        })),
    },
    operations: [] as Array<{ kind: string; key: string; options: unknown }>,
    context: {
        signal: new AbortController().signal,
        onStatus: vi.fn(),
        setBlocking: vi.fn(),
        setSource: vi.fn(),
        setPartialWritesPossible: vi.fn(),
    },
}))

vi.mock('@tauri-apps/plugin-dialog', () => ({ open: mocks.open, save: mocks.save }))
vi.mock('../platform', () => ({
    get isTauriAndroid() {
        return mocks.android
    },
}))
vi.mock('../storage/androidSafBridge', () => ({
    pickAndroidLegacyBackupSource: mocks.pickAndroidSource,
}))
vi.mock('../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: mocks.reloadPlugins,
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => mocks.runtime,
}))
vi.mock('../storage/nativeFileSourceInfo', () => ({
    describeDesktopSource: mocks.describeSource,
}))
vi.mock('../storage/nativeFileJobs', async (importOriginal) => ({
    ...await importOriginal<typeof import('../storage/nativeFileJobs')>(),
    runNativeLegacyLocalBackupRestore: mocks.restore,
    runNativeLegacyLocalBackupExport: mocks.export,
}))
vi.mock('../storage/nativeFileJobManager', () => ({
    runSharedNativeFileOperation: async (
        kind: string,
        key: string,
        operation: (context: unknown) => unknown,
        options: unknown,
    ) => {
        mocks.operations.push({ kind, key, options })
        return operation(mocks.context)
    },
}))

import { NativeFileJobError } from '../storage/nativeFileJobs'
import { importLegacyLocalBackupFromSystemPicker } from './legacyLocalBackupFileRouteProduction.svelte'

const committed = {
    revision: 4,
    sourceBytes: 512,
    sourceSha256: 'a'.repeat(64),
    characterCount: 1,
    presetCount: 0,
    warningCodes: [],
}

describe('legacy local backup production caller', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.android = false
        mocks.operations.length = 0
        mocks.open.mockResolvedValue('C:\\picked\\risu-backup.bin')
        mocks.restore.mockResolvedValue(committed)
    })

    it('opens the progress dialog for the local backup format and reports the picked desktop file', async () => {
        const result = await importLegacyLocalBackupFromSystemPicker()

        expect(result).toEqual(committed)
        expect(mocks.operations).toEqual([{
            kind: 'import',
            key: 'legacy-local-backup-import',
            options: { presentation: 'dialog', format: 'local-backup' },
        }])
        expect(mocks.describeSource).toHaveBeenCalledExactlyOnceWith('C:\\picked\\risu-backup.bin')
        expect(mocks.context.setSource).toHaveBeenCalledExactlyOnceWith({
            name: 'described:C:\\picked\\risu-backup.bin',
            bytes: 512,
        })
        expect(mocks.restore).toHaveBeenCalledWith(
            mocks.runtime,
            { type: 'desktopPath', path: 'C:\\picked\\risu-backup.bin' },
            expect.objectContaining({ afterRefresh: mocks.reloadPlugins }),
        )
        expect(mocks.restore.mock.calls[0][2]).not.toHaveProperty('onNativeFallback')
    })

    it('relays the Android spool copy as a copying-source stage and its file name as the source', async () => {
        mocks.android = true
        mocks.pickAndroidSource.mockImplementation(async (options: {
            onProgress?(progress: unknown): void
            onSource?(source: { displayName: string; bytes: number }): void
        }) => {
            options.onProgress?.({
                requestId: 'req-1',
                operation: 'source-copy',
                copiedBytes: 100,
                totalBytes: 400,
                token: null,
            })
            options.onSource?.({ displayName: 'phone-backup.bin', bytes: 400 })
            return { type: 'androidSpool', token: 'spool-token' }
        })

        await importLegacyLocalBackupFromSystemPicker()

        expect(mocks.context.onStatus).toHaveBeenCalledWith(expect.objectContaining({
            jobId: 'req-1',
            kind: 'restore-legacy-local-backup',
            progress: { completedBytes: 100, totalBytes: 400, completedItems: 0, totalItems: 1 },
            detail: expect.objectContaining({ stage: 'copying-source', stageCompleted: 100, stageTotal: 400, stageUnit: 'bytes' }),
        }))
        expect(mocks.context.setSource).toHaveBeenCalledExactlyOnceWith({ name: 'phone-backup.bin', bytes: 400 })
        expect(mocks.describeSource).not.toHaveBeenCalled()
    })

    it('continues in the WebView inside the same operation when the native job cannot read the file', async () => {
        mocks.restore.mockRejectedValueOnce(new NativeFileJobError('unsupported-format', 'legacy JSON inlays'))
        const onNativeFallback = vi.fn(async () => ({ warningCodes: ['pocket-inlay-failed'] }))

        const result = await importLegacyLocalBackupFromSystemPicker({ onNativeFallback })

        expect(result).toEqual({ warningCodes: ['pocket-inlay-failed'] })
        expect(mocks.context.onStatus).toHaveBeenCalledWith(expect.objectContaining({
            detail: expect.objectContaining({ stage: 'awaiting-reselect' }),
        }))
        expect(onNativeFallback).toHaveBeenCalledExactlyOnceWith({
            signal: mocks.context.signal,
            onStatus: mocks.context.onStatus,
            setSource: mocks.context.setSource,
            setPartialWritesPossible: mocks.context.setPartialWritesPossible,
        })
        expect(mocks.operations).toHaveLength(1)
    })

    it('rethrows native failures that are not compatibility fallbacks', async () => {
        mocks.restore.mockRejectedValueOnce(new NativeFileJobError('corrupt-input', 'truncated archive'))
        const onNativeFallback = vi.fn()

        await expect(importLegacyLocalBackupFromSystemPicker({ onNativeFallback })).rejects.toMatchObject({
            code: 'corrupt-input',
        })
        expect(onNativeFallback).not.toHaveBeenCalled()
    })

    it('fails instead of falling back when no WebView importer was offered', async () => {
        mocks.restore.mockRejectedValueOnce(new NativeFileJobError('capability-unavailable', 'no native jobs'))

        await expect(importLegacyLocalBackupFromSystemPicker()).rejects.toMatchObject({
            code: 'capability-unavailable',
        })
    })
})
