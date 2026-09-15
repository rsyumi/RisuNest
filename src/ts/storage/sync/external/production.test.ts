import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ExternalJobSummary, ExternalStorageState } from './types'

const mocks = vi.hoisted(() => ({
    revision: 8,
    listener: undefined as ((revision: number) => void) | undefined,
    bridge: {
        supported: true,
        getState: vi.fn(),
        setExecutionSession: vi.fn(async (_request: {
            kind: 'foreground' | 'hidden' | 'exitDrain'
            id: string
        }) => {}),
        startJob: vi.fn(),
        getJob: vi.fn(),
        cancelJob: vi.fn(),
        prepareDeviceCapture: vi.fn(),
        requestDeviceMaintenanceRestart: vi.fn(),
        applyReceived: vi.fn(),
    },
    flush: vi.fn(async (_reason: string) => {}),
    refreshWorkingSet: vi.fn(async () => {}),
    releaseFence: vi.fn(),
    reloadPlugins: vi.fn(async () => {}),
}))

vi.mock('./bridge', () => ({
    getExternalStorageBridge: () => mocks.bridge,
}))
vi.mock('../../persistentDataRuntime.svelte', () => ({
    flushPendingDataLocally: mocks.flush,
    capturePersistentMutationToken: vi.fn(async () => ({
        revision: mocks.revision,
        mutationGeneration: 1,
    })),
    acquireDestructiveReplacementFence: vi.fn(async () => ({
        revision: mocks.revision,
        refreshCommittedWorkingSet: mocks.refreshWorkingSet,
        release: mocks.releaseFence,
    })),
}))
vi.mock('../../../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: mocks.reloadPlugins,
}))
vi.mock('../../persistentRevisionEvents', () => ({
    subscribeLocalPersistentRevision: (listener: (revision: number) => void) => {
        mocks.listener = listener
        return () => { mocks.listener = undefined }
    },
}))

const initialState: ExternalStorageState = {
    supported: true,
    selection: {
        kind: 'external', connectionId: 'old-sync', selectionEpoch: 'old-epoch',
        paused: false, decisionRequired: false,
    },
    connections: [{
        id: 'old-sync', providerId: 'webdav', purpose: 'sync', strategy: 'sequential',
        mode: 'existing', displayName: 'Old', endpoint: {
            providerId: 'webdav', authority: 'synthetic.invalid', repositoryHint: 'old',
            warnings: [], remoteVerified: true,
        },
        scope: { library: true, referencedAssets: true, deviceSettings: false, devicePlugins: false },
        capabilities: {
            cas: false, sequential: true, backupOnly: true, resumableUpload: false,
            rangeDownload: false, snapshotDiscovery: true, evidence: 'synthetic',
        },
        status: 'ready',
    }],
    jobs: [],
}

function succeeded(connectionId: string, revision: string): ExternalJobSummary {
    return {
        id: `job-${connectionId}`,
        connectionId,
        kind: 'sync',
        state: 'succeeded',
        phase: 'complete',
        completedBytes: '0',
        completedItems: '0',
        startedAtMs: '1',
        updatedAtMs: '2',
        result: { publishedRevision: revision as `${number}` },
    }
}

