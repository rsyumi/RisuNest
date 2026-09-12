import { beforeEach, describe, expect, it, vi } from 'vitest'
const m = vi.hoisted(() => ({
    invoke: vi.fn(),
    open: vi.fn(),
    save: vi.fn(),
    chooseExport: vi.fn(),
    chooseRestore: vi.fn(),
    confirm: vi.fn(),
    portable: vi.fn(),
    block: vi.fn(),
    legacy: vi.fn(),
    export: vi.fn(),
    discard: vi.fn(),
    confirmReplacement: vi.fn(),
    after: vi.fn(),
    resume: vi.fn(),
    hold: vi.fn(),
    manager: vi.fn(),
    controller: new AbortController(),
    runtime: { revision: 4 },
}))
vi.mock('@tauri-apps/api/core', () => ({ invoke: m.invoke }))
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: m.open, save: m.save }))
vi.mock('../platform', () => ({ isTauri: true, isTauriAndroid: false }))
vi.mock('../alert', () => ({ alertConfirm: m.confirm, alertNormal: vi.fn() }))
vi.mock('../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: m.after,
}))
vi.mock('./androidSafBridge', () => ({
    discardAndroidSafSource: m.discard,
    pickAndroidBackupSource: vi.fn(),
}))
vi.mock('./deviceBackup/selectionDialog', () => ({
    selectPortableBackupExport: m.chooseExport,
    selectPortableBackupRestore: m.chooseRestore,
}))
vi.mock('./nativeFileSourceInfo', () => ({
    describeDesktopSource: async () => ({ name: 'misleading.bin', bytes: 30 }),
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => m.runtime,
}))
vi.mock('./nativeFileJobManager', () => ({
    runSharedNativeFileOperation: m.manager,
}))
vi.mock('./sync/serverSyncProduction', () => ({
    getServerSyncController: () => ({
        assertFileOperationAvailable() {},
        withReplacement: async (fn: () => Promise<unknown>) => fn(),
        confirmReplacement: m.confirmReplacement,
    }),
    resumeServerSyncAfterBackup: m.resume,
    holdServerSyncAfterRestore: m.hold,
}))
vi.mock('./nativeFileJobs', async (original) => ({
    ...(await original<object>()),
    runNativeArchiveExport: m.export,
    runNativeArchiveRestore: m.portable,
    runNativeBlockRisuSaveRestore: m.block,
    runNativeLegacyLocalBackupRestore: m.legacy,
}))
import {
    exportPortableBackupFromSystemPicker,
    restoreBackupFromNativeSource,
    restoreBackupFromSystemPicker,
} from './portableBackupFileRouteProduction.svelte'
const result = {
    revision: 5,
    sourceBytes: 30,
    sourceSha256: 'a'.repeat(64),
    characterCount: 1,
    presetCount: 1,
    warningCodes: [],
}
describe('common backup file production route', () => {
    beforeEach(() => {
        vi.resetAllMocks()
        m.controller = new AbortController()
        m.manager.mockImplementation(async (_kind, _label, run) =>
            run({
                signal: m.controller.signal,
                onStatus: vi.fn(),
                setBlocking: vi.fn(),
                setSource: vi.fn(),
            }),
        )
        m.invoke.mockResolvedValue('portable')
        m.confirm.mockResolvedValue(true)
        for (const operation of [m.portable, m.block, m.legacy, m.export])
            operation.mockResolvedValue(result)
        m.chooseExport.mockResolvedValue({
            library: true,
            deviceSections: ['local-storage', 'localforage'],
        })
        m.save.mockResolvedValue('C:\\synthetic\\backup.risunest')
    })
    it.each(['portable', 'block-risu-save', 'local-backup'])(
        'uses detected %s independently of filename',
        async (format) => {
            m.invoke.mockResolvedValue(format)
            expect(
                await restoreBackupFromNativeSource({
                    type: 'desktopPath',
                    path: 'C:\\synthetic\\misleading.bin',
                }),
            ).toEqual(result)
            const selected =
                format === 'portable'
                    ? m.portable
                    : format === 'block-risu-save'
                      ? m.block
                      : m.legacy
            expect(selected).toHaveBeenCalledOnce()
            expect(m.manager).toHaveBeenCalledOnce()
            const options = selected.mock.calls[0][2]
            await options.beforeActivation()
            expect(m.confirmReplacement).toHaveBeenCalledOnce()
            if (format === 'portable') {
                m.chooseRestore.mockResolvedValue({
                    library: true,
                    deviceSections: [],
                })
                await options.choosePortableSections({
                    libraryIncluded: true,
                    repairRequired: false,
                    deviceSections: [],
                })
                expect(m.chooseRestore).toHaveBeenCalledOnce()
            }
            await options.onNativeStatus({ state: 'succeeded', result })
            expect(m.hold).toHaveBeenCalledOnce()
            expect(m.after).not.toHaveBeenCalled()
            await options.afterRefresh()
            expect(m.after).toHaveBeenCalledOnce()
            expect(m.hold).toHaveBeenCalledOnce()
            expect(m.resume).not.toHaveBeenCalled()
        },
    )
    it('declined foreign replacement discards the unclaimed Android source', async () => {
        m.invoke.mockResolvedValue('local-backup')
        m.confirm.mockResolvedValue(false)
        expect(
            await restoreBackupFromNativeSource({
                type: 'androidSpool',
                token: 'synthetic-token',
            }),
        ).toBeNull()
        expect(m.discard).toHaveBeenCalledWith('synthetic-token')
        expect(m.legacy).not.toHaveBeenCalled()
    })
    it('device-only restoration leaves automatic sync intent unchanged', async () => {
        await restoreBackupFromNativeSource({
            type: 'desktopPath',
            path: 'C:\\synthetic\\device.risunest',
        })
        const options = m.portable.mock.calls[0][2]
        m.chooseRestore.mockResolvedValue({
            library: false,
            deviceSections: ['local-storage'],
        })
        await options.choosePortableSections({
            libraryIncluded: true,
            repairRequired: false,
            deviceSections: ['local-storage'],
        })
        await options.onNativeStatus({ state: 'succeeded', result })
        await options.afterRefresh()
        expect(m.hold).not.toHaveBeenCalled()
        expect(m.resume).not.toHaveBeenCalled()
    })
    it('rejects unsupported signatures before any restore', async () => {
        m.invoke.mockRejectedValue(new Error('unsupported-format'))
        await expect(
            restoreBackupFromNativeSource({
                type: 'androidSpool',
                token: 'synthetic-token',
            }),
        ).rejects.toThrow('unsupported-format')
        expect(m.discard).toHaveBeenCalledOnce()
        expect(m.portable).not.toHaveBeenCalled()
    })
    it('opens one common picker and returns normally when cancelled', async () => {
        m.open.mockResolvedValue(null)
        expect(await restoreBackupFromSystemPicker()).toBeNull()
        expect(m.open.mock.calls[0][0].filters[0].extensions).toEqual([
            'risunest',
            'bin',
            'risudat',
        ])
        expect(m.invoke).not.toHaveBeenCalled()
    })
    it('passes selected device sections and own extension to native full export', async () => {
        expect(await exportPortableBackupFromSystemPicker()).toEqual(result)
        expect(m.export.mock.calls[0][2]).toEqual({
            library: true,
            deviceSections: ['local-storage', 'localforage'],
        })
        expect(m.save.mock.calls[0][0].filters[0].extensions).toEqual([
            'risunest',
        ])
    })
    it('a cancelled section chooser never starts or picks a destination', async () => {
        m.chooseExport.mockResolvedValue(null)
        expect(await exportPortableBackupFromSystemPicker()).toBeNull()
        expect(m.export).not.toHaveBeenCalled()
        expect(m.save).not.toHaveBeenCalled()
    })
})
