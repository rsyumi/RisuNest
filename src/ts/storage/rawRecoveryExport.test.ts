import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    save: vi.fn(),
    native: vi.fn(),
    shared: vi.fn(),
    confirm: vi.fn(),
    controller: new AbortController(),
}))

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: mocks.save }))
vi.mock('../alert', () => ({ alertConfirm: mocks.confirm }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))
vi.mock('./nativeFileJobManager', () => ({
    runSharedNativeFileOperation: mocks.shared,
}))
vi.mock('./nativeFileJobs', () => ({
    NativeFileJobError: class NativeFileJobError extends Error {
        constructor(readonly code: string, message: string) {
            super(message)
        }
    },
    runNativeRawRecoveryExport: mocks.native,
}))

import { exportOriginalData } from './rawRecoveryExport'

describe('raw recovery export route', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.controller = new AbortController()
        mocks.confirm.mockResolvedValue(true)
        mocks.shared.mockImplementation(
            async (_kind, _key, operation, options) => {
                expect(options).toEqual({
                    format: 'raw-recovery',
                    presentation: 'dialog',
                })
                return operation({
                    signal: mocks.controller.signal,
                    onStatus: vi.fn(),
                })
            },
        )
    })

    it('selects a desktop destination and starts the runtime-independent native operation', async () => {
        mocks.save.mockResolvedValue('C:\\synthetic\\original.risunest-rescue.zip')
        mocks.native.mockResolvedValue({ warningCodes: [] })

        await expect(exportOriginalData()).resolves.toEqual({ warningCodes: [] })

        expect(mocks.save).toHaveBeenCalledWith(
            expect.objectContaining({
                defaultPath: expect.stringMatching(/^risunest-\d{4}(-\d{2}){5}\.risunest-rescue\.zip$/),
                filters: [{
                    name: 'RisuNest Rescue Archive',
                    extensions: ['risunest-rescue.zip'],
                }],
            }),
        )
        expect(mocks.native).toHaveBeenCalledWith(
            {
                type: 'desktopPath',
                path: 'C:\\synthetic\\original.risunest-rescue.zip',
            },
            expect.objectContaining({ signal: expect.any(AbortSignal) }),
        )
        expect(mocks.shared).toHaveBeenCalledWith(
            'export',
            'raw-recovery-export',
            expect.any(Function),
            expect.any(Object),
        )
    })

    it('does not start native capture when the destination picker is cancelled', async () => {
        mocks.save.mockResolvedValue(null)

        await expect(exportOriginalData()).resolves.toBeNull()
        expect(mocks.native).not.toHaveBeenCalled()
        expect(mocks.confirm).not.toHaveBeenCalled()
    })

    it('warns about account information and refuses capture without confirmation', async () => {
        mocks.save.mockResolvedValue('C:\\synthetic\\original.risunest-rescue.zip')
        mocks.confirm.mockResolvedValue(false)
        await expect(exportOriginalData()).resolves.toBeNull()
        const { language } = await import('src/lang')
        expect(mocks.confirm).toHaveBeenCalledExactlyOnceWith(language.risuNest.backup.originalDataAccountConfirm)
        expect(mocks.native).not.toHaveBeenCalled()
    })

    it('does not capture when cancelled while account confirmation is open', async () => {
        mocks.save.mockResolvedValue('C:\\synthetic\\original.risunest-rescue.zip')
        mocks.confirm.mockImplementation(async () => { mocks.controller.abort(); return true })
        await expect(exportOriginalData()).rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.native).not.toHaveBeenCalled()
    })
})
