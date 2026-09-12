import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
    isTauriMobile: true,
    relaunch: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('@tauri-apps/plugin-process', () => ({ relaunch: mocks.relaunch }))
vi.mock('../platform', () => ({
    get isTauriMobile() {
        return mocks.isTauriMobile
    },
}))

import {
    deleteNativePersistentSnapshot,
    executeNativePersistentAssetGc,
    getNativePersistentStorageStats,
    previewNativePersistentAssetGc,
    checkpointNativePersistentStore,
    createNativePersistentSnapshot,
    createPeriodicNativeSnapshotIfDue,
    listNativePersistentSnapshots,
    requestNativePersistentSnapshotRestore,
    restartNativeApp,
    restoreNativePersistentSnapshot,
    schedulePeriodicNativeSnapshot,
} from './nativePersistentMaintenance'

describe('native persistent maintenance', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.isTauriMobile = true
        mocks.relaunch.mockReset()
        vi.useRealTimers()
        vi.unstubAllGlobals()
        delete (window as Window & {
            RisuLifecycleBridge?: unknown
        }).RisuLifecycleBridge
    })

    it('maps checkpoints to the native persistent store command', async () => {
        mocks.invoke.mockResolvedValue(undefined)

        await checkpointNativePersistentStore('truncate')

        expect(mocks.invoke).toHaveBeenCalledWith('pds_checkpoint', { mode: 'truncate' })
    })

    it('maps snapshot operations to native persistent store commands', async () => {
        const created = { id: 'ab18b8a5-f45c-46ba-bbf9-74b2cae87717', bytes: 1024, durationMs: 8 }
        const snapshots = [{ id: 'ab18b8a5-f45c-46ba-bbf9-74b2cae87717', bytes: 1024, modifiedAt: 123 }]
        mocks.invoke
            .mockResolvedValueOnce(created)
            .mockResolvedValueOnce(snapshots)
            .mockResolvedValueOnce(undefined)

        await expect(createNativePersistentSnapshot('periodic')).resolves.toEqual(created)
        await expect(listNativePersistentSnapshots()).resolves.toEqual(snapshots)
        await requestNativePersistentSnapshotRestore('ab18b8a5-f45c-46ba-bbf9-74b2cae87717')

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_snapshot_create', { reason: 'periodic' }],
            ['pds_snapshot_list'],
            ['pds_snapshot_restore_request', { id: 'ab18b8a5-f45c-46ba-bbf9-74b2cae87717' }],
        ])
    })

    it('maps storage maintenance commands through their typed invoke boundary', async () => {
        mocks.invoke.mockResolvedValue({})

        await getNativePersistentStorageStats()
        await deleteNativePersistentSnapshot('ab18b8a5-f45c-46ba-bbf9-74b2cae87717')
        await previewNativePersistentAssetGc()
        await executeNativePersistentAssetGc()

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_storage_stats'],
            ['pds_snapshot_delete', { id: 'ab18b8a5-f45c-46ba-bbf9-74b2cae87717' }],
            ['pds_asset_gc_preview'],
            ['pds_asset_gc_execute'],
        ])
    })


    it.each([
        { age: 24 * 60 * 60 * 1000 - 1, due: false },
        { age: 24 * 60 * 60 * 1000, due: true },
        { age: 24 * 60 * 60 * 1000 + 1, due: true },
    ])(
        'creates a periodic snapshot only at or beyond the 24-hour boundary',
        async ({ age, due }) => {
            const now = 2_000_000_000
            const created = { path: 'periodic.db', bytes: 2048, durationMs: 10 }
            mocks.invoke
                .mockResolvedValueOnce([
                    { path: 'previous.db', bytes: 1024, modifiedAt: now - age },
                ])
                .mockResolvedValueOnce(created)

            const result = await createPeriodicNativeSnapshotIfDue(now)

            expect(result).toEqual(due ? created : null)
            expect(mocks.invoke.mock.calls).toEqual(
                due
                    ? [['pds_snapshot_list'], ['pds_snapshot_create', { reason: 'periodic' }]]
                    : [['pds_snapshot_list']],
            )
        },
    )

    it('creates the first periodic snapshot when none exist', async () => {
        const created = { path: 'first.db', bytes: 512, durationMs: 4 }
        mocks.invoke.mockResolvedValueOnce([]).mockResolvedValueOnce(created)

        await expect(createPeriodicNativeSnapshotIfDue()).resolves.toEqual(created)
    })

    it('registers one idle callback plus the hourly re-check and runs maintenance', async () => {
        let idleCallback: (() => void) | undefined
        const requestIdleCallback = vi.fn((callback: () => void) => {
            idleCallback = callback
            return 1
        })
        vi.stubGlobal('requestIdleCallback', requestIdleCallback)
        const setIntervalSpy = vi.fn()
        vi.stubGlobal('setInterval', setIntervalSpy)
        mocks.invoke.mockResolvedValueOnce([
            { path: 'recent.db', bytes: 1, modifiedAt: Date.now() },
        ])

        schedulePeriodicNativeSnapshot()

        expect(requestIdleCallback).toHaveBeenCalledTimes(1)
        expect(setIntervalSpy).toHaveBeenCalledWith(expect.any(Function), 60 * 60 * 1000)
        expect(mocks.invoke).not.toHaveBeenCalled()
        idleCallback?.()
        await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith('pds_snapshot_list'))
    })

    it('falls back to a timer and logs maintenance failures', async () => {
        vi.useFakeTimers()
        vi.stubGlobal('requestIdleCallback', undefined)
        const error = new Error('snapshot list failed')
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        mocks.invoke.mockRejectedValueOnce(error)

        schedulePeriodicNativeSnapshot()
        await vi.advanceTimersByTimeAsync(0)

        expect(consoleError).toHaveBeenCalledWith('Periodic native snapshot failed', error)
        consoleError.mockRestore()
    })

    it('re-checks hourly so long sessions still create periodic snapshots', async () => {
        vi.useFakeTimers()
        vi.stubGlobal('requestIdleCallback', undefined)
        mocks.invoke.mockResolvedValue([{ path: 'recent.db', bytes: 1, modifiedAt: Date.now() }])

        schedulePeriodicNativeSnapshot()
        await vi.advanceTimersByTimeAsync(0)
        expect(mocks.invoke).toHaveBeenCalledTimes(1)

        await vi.advanceTimersByTimeAsync(60 * 60 * 1000)
        expect(mocks.invoke).toHaveBeenCalledTimes(2)
    })

    it('reports an empty snapshot list without prompting', async () => {
        const actions = {
            choose: vi.fn(),
            confirm: vi.fn(),
            restart: vi.fn(),
            onEmpty: vi.fn(),
        }
        mocks.invoke.mockResolvedValueOnce([])

        await expect(restoreNativePersistentSnapshot(actions)).resolves.toBe(false)

        expect(actions.onEmpty).toHaveBeenCalledOnce()
        expect(actions.choose).not.toHaveBeenCalled()
    })

    it('stops when snapshot choice or confirmation is cancelled', async () => {
        const snapshot = { id: 'snapshot.db', bytes: 1, modifiedAt: 2 }
        const baseActions = {
            restart: vi.fn(),
            onEmpty: vi.fn(),
        }
        mocks.invoke.mockResolvedValueOnce([snapshot])

        await expect(
            restoreNativePersistentSnapshot({
                ...baseActions,
                choose: vi.fn().mockResolvedValue(null),
                confirm: vi.fn(),
            }),
        ).resolves.toBe(false)

        mocks.invoke.mockResolvedValueOnce([snapshot])
        await expect(
            restoreNativePersistentSnapshot({
                ...baseActions,
                choose: vi.fn().mockResolvedValue(snapshot.id),
                confirm: vi.fn().mockResolvedValue(false),
            }),
        ).resolves.toBe(false)

        expect(mocks.invoke.mock.calls).toEqual([['pds_snapshot_list'], ['pds_snapshot_list']])
        expect(baseActions.restart).not.toHaveBeenCalled()
    })

    it('rejects a path outside the returned snapshot list', async () => {
        mocks.invoke.mockResolvedValueOnce([{ path: 'allowed.db', bytes: 1, modifiedAt: 2 }])

        await expect(
            restoreNativePersistentSnapshot({
                choose: vi.fn().mockResolvedValue('other.db'),
                confirm: vi.fn(),
                restart: vi.fn(),
                onEmpty: vi.fn(),
            }),
        ).rejects.toThrow('Selected native snapshot is not available')

        expect(mocks.invoke).toHaveBeenCalledTimes(1)
    })

    it('confirms, writes the restore marker, and only then restarts', async () => {
        const order: string[] = []
        const snapshot = { id: 'snapshot.db', bytes: 1, modifiedAt: 2 }
        mocks.invoke.mockImplementation(async (command: string) => {
            order.push(command)
            return command === 'pds_snapshot_list' ? [snapshot] : undefined
        })

        await expect(
            restoreNativePersistentSnapshot({
                choose: async () => {
                    order.push('choose')
                    return snapshot.id
                },
                confirm: async () => {
                    order.push('confirm')
                    return true
                },
                restart: async () => {
                    order.push('restart')
                },
                onEmpty: vi.fn(),
            }),
        ).resolves.toBe(true)

        expect(order).toEqual([
            'pds_snapshot_list',
            'choose',
            'confirm',
            'pds_snapshot_restore_request',
            'restart',
        ])
    })

    it('does not restart when writing the restore marker fails', async () => {
        const requestError = new Error('marker failed')
        const restart = vi.fn()
        mocks.invoke
            .mockResolvedValueOnce([{ id: 'snapshot.db', bytes: 1, modifiedAt: 2 }])
            .mockRejectedValueOnce(requestError)

        await expect(
            restoreNativePersistentSnapshot({
                choose: vi.fn().mockResolvedValue('snapshot.db'),
                confirm: vi.fn().mockResolvedValue(true),
                restart,
                onEmpty: vi.fn(),
            }),
        ).rejects.toBe(requestError)

        expect(restart).not.toHaveBeenCalled()
    })

    it('requests an Android cold restart through the lifecycle bridge', async () => {
        const requestRestart = vi.fn()
        ;(window as Window & {
            RisuLifecycleBridge?: { requestRestart(): void }
        }).RisuLifecycleBridge = { requestRestart }

        await restartNativeApp()

        expect(requestRestart).toHaveBeenCalledOnce()
        expect(mocks.relaunch).not.toHaveBeenCalled()
    })

    it('does not fall back to the unsupported process relauncher on Android', async () => {
        await expect(restartNativeApp()).rejects.toThrow(
            'Android restart bridge is unavailable',
        )

        expect(mocks.relaunch).not.toHaveBeenCalled()
    })

    it('uses the process relauncher outside Tauri mobile', async () => {
        mocks.isTauriMobile = false
        mocks.relaunch.mockResolvedValue(undefined)

        await restartNativeApp()

        expect(mocks.relaunch).toHaveBeenCalledOnce()
    })
})
