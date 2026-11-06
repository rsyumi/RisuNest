import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
    os: 'windows' as 'windows' | 'android',
    relaunch: vi.fn(),
    runtime: {synthetic: 'runtime'},
    snapshotRestore: vi.fn(),
}))

const activation = {stagingId:'stage',activationRevision:8,bindingAuthority:'0'}
const bodyReceipt = {jobId:'bodies',kind:'snapshot-bodies',stagingId:'stage',activationRevision:'8',bindingAuthority:'0'}
const bodyStatus = {jobId:'bodies',kind:'snapshot-bodies',snapshotStagingId:'stage',activationRevision:8,activationAuthority:'0',state:'succeeded',phase:'complete',progress:{completedBytes:0,completedItems:0},snapshotBodies:{stageId:'stage',activatedRevision:8,bindingAuthority:'0',policy:'full',total:0,locallyPresent:0,remoteHeld:0,unavailable:0,allBodiesLocal:true,settled:true}}

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('../desktopRelaunch', () => ({ relaunch: mocks.relaunch }))
vi.mock('./persistentDataRuntime.svelte',()=>({getPersistentDataRuntime:()=>mocks.runtime}))
vi.mock('./nativeFileJobs',async importOriginal=>({...await importOriginal<typeof import('./nativeFileJobs')>(),runNativeSnapshotRestore:mocks.snapshotRestore}))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: 'tauri', os: mocks.os })))

import {
    applyNativeDataHealthRepair,
    completeNativeDataHealthIntent,
    deepScanNativeDataHealth,
    holdNativeDataHealthScan,
    scanNativeDataHealth,
    previewNativeDataHealthRepair,
    undoNativeDataHealthRepair,
    deleteNativePersistentSnapshot,
    executeNativePersistentAssetGc,
    getNativePersistentStorageStats,
    previewNativePersistentAssetGc,
    checkpointNativePersistentStore,
    createNativePersistentSnapshot,
    createPeriodicNativeSnapshotIfDue,
    listNativePersistentSnapshots,
    requestNativePersistentSnapshotRestore,
    completeNativeSnapshotRestoreBodies,
    attachNativeSnapshotRestoreBodies,
    reattachNativeSnapshotRestoreBodies,
    retrySnapshotRestoreBodiesFromOutcome,
    restartNativeApp,
    restoreNativePersistentSnapshot,
    schedulePeriodicNativeSnapshot,
} from './nativePersistentMaintenance'
import { updateDeviceSettings } from './deviceSettings'
import { get } from 'svelte/store'
import { cancelActiveNativeFileOperation, dismissNativeFileOperationOutcome, nativeFileOperation, nativeFileOperationOutcome, runSharedNativeFileOperation } from './nativeFileJobManager'