describe('external storage production integration', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.revision = 8
        mocks.bridge.getState.mockResolvedValue(initialState)
        mocks.bridge.startJob.mockImplementation(async request =>
            succeeded(request.connectionId, request.targetRevision ?? '0'))
    })
    afterEach(async () => {
        const { installExternalStorageProduction } = await import('./production')
        const dispose = await installExternalStorageProduction()
        dispose()
    })

    it('uses a fresh native exit capture instead of cached selection and drains paused sync', async () => {
        const {
            getExternalStorageSyncExitDrainAdapter,
            installExternalStorageProduction,
        } = await import('./production')
        await installExternalStorageProduction()
        const adapter = getExternalStorageSyncExitDrainAdapter({
            selection: {
                kind: 'external', id: 'new-sync', selectionEpoch: 'new-epoch',
                paused: true, decisionRequired: false,
            },
        })
        expect(adapter?.id).toBe('external:new-sync:new-epoch')
        const abort = new AbortController()
        await expect(adapter!.drain({
            revision: 12,
            libraryEpoch: 'library-epoch',
            selectionEpoch: 'new-epoch',
            selectionId: 'external:new-sync:new-epoch',
        }, abort.signal)).resolves.toEqual({ kind: 'complete' })
        expect(mocks.bridge.setExecutionSession).toHaveBeenLastCalledWith(expect.objectContaining({
            kind: 'exitDrain',
        }))
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            connectionId: 'new-sync',
            targetRevision: '12',
            reason: 'exitDrain',
            session: 'exitDrain',
        }))
        const stateReads = mocks.bridge.getState.mock.calls.length
        await adapter!.cancel('exit-unsynced')
        expect(mocks.bridge.getState).toHaveBeenCalledTimes(stateReads)
        expect(mocks.bridge.setExecutionSession).toHaveBeenLastCalledWith(expect.objectContaining({
            kind: 'exitDrain',
        }))
        const returning = getExternalStorageSyncExitDrainAdapter({
            selection: {
                kind: 'external', id: 'new-sync', selectionEpoch: 'new-epoch',
                paused: true, decisionRequired: false,
            },
        })!
        await returning.drain({
            revision: 12,
            libraryEpoch: 'library-epoch',
            selectionEpoch: 'new-epoch',
            selectionId: 'external:new-sync:new-epoch',
        }, abort.signal)
        await returning.cancel('cancel-exit')
        expect(mocks.bridge.setExecutionSession).toHaveBeenLastCalledWith(expect.objectContaining({
            kind: 'foreground',
        }))
    })

    it('flushes locally before fixing the manual goal revision', async () => {
        const { installExternalStorageProduction, requestExternalStorageNow } = await import('./production')
        await installExternalStorageProduction()
        mocks.revision = 17
        await expect(requestExternalStorageNow('old-sync', 'sync')).resolves.toMatchObject({
            kind: 'complete', revision: '17',
        })
        expect(mocks.flush).toHaveBeenCalledWith('external-sync-now')
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            targetRevision: '17', reason: 'manual',
        }))
    })

    it('serializes hidden invalidation before a new foreground inspection', async () => {
        let visibility: DocumentVisibilityState = 'visible'
        const visibilitySpy = vi.spyOn(document, 'visibilityState', 'get')
            .mockImplementation(() => visibility)
        const { installExternalStorageProduction } = await import('./production')
        await installExternalStorageProduction()
        visibility = 'hidden'
        document.dispatchEvent(new Event('visibilitychange'))
        visibility = 'visible'
        document.dispatchEvent(new Event('visibilitychange'))
        await vi.waitFor(() => expect(mocks.bridge.getState).toHaveBeenCalledTimes(2))
        const sessions = mocks.bridge.setExecutionSession.mock.calls.map(call => call[0].kind)
        expect(sessions).toEqual(['foreground', 'hidden', 'foreground'])
        visibilitySpy.mockRestore()
    })

    it('refreshes cached routing immediately after a settings mutation', async () => {
        const {
            getExternalStorageSyncExitDrainAdapter,
            installExternalStorageProduction,
            refreshExternalStorageProductionState,
        } = await import('./production')
        await installExternalStorageProduction()
        mocks.bridge.getState.mockResolvedValue({
            ...initialState,
            selection: {
                kind: 'external', connectionId: 'new-sync', selectionEpoch: 'new-epoch',
                paused: false, decisionRequired: false,
            },
            connections: [{ ...initialState.connections[0], id: 'new-sync' }],
        })
        await refreshExternalStorageProductionState()
        expect(getExternalStorageSyncExitDrainAdapter()?.id)
            .toBe('external:new-sync:new-epoch')
    })

    it('holds the replacement fence through authoritative restore refresh and plugin reload', async () => {
        const {
            installExternalStorageProduction,
            requestExternalStorageRestore,
        } = await import('./production')
        await installExternalStorageProduction()
        mocks.revision = 23
        mocks.bridge.startJob.mockResolvedValue({
            ...succeeded('old-sync', '23'),
            kind: 'restore',
            result: { snapshotId: 'snapshot-1', receivedRevision: '24' },
        })
        await expect(requestExternalStorageRestore(
            'old-sync',
            'snapshot-1',
            ['library', 'referencedAssets'],
        )).resolves.toMatchObject({ state: 'succeeded' })
        expect(mocks.flush).toHaveBeenCalledWith('external-storage-restore')
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            targetRevision: '23', snapshotId: 'snapshot-1', kind: 'restore',
        }))
        expect(mocks.refreshWorkingSet).toHaveBeenCalledWith(24)
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
    })

    it('activates a staged sync receive under the replacement fence before continuing', async () => {
        const { installExternalStorageProduction, requestExternalStorageNow } = await import('./production')
        mocks.revision = 8
        let starts = 0
        mocks.bridge.startJob.mockImplementation(async () => {
            starts += 1
            if (starts === 1) {
                return {
                    ...succeeded('old-sync', '8'),
                    state: 'waiting',
                    phase: 'remote-apply',
                    result: { receiveReady: true, snapshotId: 'remote', expectedRevision: '8' },
                }
            }
            return succeeded('old-sync', '9')
        })
        mocks.bridge.applyReceived.mockResolvedValue({
            snapshotId: 'remote', receivedRevision: '9',
        })
        mocks.bridge.getJob.mockResolvedValue({
            ...succeeded('old-sync', '8'),
            result: { snapshotId: 'remote', receivedRevision: '9' },
        })
        await installExternalStorageProduction()
        await expect(requestExternalStorageNow('old-sync', 'sync')).resolves.toMatchObject({
            kind: 'complete', revision: '9',
        })
        expect(mocks.flush).toHaveBeenCalledWith('external-storage-sync-receive')
        expect(mocks.bridge.applyReceived).toHaveBeenCalledWith('job-old-sync', '8')
        expect(mocks.refreshWorkingSet).toHaveBeenCalledWith(9)
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
        expect(mocks.bridge.startJob).toHaveBeenCalledTimes(2)
    })

    it('cancels a staged receive when a local commit advanced before activation', async () => {
        const { installExternalStorageProduction, requestExternalStorageNow } = await import('./production')
        mocks.revision = 7
        mocks.flush.mockImplementation(async reason => {
            if (reason === 'external-storage-sync-receive') mocks.revision = 8
        })
        mocks.bridge.startJob.mockResolvedValue({
            ...succeeded('old-sync', '7'),
            state: 'waiting',
            phase: 'remote-apply',
            result: { receiveReady: true, snapshotId: 'remote', expectedRevision: '7' },
        })
        mocks.bridge.cancelJob.mockResolvedValue({
            ...succeeded('old-sync', '7'), state: 'cancelled', phase: 'cancelled',
        })
        await installExternalStorageProduction()
        await expect(requestExternalStorageNow('old-sync', 'sync')).resolves.toMatchObject({
            kind: 'blocked',
        })
        expect(mocks.bridge.cancelJob).toHaveBeenCalledWith('job-old-sync')
        expect(mocks.bridge.applyReceived).not.toHaveBeenCalled()
        expect(mocks.refreshWorkingSet).not.toHaveBeenCalled()
    })

    it('reattaches a pending device capture before rebinding its backup job', async () => {
        const deviceConnection = {
            ...initialState.connections[0],
            id: 'device-backup',
            purpose: 'backup' as const,
            strategy: 'backup-only' as const,
            scope: {
                ...initialState.connections[0].scope,
                deviceSettings: true,
            },
        }
        const waitingJob: ExternalJobSummary = {
            ...succeeded('device-backup', '9'),
            id: 'device-job',
            kind: 'backup',
            state: 'waiting',
            phase: 'device-capture',
            result: undefined,
        }
        mocks.bridge.getState.mockResolvedValue({
            ...initialState,
            selection: { ...initialState.selection, kind: 'none', connectionId: undefined },
            connections: [deviceConnection],
            jobs: [waitingJob],
        })
        mocks.bridge.prepareDeviceCapture.mockResolvedValue({
            state: 'ready', captureId: 'a'.repeat(64),
        })
        const { installExternalStorageProduction } = await import('./production')
        await installExternalStorageProduction()
        expect(mocks.bridge.prepareDeviceCapture).toHaveBeenCalledWith([{
            jobId: 'device-job',
            purpose: 'backup',
            scope: deviceConnection.scope,
        }])
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            connectionId: 'device-backup',
            kind: 'backup',
            reason: 'manual',
            session: 'foreground',
        }))
    })
})
