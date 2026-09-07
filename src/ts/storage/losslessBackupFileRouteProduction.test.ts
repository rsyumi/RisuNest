import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    platform: 'desktop' as 'desktop' | 'android' | 'web',
    open: vi.fn(),
    save: vi.fn(),
    pickAndroidSource: vi.fn(),
    saveLegacy: vi.fn(),
    loadLegacy: vi.fn(),
    restore: vi.fn(),
    export: vi.fn(),
    reloadPlugins: vi.fn(),
    runtime: { revision: 4 },
}))

vi.mock('@tauri-apps/plugin-dialog', () => ({
    open: mocks.open,
    save: mocks.save,
}))
vi.mock('./androidSafBridge', () => ({
    pickAndroidLosslessBackupSource: mocks.pickAndroidSource,
}))
vi.mock('../platform', () => ({
    get isTauri() { return mocks.platform !== 'web' },
    get isTauriAndroid() { return mocks.platform === 'android' },
    get isTauriDesktop() { return mocks.platform === 'desktop' },
}))
vi.mock('../drive/backuplocal', () => ({
    SaveLocalBackup: mocks.saveLegacy,
    LoadLocalBackup: mocks.loadLegacy,
}))
vi.mock('../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: mocks.reloadPlugins,
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => mocks.runtime,
}))
vi.mock('./nativeFileJobs', () => ({
    runNativeLosslessBackupRestore: mocks.restore,
    runNativeLosslessBackupExport: mocks.export,
}))
vi.mock('./nativeFileJobManager', () => ({
    nativeFileOperation: {},
    runSharedNativeFileOperation: async (_kind: string, _key: string, operation: (context: unknown) => unknown) =>
        operation({
            signal: new AbortController().signal,
            onStatus: vi.fn(),
            setBlocking: vi.fn(),
            setSource: vi.fn(),
            setPartialWritesPossible: vi.fn(),
        }),
}))

import {
    exportLocalBackupFromSystemPicker,
    restoreLocalBackupFromSystemPicker,
} from './losslessBackupFileRouteProduction.svelte'

describe('lossless local backup production caller', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.platform = 'desktop'
        mocks.open.mockResolvedValue('C:\\picked\\backup.risulossless')
        mocks.save.mockResolvedValue('C:\\picked\\new.risulossless')
        mocks.pickAndroidSource.mockResolvedValue({
            type: 'androidSpool',
            token: '55555555-5555-4555-8555-555555555555',
        })
        mocks.restore.mockResolvedValue({
            revision: 5,
            sourceBytes: 1024,
            sourceSha256: 'a'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        })
        mocks.export.mockResolvedValue({
            revision: 4,
            sourceBytes: 2048,
            sourceSha256: 'b'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        })
    })

    it('connects the desktop system picker to native lossless export', async () => {
        await exportLocalBackupFromSystemPicker()

        expect(mocks.save).toHaveBeenCalledWith(expect.objectContaining({
            filters: [{ name: 'RisuNest Lossless Backup', extensions: ['risulossless'] }],
        }))
        expect(mocks.export).toHaveBeenCalledWith(
            mocks.runtime,
            { type: 'desktopPath', path: 'C:\\picked\\new.risulossless' },
            expect.any(Object),
        )
        expect(mocks.saveLegacy).not.toHaveBeenCalled()
    })

    it('uses the Android owned spool picker without moving bytes through WebView', async () => {
        mocks.platform = 'android'

        await restoreLocalBackupFromSystemPicker()

        expect(mocks.open).not.toHaveBeenCalled()
        expect(mocks.pickAndroidSource).toHaveBeenCalledOnce()
        expect(mocks.restore).toHaveBeenCalledWith(
            mocks.runtime,
            {
                type: 'androidSpool',
                token: '55555555-5555-4555-8555-555555555555',
            },
            expect.any(Object),
        )
    })

    it('preserves the legacy local backup implementation on Web', async () => {
        mocks.platform = 'web'

        await exportLocalBackupFromSystemPicker()
        await restoreLocalBackupFromSystemPicker()

        expect(mocks.saveLegacy).toHaveBeenCalledOnce()
        expect(mocks.loadLegacy).toHaveBeenCalledOnce()
        expect(mocks.open).not.toHaveBeenCalled()
        expect(mocks.save).not.toHaveBeenCalled()
        expect(mocks.restore).not.toHaveBeenCalled()
        expect(mocks.export).not.toHaveBeenCalled()
    })
})