describe('native persistent maintenance', () => {
    it('sends the viewed diagnosis identity and expected revision for repair and undo under a maintenance hold', async () => {
        const held: string[] = []
        window.RisuBackgroundTasks = {
            begin: vi.fn(async (_id: string, kind: string) => { held.push(kind); return 'active' }),
            progress: vi.fn(async () => {}),
            end: vi.fn(async () => {}),
        }
        mocks.os = 'android'
        try {
            await previewNativeDataHealthRepair(['repair'], 123)
            await applyNativeDataHealthRepair(['repair'], true, 7, 123)
            await undoNativeDataHealthRepair('journal', 8)
            await completeNativeDataHealthIntent(2, 9, 123)
        } finally {
            mocks.os = 'windows'
            delete window.RisuBackgroundTasks
        }
        expect(held).toEqual(Array(3).fill('maintenance'))
        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_data_health_repair_preview', { selection: ['repair'], expectedScannedAt: 123 }],
            ['pds_data_health_repair_apply', { selection: ['repair'], snapshot: true, expectedRevision: 7, expectedScannedAt: 123 }],
            ['pds_data_health_undo', { journalId: 'journal', expectedRevision: 8 }],
            ['pds_data_health_complete_intent', { finding: 2, expectedRevision: 9, expectedScannedAt: 123 }],
        ])
    })
    beforeEach(() => {
        mocks.invoke.mockReset().mockImplementation(async command => {
            if (command === 'native_snapshot_restore_bodies_start') return bodyReceipt
            if (command === 'native_snapshot_restore_bodies_status') return bodyStatus
        })
        mocks.os = 'windows'
        mocks.relaunch.mockReset()
        mocks.snapshotRestore.mockReset().mockResolvedValue(activation)
        nativeFileOperationOutcome.set(null)
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

        await expect(createNativePersistentSnapshot('periodic')).resolves.toEqual(created)
        await expect(listNativePersistentSnapshots()).resolves.toEqual(snapshots)
        await expect(requestNativePersistentSnapshotRestore('ab18b8a5-f45c-46ba-bbf9-74b2cae87717')).resolves.toBe(true)

        expect(mocks.snapshotRestore).toHaveBeenCalledOnce()
        expect(mocks.snapshotRestore.mock.calls[0].slice(0, 2)).toEqual([mocks.runtime, {snapshotId:'ab18b8a5-f45c-46ba-bbf9-74b2cae87717'}])
        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_snapshot_create', { reason: 'periodic' }],
            ['pds_snapshot_list'],
            ['native_snapshot_restore_bodies_start',{stagingId:'stage',activationRevision:'8',bindingAuthority:'0'}],
            ['native_snapshot_restore_bodies_status',{receipt:bodyReceipt}],
            ['native_snapshot_restore_bodies_status',{receipt:bodyReceipt}],
        ])
    })

    it('maps storage maintenance commands through their typed invoke boundary, holding only user-started work', async () => {
        mocks.invoke.mockImplementation(async command => command === 'pds_snapshot_list' ? [] : {})
        const held: string[] = []
        window.RisuBackgroundTasks = {
            begin: vi.fn(async (_id: string, kind: string) => { held.push(kind); return 'active' }),
            progress: vi.fn(async () => {}),
            end: vi.fn(async () => {}),
        }
        mocks.os = 'android'
        try {
            await getNativePersistentStorageStats()
            await deleteNativePersistentSnapshot('ab18b8a5-f45c-46ba-bbf9-74b2cae87717')
            await previewNativePersistentAssetGc()
            await createPeriodicNativeSnapshotIfDue()
            expect(held).toEqual([])
            await executeNativePersistentAssetGc()
            await createNativePersistentSnapshot('manual')
            await scanNativeDataHealth()
            expect(held).toEqual(Array(3).fill('maintenance'))
            // A deep scan's pages run under the one hold the whole scan keeps.
            await holdNativeDataHealthScan(async () => {
                expect(held).toEqual(Array(4).fill('maintenance'))
                await deepScanNativeDataHealth(true)
            })
            expect(held).toEqual(Array(4).fill('maintenance'))
        } finally {
            mocks.os = 'windows'
            delete window.RisuBackgroundTasks
        }

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_storage_stats'],
            ['pds_snapshot_delete', { id: 'ab18b8a5-f45c-46ba-bbf9-74b2cae87717' }],
            ['pds_asset_gc_preview'],
            ['pds_snapshot_list'],
            ['pds_snapshot_create', { reason: 'periodic' }],
            ['pds_asset_gc_execute'],
            ['pds_snapshot_create', { reason: 'manual' }],
            ['pds_data_health_scan'],
            ['pds_data_health_deep_scan', { resume: true }],
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

    it('creates a periodic snapshot when the newest archive timestamp is in the future', async () => {
        const now = 2_000_000_000
        const created = { path: 'clock-recovered.db', bytes: 512, durationMs: 4 }
        mocks.invoke
            .mockResolvedValueOnce([{ path: 'future.db', bytes: 1, modifiedAt: now + 7 * 24 * 60 * 60 * 1000 }])
            .mockResolvedValueOnce(created)

        await expect(createPeriodicNativeSnapshotIfDue(now)).resolves.toEqual(created)
        expect(mocks.invoke).toHaveBeenLastCalledWith('pds_snapshot_create', {
            reason: 'periodic',
        })
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
        await vi.waitFor(() => expect(mocks.invoke.mock.calls).toEqual([
            ['pds_snapshot_list'],
            ['pds_message_object_sweep'],
        ]))
    })

    it('creates no periodic snapshot while automatic snapshots are off, even with none stored, and still sweeps', async () => {
        vi.useFakeTimers()
        vi.stubGlobal('requestIdleCallback', undefined)
        mocks.invoke.mockResolvedValue([])
        updateDeviceSettings({ automaticSnapshotsEnabled: false })
        try {
            await expect(createPeriodicNativeSnapshotIfDue()).resolves.toBeNull()
            schedulePeriodicNativeSnapshot()
            await vi.advanceTimersByTimeAsync(0)
        } finally {
            updateDeviceSettings({ automaticSnapshotsEnabled: true })
        }

        expect(mocks.invoke.mock.calls).toEqual([['pds_message_object_sweep']])
    })

    it('sweeps stored message objects even when the snapshot check fails', async () => {
        vi.useFakeTimers()
        vi.stubGlobal('requestIdleCallback', undefined)
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const sweepError = new Error('sweep failed')
        mocks.invoke
            .mockRejectedValueOnce(new Error('snapshot list failed'))
            .mockRejectedValueOnce(sweepError)

        schedulePeriodicNativeSnapshot()
        await vi.advanceTimersByTimeAsync(0)

        expect(mocks.invoke.mock.calls).toEqual([['pds_snapshot_list'], ['pds_message_object_sweep']])
        expect(consoleError).toHaveBeenCalledWith('Periodic native object sweep failed', sweepError)
        consoleError.mockRestore()
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
        mocks.invoke.mockImplementation(async (command: string) =>
            command === 'pds_snapshot_list' ? [{ path: 'recent.db', bytes: 1, modifiedAt: Date.now() }] : undefined)
        const calls = (command: string) => mocks.invoke.mock.calls.filter(([name]) => name === command).length

        schedulePeriodicNativeSnapshot()
        await vi.advanceTimersByTimeAsync(0)
        expect([calls('pds_snapshot_list'), calls('pds_message_object_sweep')]).toEqual([1, 1])

        await vi.advanceTimersByTimeAsync(60 * 60 * 1000)
        expect([calls('pds_snapshot_list'), calls('pds_message_object_sweep')]).toEqual([2, 2])
    })

    it('reports an empty snapshot list without prompting', async () => {
        const actions = {
            choose: vi.fn(),
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
            onEmpty: vi.fn(),
        }
        mocks.invoke.mockResolvedValueOnce([snapshot])

        await expect(
            restoreNativePersistentSnapshot({
                ...baseActions,
                choose: vi.fn().mockResolvedValue(null),
            }),
        ).resolves.toBe(false)

        mocks.snapshotRestore.mockRejectedValueOnce(new DOMException('Native file job was cancelled', 'AbortError'))
        mocks.invoke.mockResolvedValueOnce([snapshot])
        await expect(
            restoreNativePersistentSnapshot({
                ...baseActions,
                choose: vi.fn().mockResolvedValue(snapshot.id),
            }),
        ).resolves.toBe(false)

        expect(mocks.snapshotRestore).toHaveBeenCalledOnce()
        expect(mocks.invoke.mock.calls).toEqual([['pds_snapshot_list'], ['pds_snapshot_list']])
        expect(mocks.relaunch).not.toHaveBeenCalled()
    })

    it('rejects a path outside the returned snapshot list', async () => {
        mocks.invoke.mockResolvedValueOnce([{ path: 'allowed.db', bytes: 1, modifiedAt: 2 }])

        await expect(
            restoreNativePersistentSnapshot({
                choose: vi.fn().mockResolvedValue('other.db'),
                onEmpty: vi.fn(),
            }),
        ).rejects.toThrow('Selected native snapshot is not available')

        expect(mocks.invoke).toHaveBeenCalledTimes(1)
    })

    it('copies the missing bodies only after the snapshot job has activated, under one hold across both', async () => {
        const events: string[] = []
        mocks.snapshotRestore.mockImplementation(async (_runtime, _request, options) => {
            options.onStatus({ ...bodyStatus, kind: 'restore-snapshot', state: 'running', phase: 'reading-source', progress: { completedBytes: 25, totalBytes: 100, completedItems: 0 } })
            events.push('activated')
            return activation
        })
        mocks.invoke.mockImplementation(async command => {
            events.push(command)
            if (command === 'pds_snapshot_list') return [{id:'snapshot.db',bytes:1,modifiedAt:2}]
            if (command === 'native_snapshot_restore_bodies_start') return bodyReceipt
            if (command === 'native_snapshot_restore_bodies_status') return { ...bodyStatus, progress: { completedBytes: 75, totalBytes: 100, completedItems: 0 } }
        })
        let held = 0
        const progress = new Map<string, number[]>()
        window.RisuBackgroundTasks = {
            begin: vi.fn(async id => { held++; progress.set(id, []); return 'active' }),
            progress: vi.fn(async (id, percent) => { progress.get(id)!.push(percent) }),
            end: vi.fn(async () => { if (--held === 0) events.push('released') }),
        }
        mocks.os = 'android'
        try {
            await expect(restoreNativePersistentSnapshot({choose:async()=>'snapshot.db',onEmpty:vi.fn()})).resolves.toBe(true)
        } finally {
            mocks.os = 'windows'
            delete window.RisuBackgroundTasks
        }
        const work = events.filter(event => event !== 'native_snapshot_restore_bodies_status')
        expect(work).toEqual(['pds_snapshot_list', 'activated', 'native_snapshot_restore_bodies_start', 'released'])
        // The spanning hold mirrors the measured phase, so it cannot mask either phase as unknown.
        expect([...progress.values()][0]).toEqual([25, -1, 75])
        expect(mocks.relaunch).not.toHaveBeenCalled()
    })
    it('leaves a restore failure to the operation dialog and copies the bodies when the app recovers the activation', async () => {
        let recover: ((value: typeof activation) => Promise<void>) | undefined
        mocks.snapshotRestore.mockImplementation(async (_runtime, _request, options) => {
            recover = options.afterActivationRecovery
            throw new Error('synthetic committed refresh failure')
        })
        await expect(requestNativePersistentSnapshotRestore('snapshot')).resolves.toBe(false)
        expect(get(nativeFileOperationOutcome)).toMatchObject({kind:'import', state:'failed'})
        expect(mocks.invoke).not.toHaveBeenCalled()
        await recover!(activation)
        expect(mocks.invoke.mock.calls[0]).toEqual(['native_snapshot_restore_bodies_start',{stagingId:'stage',activationRevision:'8',bindingAuthority:'0'}])
    })
    it('reports post-adoption body failure without restoring the snapshot again', async () => {
        const failed={...bodyStatus,state:'failed',error:{code:'snapshot-bodies-incomplete',message:'Snapshot library is restored, but some bodies remain unavailable'},snapshotBodies:{...bodyStatus.snapshotBodies,allBodiesLocal:false,settled:false,total:1,locallyPresent:0,unavailable:1}}
        mocks.invoke.mockImplementation(async command=>command==='native_snapshot_restore_bodies_start'?bodyReceipt:command==='native_snapshot_restore_bodies_status'?failed:undefined)
        await expect(requestNativePersistentSnapshotRestore('snapshot')).rejects.toMatchObject({name:'NativeSnapshotBodiesCommittedError',bodies:{unavailable:1}})
        expect(mocks.snapshotRestore).toHaveBeenCalledOnce()
        mocks.invoke.mockImplementation(async command=>command==='native_snapshot_restore_bodies_start'?bodyReceipt:bodyStatus)
        await expect(completeNativeSnapshotRestoreBodies('stage',8,'0')).resolves.toMatchObject({allBodiesLocal:true})
        expect(mocks.snapshotRestore).toHaveBeenCalledOnce()
    })
    it('keeps the committed snapshot identity after a lost body-start response', async () => {
        mocks.invoke.mockRejectedValueOnce(new Error('start response lost')).mockResolvedValueOnce([])
        await expect(requestNativePersistentSnapshotRestore('snapshot')).rejects.toMatchObject({
            name:'NativeSnapshotBodiesCommittedError',code:'snapshot-body-start-unknown',
            receipt:{stagingId:'stage',activationRevision:'8',bindingAuthority:'0'},
        })
        expect(get(nativeFileOperationOutcome)).toMatchObject({kind:'import', state:'succeeded'})
        mocks.invoke.mockImplementation(async command=>command==='native_snapshot_restore_bodies_start'?bodyReceipt:bodyStatus)
        await expect(completeNativeSnapshotRestoreBodies('stage',8,'0')).resolves.toMatchObject({allBodiesLocal:true})
        expect(mocks.snapshotRestore).toHaveBeenCalledOnce()
    })
    it('reconciles a lost start response by the existing receipt even if the worker already finished', async () => {
        mocks.invoke.mockImplementation(async command => {
            if (command === 'native_snapshot_restore_bodies_start') throw new Error('lost response')
            if (command === 'native_file_job_list') return [bodyStatus]
            if (command === 'native_snapshot_restore_bodies_status') return bodyStatus
        })
        await expect(completeNativeSnapshotRestoreBodies('stage', 8, '0')).resolves.toMatchObject({settled:true})
        expect(mocks.snapshotRestore).not.toHaveBeenCalled()
        expect(mocks.invoke.mock.calls.filter(call => call[0] === 'native_snapshot_restore_bodies_start')).toHaveLength(1)
    })
    it('reattaches exactly one progress owner after reload and forwards cancellation without another restore', async () => {
        const running = {...bodyStatus, jobId:'reloaded', state:'running', phase:'copying-missing-bodies'}
        let cancelled = false
        mocks.invoke.mockImplementation(async command => {
            if (command === 'native_file_job_cancel') { cancelled = true; return 'requested' }
            if (command === 'native_snapshot_restore_bodies_status') return {...running, state:cancelled ? 'cancelled' : 'running'}
            throw new Error(`Unexpected ${command}`)
        })
        const first = reattachNativeSnapshotRestoreBodies([running as never])
        await reattachNativeSnapshotRestoreBodies([running as never])
        await vi.waitFor(() => expect(get(nativeFileOperation)?.status?.jobId).toBe('reloaded'))
        cancelActiveNativeFileOperation()
        await first
        expect(get(nativeFileOperationOutcome)).toMatchObject({state:'cancelled',status:{jobId:'reloaded'}})
        expect(mocks.invoke.mock.calls.filter(call => call[0] === 'native_file_job_cancel')).toHaveLength(1)
        expect(mocks.snapshotRestore).not.toHaveBeenCalled()
        expect(mocks.invoke.mock.calls.some(call => call[0] === 'native_snapshot_restore_bodies_start')).toBe(false)
    })
    it.each(['failed', 'cancelled'] as const)('shows a recovered %s outcome and starts only an explicit body retry', async state => {
        const terminal = {...bodyStatus, jobId:`recovered-${state}`, state}
        mocks.invoke.mockResolvedValue(terminal)
        await reattachNativeSnapshotRestoreBodies([terminal as never])
        expect(get(nativeFileOperationOutcome)).toMatchObject({state,status:{jobId:terminal.jobId}})
        expect(mocks.invoke.mock.calls.every(call => call[0] === 'native_snapshot_restore_bodies_status')).toBe(true)
        mocks.invoke.mockImplementation(async command => command === 'native_snapshot_restore_bodies_start' ? bodyReceipt : bodyStatus)
        await retrySnapshotRestoreBodiesFromOutcome()
        expect(mocks.invoke).toHaveBeenCalledWith('native_snapshot_restore_bodies_start', {...activation,activationRevision:'8',previousJobId:terminal.jobId})
        expect(mocks.snapshotRestore).not.toHaveBeenCalled()
    })
    it('waits for an existing dialog owner before attaching the recovered job', async () => {
        let release!: () => void
        const busy = runSharedNativeFileOperation('export', 'busy-before-recovery', () => new Promise<void>(resolve => { release = resolve }))
        const terminal = {...bodyStatus,jobId:'after-busy'}
        mocks.invoke.mockResolvedValue(terminal)
        const recovered = reattachNativeSnapshotRestoreBodies([terminal as never])
        await Promise.resolve()
        expect(mocks.invoke).not.toHaveBeenCalled()
        release()
        await Promise.all([busy, recovered])
        expect(get(nativeFileOperationOutcome)).toMatchObject({state:'succeeded',status:{jobId:'after-busy'}})
    })
    it('keeps mixed-stage outcomes and their retries until each exact receipt is dismissed', async () => {
        const first = {...bodyStatus, jobId:'mixed-failed', snapshotStagingId:'mixed-first', state:'failed'}
        const retried = {...bodyStatus, jobId:'mixed-retried', snapshotStagingId:'mixed-first', snapshotBodies:{...bodyStatus.snapshotBodies,stageId:'mixed-first'}}
        const second = {...bodyStatus, jobId:'mixed-cancelled', snapshotStagingId:'mixed-second', state:'cancelled'}
        const third = {...bodyStatus, jobId:'mixed-succeeded', snapshotStagingId:'mixed-third', snapshotBodies:{...bodyStatus.snapshotBodies,stageId:'mixed-third'}}
        const statuses = new Map([first, retried, second, third].map(status => [status.jobId, status]))
        const acknowledged: string[] = []
        let rejectAcknowledgement = false
        mocks.invoke.mockImplementation(async (command, args) => {
            if (command === 'native_snapshot_restore_bodies_status') return statuses.get(args.receipt.jobId)
            if (command === 'native_snapshot_restore_bodies_start') return {...bodyReceipt,jobId:retried.jobId,stagingId:retried.snapshotStagingId}
            if (command === 'native_file_job_forget') {
                if (rejectAcknowledgement) throw new Error('synthetic acknowledgement failure')
                acknowledged.push(args.jobId)
                return true
            }
            throw new Error(`Unexpected ${command}`)
        })
        const recovered = reattachNativeSnapshotRestoreBodies([first, second, third] as never)
        await vi.waitFor(() => expect(get(nativeFileOperationOutcome)?.status?.jobId).toBe(first.jobId))
        expect(mocks.invoke.mock.calls.some(call => call[1]?.receipt?.jobId === second.jobId)).toBe(false)
        await retrySnapshotRestoreBodiesFromOutcome()
        expect(get(nativeFileOperationOutcome)?.status?.jobId).toBe(retried.jobId)
        expect(mocks.invoke.mock.calls.some(call => call[1]?.receipt?.jobId === second.jobId)).toBe(false)
        await dismissNativeFileOperationOutcome()
        await vi.waitFor(() => expect(get(nativeFileOperationOutcome)?.status?.jobId).toBe(second.jobId))
        expect(acknowledged).toEqual([retried.jobId])
        rejectAcknowledgement = true
        await expect(dismissNativeFileOperationOutcome()).rejects.toThrow('acknowledgement failure')
        expect(get(nativeFileOperationOutcome)?.status?.jobId).toBe(second.jobId)
        expect(mocks.invoke.mock.calls.some(call => call[1]?.receipt?.jobId === third.jobId)).toBe(false)
        rejectAcknowledgement = false
        await dismissNativeFileOperationOutcome()
        await recovered
        expect(get(nativeFileOperationOutcome)?.status?.jobId).toBe(third.jobId)
        await dismissNativeFileOperationOutcome()
        expect(acknowledged).toEqual([retried.jobId, second.jobId, third.jobId])
        expect(get(nativeFileOperationOutcome)).toBeNull()
        expect(mocks.snapshotRestore).not.toHaveBeenCalled()
    })
    it('shows an already finished job without starting more body work', async () => {
        mocks.invoke.mockResolvedValue(bodyStatus)
        await reattachNativeSnapshotRestoreBodies([bodyStatus as never])
        expect(get(nativeFileOperationOutcome)).toMatchObject({state:'succeeded'})
        expect(mocks.invoke.mock.calls.every(call => call[0] === 'native_snapshot_restore_bodies_status')).toBe(true)
    })
    it('refuses stale binding and substituted job receipts before publishing progress', async () => {
        mocks.invoke.mockRejectedValueOnce({code:'invalid-activation-receipt',message:'stale binding'})
        await expect(attachNativeSnapshotRestoreBodies(bodyReceipt as never)).rejects.toMatchObject({code:'invalid-activation-receipt'})
        expect(get(nativeFileOperation)).toBeNull()
        mocks.invoke.mockResolvedValue({...bodyStatus,jobId:'other-job'})
        await expect(attachNativeSnapshotRestoreBodies(bodyReceipt as never)).rejects.toMatchObject({code:'snapshot-body-receipt-mismatch'})
        expect(get(nativeFileOperation)).toBeNull()
        expect(mocks.snapshotRestore).not.toHaveBeenCalled()
    })
    it('refuses a start response for a different activation before attaching its job', async () => {
        mocks.invoke.mockResolvedValue({...bodyReceipt, stagingId:'other-stage'})
        await expect(completeNativeSnapshotRestoreBodies('stage', 8, '0')).rejects.toMatchObject({code:'snapshot-body-receipt-mismatch'})
        expect(mocks.invoke).toHaveBeenCalledOnce()
        expect(get(nativeFileOperation)).toBeNull()
    })
    it('reports a restore the shared operation refused to start', async () => {
        let release!: () => void
        const busy = runSharedNativeFileOperation('export', 'synthetic-busy', () => new Promise<void>(resolve => { release = resolve }))
        await expect(requestNativePersistentSnapshotRestore('snapshot')).rejects.toMatchObject({name:'NativeFileOperationBusyError'})
        release()
        await busy
        expect(mocks.snapshotRestore).not.toHaveBeenCalled()
        expect(mocks.relaunch).not.toHaveBeenCalled()
    })

    it('requests an Android cold restart through the lifecycle bridge', async () => {
        mocks.os = 'android'
        const requestRestart = vi.fn()
        ;(window as Window & {
            RisuLifecycleBridge?: { requestRestart(): void }
        }).RisuLifecycleBridge = { requestRestart }

        await restartNativeApp()

        expect(requestRestart).toHaveBeenCalledOnce()
        expect(mocks.relaunch).not.toHaveBeenCalled()
    })

    it('does not fall back to the unsupported process relauncher on Android', async () => {
        mocks.os = 'android'
        await expect(restartNativeApp()).rejects.toThrow(
            'Android restart bridge is unavailable',
        )

        expect(mocks.relaunch).not.toHaveBeenCalled()
    })

    it('uses the process relauncher outside Tauri mobile', async () => {
        mocks.relaunch.mockResolvedValue(undefined)

        await restartNativeApp()

        expect(mocks.relaunch).toHaveBeenCalledOnce()
    })
})
