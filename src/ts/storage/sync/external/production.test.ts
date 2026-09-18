import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ExternalJobSummary, ExternalStorageState } from './types'
import type { CommittedApplyOutcome } from '../../persistentDataRuntime'

const mocks = vi.hoisted(() => ({
    persistentRuntime: {},
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
        applyReceived: vi.fn(),
        openConflictSource: vi.fn(),
        releaseConflictSource: vi.fn(),
    },
    restoreConflictSource: vi.fn(),
    exportConflictSource: vi.fn(),
    flush: vi.fn(async (_reason: string) => {}),
    refreshWorkingSet: vi.fn(async (revision: number): Promise<CommittedApplyOutcome> => ({
        kind: 'committed', revision, projection: 'applied',
    })),
    releaseFence: vi.fn(),
    reloadPlugins: vi.fn(async () => {}),
    pendingContinuation: undefined as undefined | (() => void | Promise<void>),
    registerContinuation: vi.fn(),
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
    getPersistentStorageAuthorityEpoch: vi.fn(() => 2),
    getPersistentDataRuntime: vi.fn(() => mocks.persistentRuntime),
}))
vi.mock('../../../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: mocks.reloadPlugins,
}))
vi.mock('../../committedWorkingSetContinuation', () => ({
    registerCommittedWorkingSetContinuation: (
        _revision: number,
        _runtime: object,
        _authorityEpoch: number,
        continuation: () => void | Promise<void>,
    ) => {
        mocks.registerContinuation(_revision, _runtime, _authorityEpoch, continuation)
        mocks.pendingContinuation = continuation
    },
}))
vi.mock('../../persistentRevisionEvents', () => ({
    subscribeLocalPersistentRevision: (listener: (revision: number) => void) => {
        mocks.listener = listener
        return () => { mocks.listener = undefined }
    },
}))
vi.mock('../../portableBackupFileRouteProduction.svelte', () => ({
    restoreBackupFromNativeSource: mocks.restoreConflictSource,
    exportPortableBackupFromReferenceSource: mocks.exportConflictSource,
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
        retentionPolicy: { keepCount: 10, keepDays: 30 },
        capabilities: {
            immutableCreate: true,
            directCompleteRead: true,
            atomicCreateHead: true,
            conditionalHeadUpdate: false,
            stableHeadReplace: true,
            headReadAfterWrite: true,
            headRetryControl: true,
            leaseOperations: false,
            deleteObjects: true,
            conditionalGet: false,
            resumableUpload: false,
            range: false,
            snapshotDiscovery: true,
            maxStoredBytes: null,
            sdkOverheadBytes: 0,
            uploadAlignment: 1,
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
        mocks.pendingContinuation = undefined
        mocks.restoreConflictSource.mockImplementation(async (source) => {
            await source({
                signal: new AbortController().signal,
                onStatus: () => {},
                onSource: () => {},
            })
        })
        mocks.exportConflictSource.mockImplementation(async (source) => {
            await source()
        })
        mocks.bridge.openConflictSource.mockResolvedValue({
            source: { type: 'conflictReference', token: 'external:source-token' },
        })
        mocks.bridge.releaseConflictSource.mockResolvedValue(undefined)
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

    it('releases the replacement fence before authoritative restore plugin reload', async () => {
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
        const events: string[] = []
        mocks.releaseFence.mockImplementation(() => events.push('fence-released'))
        mocks.reloadPlugins.mockImplementation(async () => { events.push('plugins-reloaded') })
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
        expect(events).toEqual(['fence-released', 'plugins-reloaded'])
    })

    it('continues a committed restore once after read-only recovery without reactivation', async () => {
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
        mocks.refreshWorkingSet.mockResolvedValueOnce({
            kind: 'committed', revision: 24, projection: 'refresh-required',
        })

        await expect(requestExternalStorageRestore(
            'old-sync',
            'snapshot-1',
            ['library'],
        )).rejects.toBeInstanceOf(Error)

        expect(mocks.releaseFence).toHaveBeenCalledOnce()
        expect(mocks.reloadPlugins).not.toHaveBeenCalled()
        expect(mocks.registerContinuation).toHaveBeenCalledWith(
            24,
            mocks.persistentRuntime,
            2,
            expect.any(Function),
        )
        expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
        expect(mocks.pendingContinuation).toBeTypeOf('function')
        await mocks.pendingContinuation!()
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
        expect(mocks.refreshWorkingSet).toHaveBeenCalledOnce()
    })

    it('activates a repository side that a conflict decision received', async () => {
        const {
            installExternalStorageProduction,
            requestExternalStorageResolveConflict,
        } = await import('./production')
        mocks.revision = 11
        mocks.bridge.startJob.mockResolvedValue({
            ...succeeded('old-sync', '11'),
            kind: 'resolve-conflict',
            state: 'waiting',
            phase: 'remote-apply',
            result: { receiveReady: true, snapshotId: 'remote', expectedRevision: '11' },
        })
        mocks.bridge.applyReceived.mockResolvedValue({
            snapshotId: 'remote', receivedRevision: '12',
        })
        mocks.bridge.getJob.mockResolvedValue({
            ...succeeded('old-sync', '11'),
            kind: 'resolve-conflict',
            result: { snapshotId: 'remote', receivedRevision: '12' },
        })
        await installExternalStorageProduction()
        await expect(requestExternalStorageResolveConflict('old-sync', 'conflict-1', 'remote'))
            .resolves.toMatchObject({ state: 'succeeded' })
        expect(mocks.flush).toHaveBeenCalledWith('external-storage-resolve-conflict')
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            connectionId: 'old-sync', kind: 'resolve-conflict',
            conflictId: 'conflict-1', choice: 'remote',
        }))
        expect(mocks.bridge.applyReceived).toHaveBeenCalledWith('job-old-sync', '11')
        expect(mocks.refreshWorkingSet).toHaveBeenCalledWith(12)
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
    })

    it.each(['01', '9007199254740992'])(
        'rejects an invalid received revision %s at the local revision boundary',
        async (receivedRevision) => {
            const {
                installExternalStorageProduction,
                requestExternalStorageResolveConflict,
            } = await import('./production')
            mocks.revision = 11
            mocks.bridge.startJob.mockResolvedValue({
                ...succeeded('old-sync', '11'),
                kind: 'resolve-conflict',
                state: 'waiting',
                phase: 'remote-apply',
                result: { receiveReady: true, snapshotId: 'remote', expectedRevision: '11' },
            })
            mocks.bridge.applyReceived.mockResolvedValue({
                snapshotId: 'remote', receivedRevision,
            })
            await installExternalStorageProduction()

            await expect(
                requestExternalStorageResolveConflict('old-sync', 'conflict-1', 'remote'),
            ).rejects.toBeInstanceOf(RangeError)
            expect(mocks.refreshWorkingSet).not.toHaveBeenCalled()
            expect(mocks.releaseFence).toHaveBeenCalledOnce()
        },
    )

    it('releases an opened conflict source after a cancelled restore', async () => {
        const { requestExternalConflictRestore } = await import('./production')
        mocks.restoreConflictSource.mockImplementationOnce(async (source) => {
            await source({
                signal: new AbortController().signal,
                onStatus: () => {},
                onSource: () => {},
            })
            throw new DOMException('cancelled', 'AbortError')
        })

        await expect(requestExternalConflictRestore('conflict-1', 'remote'))
            .rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.bridge.openConflictSource).toHaveBeenCalledWith(
            'conflict-1',
            'remote',
        )
        expect(mocks.bridge.releaseConflictSource).toHaveBeenCalledWith(
            'external:source-token',
        )
    })

    it('holds a conflict source through portable export and releases it after failure', async () => {
        const { requestExternalConflictExport } = await import('./production')
        mocks.exportConflictSource.mockImplementationOnce(async (source) => {
            await source()
            throw new Error('synthetic export failure')
        })

        await expect(requestExternalConflictExport('conflict-1', 'local'))
            .rejects.toThrow('synthetic export failure')
        expect(mocks.bridge.openConflictSource).toHaveBeenCalledWith(
            'conflict-1',
            'local',
        )
        expect(mocks.bridge.releaseConflictSource).toHaveBeenCalledWith(
            'external:source-token',
        )
    })

    it('defers a received repository continuation without applying it again', async () => {
        const {
            installExternalStorageProduction,
            requestExternalStorageResolveConflict,
        } = await import('./production')
        mocks.revision = 11
        mocks.bridge.startJob.mockResolvedValue({
            ...succeeded('old-sync', '11'),
            kind: 'resolve-conflict',
            state: 'waiting',
            phase: 'remote-apply',
            result: { receiveReady: true, snapshotId: 'remote', expectedRevision: '11' },
        })
        mocks.bridge.applyReceived.mockResolvedValue({
            snapshotId: 'remote', receivedRevision: '12',
        })
        mocks.refreshWorkingSet.mockResolvedValueOnce({
            kind: 'committed', revision: 12, projection: 'refresh-required',
        })
        await installExternalStorageProduction()

        await expect(
            requestExternalStorageResolveConflict('old-sync', 'conflict-1', 'remote'),
        ).rejects.toBeInstanceOf(Error)

        expect(mocks.bridge.applyReceived).toHaveBeenCalledOnce()
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
        expect(mocks.reloadPlugins).not.toHaveBeenCalled()
        await mocks.pendingContinuation!()
        expect(mocks.bridge.applyReceived).toHaveBeenCalledOnce()
        expect(mocks.refreshWorkingSet).toHaveBeenCalledOnce()
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
    })

    it('reports a conflict decision that did not finish instead of returning it', async () => {
        const {
            installExternalStorageProduction,
            requestExternalStorageResolveConflict,
        } = await import('./production')
        mocks.bridge.startJob.mockResolvedValue({
            ...succeeded('old-sync', '11'),
            kind: 'resolve-conflict',
            state: 'failed',
            phase: 'failed',
            error: { code: 'transient', message: 'Repository unreachable', action: 'retry' },
            result: undefined,
        })
        await installExternalStorageProduction()
        await expect(requestExternalStorageResolveConflict('old-sync', 'conflict-1', 'remote'))
            .rejects.toMatchObject({ name: 'transient' })
        expect(mocks.bridge.applyReceived).not.toHaveBeenCalled()
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

})
