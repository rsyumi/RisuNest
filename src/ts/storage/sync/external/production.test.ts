import type { MobileBackgroundTask, MobileTaskKind } from '../../../mobileBackgroundTask'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ExternalConnectionSummary, ExternalJobSummary, ExternalStorageState } from './types'
import type { CommittedApplyOutcome } from '../../persistentDataRuntime'
import { createLeaseStub, createStoreStub } from '../../persistentDataStore.testSupport'

const mocks = vi.hoisted(() => ({
    protected: false,
    desktop: false,
    protectScopes: false,
    scopes: 0,
    backgroundChanged: undefined as (() => void) | undefined,
    backgroundSignal: undefined as AbortSignal | undefined,
    persistentRuntime: { subscribeActiveConversationViewportSource: () => () => {} },
    revision: 8,
    listener: undefined as ((revision: number) => void) | undefined,
    bridge: {
        supported: true,
        getState: vi.fn(),
        setExecutionSession: vi.fn(async (_request: {
            kind: 'foreground' | 'hidden'
            id: string
        }) => {}),
        startJob: vi.fn(),
        getJob: vi.fn(),
        cancelJob: vi.fn(),
        stopRestore: vi.fn(),
        confirmRestoreAdoption: vi.fn(async () => {}),
        openConflictSource: vi.fn(),
        releaseConflictSource: vi.fn(),
    },
    restoreConflictSource: vi.fn(),
    exportConflictSource: vi.fn(),
    flush: vi.fn(async (_reason: string) => {}),
    refreshWorkingSet: vi.fn(async (revision: number): Promise<CommittedApplyOutcome> => ({
        kind: 'committed', revision, projection: 'applied',
    })),
    refreshReleased: vi.fn(async (revision: number): Promise<CommittedApplyOutcome> => ({
        kind: 'committed', revision, projection: 'applied',
    })),
    releaseFence: vi.fn(),
    openStore: vi.fn(async () => {}),
    reloadPlugins: vi.fn(async () => {}),
    fencePlugins: vi.fn(async () => {}),
    bindingFence: vi.fn(async () => {}),
    bindingAssert: vi.fn(async () => {}),
    bindingResume: vi.fn(async () => {}),
    completeGuard: vi.fn(),
    pendingContinuation: undefined as undefined | (() => void | Promise<void>),
    registerContinuation: vi.fn(),
    persistentBackgroundError: vi.fn(),
}))

vi.mock('../../../mobileBackgroundTask', async original => {
    const actual = await original<typeof import('../../../mobileBackgroundTask')>()
    return {
        ...actual,
        beginMobileBackgroundTask: vi.fn(actual.beginMobileBackgroundTask),
        hasMobileBackgroundTasks: () => mocks.protected || (mocks.protectScopes && mocks.scopes > 0),
        runWithMobileBackgroundTask<T>(kind: MobileTaskKind, operation: (task: MobileBackgroundTask) => Promise<T>, signal?: AbortSignal) {
            mocks.scopes++
            return actual.runWithMobileBackgroundTask(kind, operation, signal ?? mocks.backgroundSignal).finally(() => {
                mocks.scopes--
                mocks.backgroundChanged?.()
            })
        },
        subscribeMobileBackgroundTasks: (listener: () => void) => {
            mocks.backgroundChanged = listener
            return () => { mocks.backgroundChanged = undefined }
        },
    }
})

// Tauri desktop also starts the sync adapters, which read the native binding; this library has none.
vi.mock('@tauri-apps/api/core', async importOriginal => ({
    ...await importOriginal<typeof import('@tauri-apps/api/core')>(),
    invoke: vi.fn(async (command: string) => {
        if (command === 'pds_lww_binding_state') return { target: { kind: 'none' }, targetAuthority: '0', selectionEpoch: '0', libraryId: null, progress: null }
        throw new Error(`Unexpected native command ${command}`)
    }),
}))
vi.mock('@tauri-apps/api/event', async importOriginal => ({
    ...await importOriginal<typeof import('@tauri-apps/api/event')>(), listen: vi.fn(async () => () => {}),
}))
vi.mock('../../../platform', async () => (await import('../../../platform.testSupport')).platformModule(() => ({ runtime: mocks.desktop ? 'tauri' : 'web', os: 'windows' })))
vi.mock('../../../stores.svelte', async () => ({ selectedCharID: (await import('svelte/store')).writable(-1) }))
vi.mock('../../database.svelte', () => ({ getDatabase: () => ({ characters: [] }) }))
vi.mock('../../deviceStateRestore', () => ({ flushDeviceStateBeforeRestore: vi.fn(), refreshDeviceStateAfterRestore: vi.fn() }))

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
    refreshActiveWorkingSetFromStore: mocks.refreshReleased,
    getPersistentStorageAuthorityEpoch: vi.fn(() => 2),
    getPersistentDataRuntime: vi.fn(() => mocks.persistentRuntime),
}))
vi.mock('../../../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: mocks.reloadPlugins,
}))
vi.mock('../../../plugins/apiV3/v3.svelte', () => ({
    fencePluginExecutionForAuthorityReplacement: mocks.fencePlugins,
    invalidatePluginCachesAfterAuthorityReplacement: vi.fn(async () => {}),
    restartPluginsAfterAuthorityReplacement: mocks.reloadPlugins,
}))
vi.mock('../bindingRegistry', async original => ({
    ...await original<typeof import('../bindingRegistry')>(),
    prepareBoundLibraryReplacement: vi.fn(async () => ({
        fence: mocks.bindingFence, assertAuthority: mocks.bindingAssert, resume: mocks.bindingResume, release: () => {},
    })),
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
        paused: false,
    },
    connections: [{
        id: 'old-sync', providerId: 'webdav', purpose: 'backup', strategy: 'backup-only',
        mode: 'existing', displayName: 'Old', endpoint: {
            providerId: 'webdav', authority: 'synthetic.invalid', repositoryHint: 'old',
            warnings: [], remoteVerified: true,
        },
        retentionPolicy: { keepCount: 10, keepDays: 30 },
        transferConcurrency: 4,
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
        status: 'ready', automaticBackupPaused: false,
    }],
    jobs: [],
}

function succeeded(connectionId: string, revision: string): ExternalJobSummary {
    return {
        id: `job-${connectionId}`,
        connectionId,
        kind: 'backup',
        state: 'succeeded',
        phase: 'complete',
        completedBytes: '0',
        completedItems: '0',
        startedAtMs: '1',
        updatedAtMs: '2',
        result: { publishedRevision: revision as `${number}` },
    }
}

async function installActualRestorePause() {
    const { createPersistentDataRuntime, capturePersistentRoot } = await import('../../persistentDataRuntime')
    const { makeDatabase } = await import('../../saveCoordinator.testSupport')
    const database = makeDatabase()
    database.characters = []
    let actual: ReturnType<typeof createPersistentDataRuntime>
    Object.assign(mocks.persistentRuntime, {
        store: { open: mocks.openStore },
        async withPausedPersistentWrites(reason: string, operation: (token: unknown) => Promise<unknown>) {
            const store = createStoreStub({
                open: async () => undefined,
                readRoot: async () => ({ revision: mocks.revision, value: capturePersistentRoot(database) }),
                acquireRevision: async (revision: number) => createLeaseStub({
                    revision, readRoot: async () => ({ revision, value: capturePersistentRoot(database) }),
                    queryPresets: async () => ({ revision, items: [] }),
                    queryCharacters: async () => ({ revision, items: [] }),
                    queryPluginStorage: async () => ({ revision, items: [] }),
                    release: async () => {},
                }),
            })
            actual = createPersistentDataRuntime({
                store, state: {
                    captureRoot: () => capturePersistentRoot(database), captureSelectedCharacter: () => null,
                    captureCharacter: () => null,
                    getSelectedCharacterId: () => undefined, getSelectedConversationId: () => null,
                    replaceDatabase: () => {}, publishCharacter: () => {}, publishConversation: () => {},
                }, prepareDatabase: async value => value,
                clock: { setTimeout: () => 1, clearTimeout: () => {} },
                onBackgroundError: mocks.persistentBackgroundError,
            })
            await actual.initializeActiveWorkingSet(database)
            return actual.withPausedPersistentWrites(reason, operation).finally(() => mocks.releaseFence())
        },
        beginActivatedLibraryGuard(token: import('../../saveCoordinator').PersistentMutationToken) {
            const guard = actual.beginActivatedLibraryGuard(token)
            return { ...guard, complete() { guard.complete(); mocks.completeGuard() } }
        },
        async refreshActivatedLibraryUnderPause(token: import('../../saveCoordinator').PersistentMutationToken) {
            const revision = token.revision + 1
            const outcome = await mocks.refreshWorkingSet(revision)
            if (outcome.projection !== 'applied') return outcome
            mocks.revision = revision
            return actual.refreshActivatedLibraryUnderPause(token)
        },
    })
}

describe('external storage production integration', () => {
    describe('committed restore retries', () => {
        let production: typeof import('./production')
        let recovery: typeof import('./applicationRecovery')

        beforeEach(async () => {
            production = await import('./production')
            recovery = await import('./applicationRecovery')
        })

        it.each(['adoption', 'settled'] as const)('retries a committed restore after one %s failure without completing its actual guard twice', async failure => {
            await production.installExternalStorageProduction()
            mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
                ...succeeded('old-sync', '8'), id, kind: 'restore', applicationStarted: true,
                result: { receivedRevision: '9' },
            }))
            if (failure === 'adoption') mocks.bridge.confirmRestoreAdoption.mockRejectedValueOnce(new Error('Lost adoption response'))
            else mocks.bindingResume.mockRejectedValueOnce(new Error('Adapter resume failed'))
            await expect(production.requestExternalStorageRestore('old-sync', 'snapshot', ['library'])).rejects.toThrow()
            expect(mocks.completeGuard).toHaveBeenCalledTimes(failure === 'adoption' ? 0 : 1)
            await recovery.retryExternalApplication()
            expect(mocks.completeGuard).toHaveBeenCalledOnce()
            expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
            expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
            expect(recovery.hasPendingExternalApplication()).toBe(false)
        })
    })

    it.each([false, true])('restores the original plugins and binding after proven unchanged pause admission failure (plugin restart fails: %s)', async restartFails => {
        const production = await import('./production')
        await production.installExternalStorageProduction()
        Object.assign(mocks.persistentRuntime, { withPausedPersistentWrites: async () => { throw new Error('Pause unavailable') } })
        if (restartFails) mocks.reloadPlugins.mockRejectedValueOnce(new Error('Plugin restart failed'))
        await expect(production.requestExternalStorageRestore('old-sync', 'snapshot', ['library'])).rejects.toThrow(restartFails ? 'Plugin restart failed' : 'Pause unavailable')
        expect(mocks.bindingAssert).toHaveBeenCalledOnce()
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.bindingResume).toHaveBeenCalledOnce()
        expect(mocks.bridge.startJob).not.toHaveBeenCalled()
    })

    it('releases an abandoned external restore when preparation and authority verification fail', async () => {
        const production = await import('./production')
        await production.installExternalStorageProduction()
        const registry = await import('../bindingRegistry')
        const actual = await vi.importActual<typeof import('../bindingRegistry')>('../bindingRegistry')
        vi.mocked(registry.prepareBoundLibraryReplacement).mockImplementationOnce(actual.prepareBoundLibraryReplacement)
        const { invoke } = await import('@tauri-apps/api/core')
        const original = vi.mocked(invoke).getMockImplementation()!
        const target = { kind: 'external' as const, connectionId: 'old-sync' }
        let unavailable = false
        vi.mocked(invoke).mockImplementation(async () => {
            if (unavailable) throw new Error('Binding state unavailable')
            return { target, libraryId: 'synthetic', targetAuthority: '1', selectionEpoch: '1', progress: null }
        })
        const unsupported = async (): Promise<never> => { throw new Error('Unexpected binding switch') }
        const unregister = actual.registerSyncBindingTransport(target, {
            inspectTarget: unsupported, pullAvailableState: unsupported, replaceFromTarget: unsupported,
            publishInitialSharedState: unsupported, fenceOldJobs: async () => {},
            resumeBinding: async () => {}, receiveAvailableChanges: async () => {},
        })
        mocks.fencePlugins.mockImplementationOnce(async () => {
            expect(actual.isSyncBindingReplacementPending()).toBe(true)
            unavailable = true
            throw new Error('Plugin preparation failed')
        })
        try {
            await expect(production.requestExternalStorageRestore('old-sync', 'snapshot', ['library'])).rejects.toThrow('Binding state unavailable')
            expect(actual.isSyncBindingReplacementPending()).toBe(false)
            expect(mocks.bridge.startJob).not.toHaveBeenCalled()
        } finally { unregister(); vi.mocked(invoke).mockImplementation(original) }
    })

    it('keeps desktop work alive when the window becomes hidden', async () => {
        mocks.desktop = true
        const { installExternalStorageProduction } = await import('./production')
        await installExternalStorageProduction()
        mocks.bridge.setExecutionSession.mockClear()
        const visibility = vi.spyOn(document, 'visibilityState', 'get').mockReturnValue('hidden')
        document.dispatchEvent(new Event('visibilitychange'))
        await Promise.resolve()
        await Promise.resolve()
        expect(mocks.bridge.setExecutionSession).not.toHaveBeenCalled()
        expect(mocks.bridge.cancelJob).not.toHaveBeenCalled()
        visibility.mockRestore()
    })

    it.each([false, true])('defers automatic backups through file selection and waits for an existing backup (desktop=%s)', async desktop => {
        vi.useFakeTimers()
        mocks.desktop = desktop
        const admission = await import('../../libraryFileOperation')
        const { installExternalStorageProduction } = await import('./production')
        const running: ExternalJobSummary = { ...succeeded('old-sync', '8'), state: 'running', phase: 'upload', result: undefined }
        mocks.bridge.startJob.mockResolvedValue(running)
        mocks.bridge.getJob.mockResolvedValue(running)
        const dispose = await installExternalStorageProduction()
        await vi.advanceTimersByTimeAsync(60_000)
        expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
        const release = admission.reserveLibraryFileOperation()
        const ready = vi.fn()
        const waiting = Promise.resolve(admission.waitForLibraryFileOperation(new AbortController().signal)).then(ready)
        const visibility = vi.spyOn(document, 'visibilityState', 'get')
        try {
            mocks.bridge.setExecutionSession.mockClear()
            visibility.mockReturnValue('hidden'); document.dispatchEvent(new Event('visibilitychange'))
            visibility.mockReturnValue('visible'); document.dispatchEvent(new Event('visibilitychange'))
            window.dispatchEvent(new Event('focus'))
            mocks.revision = 9; mocks.listener?.(9)
            await vi.advanceTimersByTimeAsync(65_000)
            expect(ready).not.toHaveBeenCalled()
            expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
            expect(mocks.bridge.setExecutionSession).not.toHaveBeenCalled()
            mocks.bridge.getJob.mockResolvedValue(succeeded('old-sync', '8'))
            await vi.advanceTimersByTimeAsync(1_000); await waiting
            expect(ready).toHaveBeenCalledOnce()
            expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
            mocks.bridge.startJob.mockImplementation(async request => succeeded(request.connectionId, request.targetRevision))
            release(); await vi.advanceTimersByTimeAsync(65_000)
            expect(mocks.bridge.startJob).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        } finally {
            release(); dispose(); visibility.mockRestore(); vi.useRealTimers()
        }
    })

    it('keeps a running automatic backup alive when the device goes offline', async () => {
        vi.useFakeTimers()
        try {
            const running = (id: string): ExternalJobSummary => ({
                ...succeeded('old-sync', '8'), id, state: 'running', phase: 'upload', result: undefined,
            })
            mocks.bridge.startJob.mockImplementation(async () => running('automatic-backup'))
            mocks.bridge.getJob.mockImplementation(async id => running(id))
            const { installExternalStorageProduction } = await import('./production')
            await installExternalStorageProduction()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
            window.dispatchEvent(new Event('offline'))
            await vi.advanceTimersByTimeAsync(1_000)
            expect(mocks.bridge.cancelJob).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it.each([
        ['pin-history', 'pinRequest', { snapshotId: 'snapshot-8' }],
        ['delete-history', 'deleteRequest', { pointId: 'point-8', pointObservation: 'observation-8', confirmOtherDevice: true, confirmLastRetained: true }],
        ['check-repository', 'checkRequest', { snapshotId: 'snapshot-8' }],
    ] as const)('resumes %s with the exact retained request and job ID after recovery', async (kind, field, details) => {
        const { installExternalStorageProduction, resumeExternalStorageJob } = await import('./production')
        await installExternalStorageProduction()
        const job: ExternalJobSummary = { ...succeeded('old-sync', '8'), id: 'retained-history', kind,
            state: 'waiting', phase: 'paused', reason: 'manual', [field]: details }
        await expect(resumeExternalStorageJob(job)).resolves.toMatchObject({ kind: 'complete' })
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            connectionId: 'old-sync', kind, reason: 'manual', ...details,
        }), 'retained-history')
        expect(mocks.flush).not.toHaveBeenCalled()
    })

    it('cancels a stopped restore before releasing its edit fence', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        await installExternalStorageProduction()
        let stopped: ExternalJobSummary
        mocks.bridge.startJob.mockImplementation(async (_request, id) => stopped = {
            ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'waiting', phase: 'paused',
            error: { code: 'reauthRequired', message: 'Sign in required', action: 'reauthenticate', retryable: false },
        })
        mocks.bridge.cancelJob.mockImplementation(async () => {
            expect(mocks.releaseFence).not.toHaveBeenCalled()
            return { ...stopped, state: 'cancelled', applicationStarted: false }
        })
        await expect(requestExternalStorageRestore('old-sync', 'snapshot', ['library'])).rejects.toThrow('Sign in required')
        expect(mocks.bridge.cancelJob).toHaveBeenCalledExactlyOnceWith(expect.any(String), true)
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
    })

    it('marks a restore it stops after three failed retries as stopped by the app', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        await installExternalStorageProduction()
        vi.useFakeTimers()
        try {
            const paused = (id: string): ExternalJobSummary => ({
                ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'waiting', phase: 'paused',
                error: { code: 'transient', message: 'Retry', action: 'retry', retryable: true },
            })
            mocks.bridge.startJob.mockImplementation(async (_request, id) => paused(id))
            mocks.bridge.cancelJob.mockImplementation(async (id, stoppedByApp) => ({
                ...paused(id), state: 'cancelled', phase: 'cancelled', applicationStarted: false, stoppedByApp,
            }))
            const operation = expect(requestExternalStorageRestore('old-sync', 'snapshot', ['library'])).rejects.toThrow('Retry')
            await vi.advanceTimersByTimeAsync(3 * 5_100)
            await operation
            expect(mocks.bridge.startJob).toHaveBeenCalledTimes(4)
            expect(mocks.bridge.cancelJob).toHaveBeenCalledExactlyOnceWith(mocks.bridge.startJob.mock.calls[0][1], true)
        } finally { vi.useRealTimers() }
    })

    it('leaves a restore stopped through its background task as a cancellation', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        await installExternalStorageProduction()
        const stop = new AbortController()
        stop.abort()
        mocks.backgroundSignal = stop.signal
        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'waiting', phase: 'paused',
            error: { code: 'transient', message: 'Retry', action: 'retry', retryable: true },
        }))
        mocks.bridge.cancelJob.mockImplementation(async id => ({
            ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'cancelled', applicationStarted: false,
        }))
        await expect(requestExternalStorageRestore('old-sync', 'snapshot', ['library'])).rejects.toThrow()
        expect(mocks.bridge.cancelJob).toHaveBeenCalledExactlyOnceWith(expect.any(String), false)
    })

    it('retries a stopped restore with the identical admission request while retaining its fence', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        await installExternalStorageProduction()
        vi.useFakeTimers()
        try {
            let starts = 0
            mocks.bridge.startJob.mockImplementation(async (_request, id) => {
                expect(mocks.releaseFence).not.toHaveBeenCalled()
                return ++starts === 1 ? {
                    ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'waiting', phase: 'paused',
                    error: { code: 'transient', message: 'Retry', action: 'retry', retryable: true },
                } : { ...succeeded('old-sync', '8'), id, kind: 'restore', applicationStarted: true, result: { receivedRevision: '9' } }
            })
            const operation = requestExternalStorageRestore('old-sync', 'snapshot', ['library'])
            await vi.advanceTimersByTimeAsync(5_100)
            await expect(operation).resolves.toMatchObject({ state: 'succeeded' })
            const calls = mocks.bridge.startJob.mock.calls
            expect(calls).toHaveLength(2)
            expect(calls[1]).toEqual(calls[0])
            expect(mocks.releaseFence).toHaveBeenCalledOnce()
        } finally { vi.useRealTimers() }
    })

    it('reports deferred history deletion without polling a stopped manual job forever', async () => {
        const { installExternalStorageProduction, requestExternalStorageDeleteHistory } = await import('./production')
        const dispose = await installExternalStorageProduction()
        mocks.bridge.startJob.mockResolvedValue({
            ...succeeded('old-sync', '1'), kind: 'delete-history', state: 'waiting',
            error: { code: 'Transient', message: 'Deletion deferred' },
        })
        await expect(requestExternalStorageDeleteHistory('old-sync', {
            id: 'point', pointId: 'point', pointObservation: 'authenticated-observation',
            deletable: true, snapshotId: 'snapshot', kind: 'backup-point',
            createdAtMs: '1', logicalRevision: '1', pinned: false,
            complete: true, verified: true, includedSections: [], sameDevice: true,
        }, false, false)).rejects.toThrow('Deletion deferred')
        expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
            kind: 'delete-history', pointId: 'point',
            pointObservation: 'authenticated-observation',
            confirmOtherDevice: false, confirmLastRetained: false,
        }))
        expect(mocks.bridge.getJob).not.toHaveBeenCalled()
        dispose()
    })

    beforeEach(async () => {
        mocks.desktop = false
        mocks.protected = false
        mocks.protectScopes = false
        mocks.scopes = 0
        mocks.backgroundChanged = undefined
        mocks.backgroundSignal = undefined
        vi.resetModules()
        vi.clearAllMocks()
        const persistent = await import('../../persistentDataRuntime.svelte')
        vi.mocked(persistent.capturePersistentMutationToken).mockReset().mockImplementation(async () => ({
            revision: mocks.revision, mutationGeneration: 1,
        }))
        vi.mocked(persistent.acquireDestructiveReplacementFence).mockReset().mockImplementation(async () => ({
            revision: mocks.revision,
            refreshCommittedWorkingSet: mocks.refreshWorkingSet,
            release: mocks.releaseFence,
        }))
        delete (mocks.persistentRuntime as { revision?: number }).revision
        mocks.releaseFence.mockReset()
        mocks.openStore.mockReset().mockResolvedValue(undefined)
        mocks.flush.mockReset().mockResolvedValue(undefined)
        mocks.reloadPlugins.mockReset().mockResolvedValue(undefined)
        mocks.fencePlugins.mockReset().mockResolvedValue(undefined)
        mocks.bindingFence.mockReset().mockResolvedValue(undefined)
        mocks.bindingAssert.mockReset().mockResolvedValue(undefined)
        mocks.bindingResume.mockReset().mockResolvedValue(undefined)
        mocks.completeGuard.mockReset()
        mocks.bridge.confirmRestoreAdoption.mockReset().mockResolvedValue(undefined)
        await installActualRestorePause()
        mocks.refreshWorkingSet.mockReset().mockImplementation(async revision => ({
            kind: 'committed', revision, projection: 'applied',
        }))
        mocks.refreshReleased.mockReset().mockImplementation(async revision => ({
            kind: 'committed', revision, projection: 'applied',
        }))
        mocks.bridge.cancelJob.mockReset().mockImplementation(async id => ({ ...succeeded('old-sync', '8'), id, state: 'cancelled' }))
        mocks.bridge.getJob.mockReset()
        mocks.bridge.getJob.mockImplementation(async id => (await mocks.bridge.getState()).jobs.find((job: ExternalJobSummary) => job.id === id))
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
        mocks.bridge.startJob.mockReset().mockImplementation(async request =>
            succeeded(request.connectionId, request.targetRevision ?? '0'))
    })
    afterEach(async () => {
        const { installExternalStorageProduction } = await import('./production')
        const dispose = await installExternalStorageProduction()
        dispose()
    })

    it('stops an unfinished restore by its ID and takes the settled native state', async () => {
        const { installExternalStorageProduction, stopExternalStorageRestore } = await import('./production')
        await installExternalStorageProduction()
        const stopped: ExternalJobSummary = { ...succeeded('old-sync', '8'), id: 'unfinished-restore', kind: 'restore',
            state: 'failed', phase: 'paused', applicationStarted: true }
        mocks.bridge.stopRestore.mockResolvedValue(stopped)
        mocks.bridge.getState.mockClear()
        await expect(stopExternalStorageRestore('unfinished-restore')).resolves.toBe(stopped)
        expect(mocks.bridge.stopRestore).toHaveBeenCalledWith('unfinished-restore')
        expect(mocks.bridge.getState).toHaveBeenCalledOnce()
        expect(mocks.bridge.startJob).not.toHaveBeenCalled()
        expect(mocks.flush).not.toHaveBeenCalled()
    })

    it('keeps the same protected execution session through Home and foreground return', async () => {
        let visibility: DocumentVisibilityState = 'visible'
        const visible = vi.spyOn(document, 'visibilityState', 'get').mockImplementation(() => visibility)
        try {
            const { installExternalStorageProduction } = await import('./production')
            await installExternalStorageProduction()
            const session = mocks.bridge.setExecutionSession.mock.calls[0][0]
            mocks.protected = true
            visibility = 'hidden'
            document.dispatchEvent(new Event('visibilitychange'))
            await Promise.resolve()
            await Promise.resolve()
            expect(mocks.bridge.setExecutionSession).toHaveBeenCalledExactlyOnceWith(session)
            expect(mocks.bridge.cancelJob).not.toHaveBeenCalled()
            visibility = 'visible'
            document.dispatchEvent(new Event('visibilitychange'))
            await vi.waitFor(() => expect(mocks.bridge.getState).toHaveBeenCalledTimes(2))
            expect(mocks.bridge.setExecutionSession).toHaveBeenCalledExactlyOnceWith(session)
            visibility = 'hidden'
            document.dispatchEvent(new Event('visibilitychange'))
            await Promise.resolve()
            mocks.desktop = false
        mocks.protected = false
            mocks.backgroundChanged?.()
            await vi.waitFor(() => expect(mocks.bridge.setExecutionSession).toHaveBeenCalledTimes(2))
            expect(mocks.bridge.setExecutionSession.mock.lastCall?.[0].kind).toBe('hidden')
            // Routine work that begins on return does not keep the hidden session.
            mocks.protected = true
            visibility = 'visible'
            document.dispatchEvent(new Event('visibilitychange'))
            await vi.waitFor(() => expect(mocks.bridge.setExecutionSession).toHaveBeenCalledTimes(3))
            expect(mocks.bridge.setExecutionSession.mock.lastCall?.[0].kind).toBe('foreground')
        } finally { visible.mockRestore() }
    })

    it('holds a keep or check job until it settles and leaves an unprotected one unpolled', async () => {
        vi.useFakeTimers()
        try {
            const { beginMobileBackgroundTask } = await import('../../../mobileBackgroundTask')
            const { startExternalStorageMaintenanceJob } = await import('./production')
            const hold = (state: 'active' | 'refused') => ({
                protection: state === 'active' ? { state } as const : { state, reason: 'background' } as const,
                progress: vi.fn(), dispose: vi.fn(async (_success?: boolean) => {}),
            })
            const running: ExternalJobSummary = {
                ...succeeded('old-sync', '8'), kind: 'check-repository', state: 'running', phase: 'verify',
                completedBytes: '1', totalBytes: '4', result: undefined,
            }
            mocks.bridge.startJob.mockResolvedValue(running)
            mocks.bridge.getJob
                .mockResolvedValueOnce({ ...running, completedBytes: '2' })
                .mockResolvedValueOnce({ ...running, state: 'succeeded', completedBytes: '4' })
            const active = hold('active')
            vi.mocked(beginMobileBackgroundTask).mockResolvedValueOnce(active)
            const request = { connectionId: 'old-sync', kind: 'check-repository', reason: 'manual' } as const
            await expect(startExternalStorageMaintenanceJob(request)).resolves.toEqual(running)
            expect(beginMobileBackgroundTask).toHaveBeenCalledExactlyOnceWith('maintenance', undefined, true)
            await vi.advanceTimersByTimeAsync(500)
            expect(active.progress).toHaveBeenLastCalledWith(50)
            expect(active.dispose).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(active.dispose).toHaveBeenCalledExactlyOnceWith(true)

            const refused = hold('refused')
            vi.mocked(beginMobileBackgroundTask).mockResolvedValueOnce(refused)
            await startExternalStorageMaintenanceJob(request)
            expect(refused.dispose).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(1_000)
            expect(mocks.bridge.getJob).toHaveBeenCalledTimes(2)
        } finally { vi.useRealTimers() }
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

    it('queues the current durable revision after reconnecting without another edit', async () => {
        vi.useFakeTimers()
        try {
            const { installExternalStorageProduction } = await import('./production')
            await installExternalStorageProduction()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
            mocks.bridge.startJob.mockClear()
            mocks.revision = 12
            window.dispatchEvent(new Event('online'))
            await vi.advanceTimersByTimeAsync(60_001)
            expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
                connectionId: 'old-sync', targetRevision: '12', reason: 'automatic',
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('registers only the sync transports for a start that left sync off and refreshes them after a settings change', async () => {
        vi.useFakeTimers()
        const lww = { install: vi.fn(async () => () => {}), refresh: vi.fn(async () => {}) }
        vi.doMock('./lwwProduction', () => ({
            installExternalLwwAdapters: lww.install, refreshExternalLwwAdapters: lww.refresh, externalLwwExitDrain: vi.fn(),
        }))
        try {
            const { installExternalSyncTransports, refreshExternalStorageProductionState } = await import('./production')
            const dispose = await installExternalSyncTransports()
            expect(lww.install).toHaveBeenCalledExactlyOnceWith(initialState, false)
            expect(mocks.bridge.setExecutionSession).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).not.toHaveBeenCalled()
            const added = { ...initialState, connections: [...initialState.connections, { ...initialState.connections[0], id: 'new-sync', purpose: 'sync' as const }] }
            mocks.bridge.getState.mockResolvedValue(added)
            await refreshExternalStorageProductionState()
            expect(lww.refresh).toHaveBeenCalledExactlyOnceWith(added)
            dispose()
        } finally {
            vi.doUnmock('./lwwProduction')
            vi.useRealTimers()
        }
    })

    it('queues the already-saved revision for a new destination without another edit', async () => {
        vi.useFakeTimers()
        try {
            const { installExternalStorageProduction, refreshExternalStorageProductionState } = await import('./production')
            await installExternalStorageProduction()
            mocks.revision = 21
            mocks.bridge.getState.mockResolvedValue({
                ...initialState,
                selection: { ...initialState.selection, connectionId: 'new-sync' },
                connections: [{ ...initialState.connections[0], id: 'new-sync' }],
            })
            await refreshExternalStorageProductionState()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
            expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
                connectionId: 'new-sync', targetRevision: '21', reason: 'automatic',
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('resumes automatic backup after a newly verified connection repair', async () => {
        vi.useFakeTimers()
        try {
            const { installExternalStorageProduction, refreshExternalStorageProductionState } = await import('./production')
            await installExternalStorageProduction()
            mocks.bridge.startJob.mockRejectedValueOnce({ kind: 'reauthRequired' })
            mocks.listener?.(8)
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledTimes(1)
            mocks.revision = 9
            mocks.listener?.(9)
            await refreshExternalStorageProductionState()
            await vi.advanceTimersByTimeAsync(300_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledTimes(1)
            mocks.bridge.getState.mockResolvedValue({
                ...initialState,
                connections: [{ ...initialState.connections[0], status: 'paused', automaticBackupPaused: true, lastVerifiedAtMs: '400000' }],
            })
            await refreshExternalStorageProductionState()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledTimes(1)
            mocks.bridge.getState.mockResolvedValue({
                ...initialState,
                connections: [{ ...initialState.connections[0], lastVerifiedAtMs: '400000' }],
            })
            await refreshExternalStorageProductionState()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenCalledTimes(2)
            expect(mocks.bridge.startJob).toHaveBeenLastCalledWith(expect.objectContaining({ targetRevision: '9' }))
        } finally {
            vi.useRealTimers()
        }
    })

    it.each([
        ['signs in again', [{ lastVerifiedAtMs: '400000' }]],
        ['is removed', []],
    ] as const)('holds a backup that a read through another storage stopped until that storage %s', async (_, after) => {
        vi.useFakeTimers()
        try {
            const drive = { ...initialState.connections[0], id: 'drive-2', displayName: 'Drive', lastVerifiedAtMs: '100' as const }
            const withDrive = (others: readonly Partial<ExternalConnectionSummary>[]): ExternalStorageState =>
                ({ ...initialState, connections: [initialState.connections[0], ...others.map(other => ({ ...drive, ...other }))] })
            mocks.bridge.getState.mockResolvedValue(withDrive([{}]))
            const refused: ExternalJobSummary = { ...succeeded('old-sync', '0'), state: 'waiting', phase: 'paused', result: undefined,
                error: { code: 'reauthRequired', message: '', retryable: false, action: 'reauthenticate', connectionId: 'drive-2' } }
            let refusing = true
            mocks.bridge.startJob.mockImplementation(async request => {
                if (request.connectionId !== 'old-sync' || !refusing) return succeeded(request.connectionId, request.targetRevision ?? '0')
                refusing = false
                return refused
            })
            const started = (id: string) => mocks.bridge.startJob.mock.calls.filter(([request]) => request.connectionId === id).length
            const { installExternalStorageProduction, refreshExternalStorageProductionState } = await import('./production')
            await installExternalStorageProduction()
            mocks.listener?.(8)
            await vi.advanceTimersByTimeAsync(60_000)
            expect([started('old-sync'), started('drive-2')]).toEqual([1, 1])

            // The refusing storage holds its own backups too until the state read after its renewal.
            mocks.revision = 9
            mocks.listener?.(9)
            await vi.advanceTimersByTimeAsync(300_000)
            expect([started('old-sync'), started('drive-2')]).toEqual([1, 1])

            mocks.bridge.getState.mockResolvedValue(withDrive(after))
            await refreshExternalStorageProductionState()
            await vi.advanceTimersByTimeAsync(60_000)
            expect(started('old-sync')).toBe(2)

            // A storage that signed in again backs up its own changes too.
            const before = started('drive-2')
            mocks.revision = 10
            mocks.listener?.(10)
            await vi.advanceTimersByTimeAsync(60_000)
            expect(started('drive-2') > before).toBe(after.length > 0)
        } finally {
            vi.useRealTimers()
        }
    })

    it.each([false, true])('uses a matching unfinished restore after reopen and refuses a different request (%s)', async (different) => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        const pending = {
            ...succeeded('old-sync', '8'), id: 'existing-restore', kind: 'restore',
            state: 'uncertain', phase: 'local-apply-unknown', applicationStarted: true,
            restoreRequest: { snapshotId: 'snapshot-1', targetRevision: '8', restoreAreas: ['library'] },
        }
        mocks.bridge.getState.mockResolvedValue({ ...initialState, jobs: [pending] })
        await installExternalStorageProduction()
        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...pending, id, state: 'succeeded', result: { receivedRevision: '9' },
        }))
        const operation = requestExternalStorageRestore('old-sync', different ? 'other' : 'snapshot-1', ['library'])
        if (different) {
            await expect(operation).rejects.toThrow('different snapshot')
            expect(mocks.bridge.startJob).not.toHaveBeenCalled()
            expect(mocks.refreshWorkingSet).not.toHaveBeenCalled()
        } else {
            await expect(operation).resolves.toMatchObject({ state: 'succeeded' })
            expect(mocks.bridge.startJob).toHaveBeenCalledWith(expect.objectContaining({
                snapshotId: 'snapshot-1', targetRevision: '8', restoreAreas: ['library'],
            }), 'existing-restore')
        }
    })

    it.each([false, true])('unlocks a failed restore only when applicationStarted is false (%s)', async (started) => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        const recovery = await import('./applicationRecovery')
        await installExternalStorageProduction()
        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'failed',
            applicationStarted: started, result: undefined,
        }))
        await expect(requestExternalStorageRestore('old-sync', 'snapshot-1', ['library'])).rejects.toThrow()
        expect(mocks.releaseFence).toHaveBeenCalledTimes(started ? 0 : 1)
        expect(recovery.hasPendingExternalApplication()).toBe(started)
        expect(mocks.refreshWorkingSet).not.toHaveBeenCalled()
    })

    it('resumes the binding when the plugin restart after an unchanged restore fails, and settles on retry', async () => {
        vi.useFakeTimers()
        try {
            const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
            const recovery = await import('./applicationRecovery')
            await installExternalStorageProduction()
            mocks.bridge.startJob.mockImplementation(async (request, id) => request.kind === 'restore' ? ({
                ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'failed', applicationStarted: false, result: undefined,
            }) : succeeded(request.connectionId, request.targetRevision ?? '0'))
            mocks.reloadPlugins.mockRejectedValueOnce(new Error('Plugin restart failed'))
            await expect(requestExternalStorageRestore('old-sync', 'snapshot-1', ['library'])).rejects.toThrow('Plugin restart failed')
            expect(mocks.bindingResume).toHaveBeenCalledOnce()
            expect(recovery.hasPendingExternalApplication()).toBe(true)
            await expect(recovery.retryExternalApplication()).rejects.toThrow('External storage restore failed')
            expect(mocks.reloadPlugins).toHaveBeenCalledTimes(2)
            expect(recovery.hasPendingExternalApplication()).toBe(false)
            mocks.listener?.(8)
            await vi.advanceTimersByTimeAsync(60_000)
            expect(mocks.bridge.startJob).toHaveBeenLastCalledWith(expect.objectContaining({ kind: 'backup', targetRevision: '8' }))
        } finally {
            vi.useRealTimers()
        }
    })
    it('unlocks an unfinished restore once it is stopped', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        const recovery = await import('./applicationRecovery')
        await installExternalStorageProduction()
        const unfinished = (id: string) => ({
            ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'uncertain',
            phase: 'local-apply-unknown', applicationStarted: true, result: undefined,
        })
        mocks.bridge.startJob.mockImplementation(async (_request, id) => unfinished(id))
        await expect(requestExternalStorageRestore('old-sync', 'snapshot-1', ['library'])).rejects.toThrow()
        expect(recovery.hasPendingExternalApplication()).toBe(true)
        expect(mocks.releaseFence).not.toHaveBeenCalled()

        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...unfinished(id), state: 'failed', phase: 'paused', restoreStopped: true,
        }))
        await expect(recovery.retryExternalApplication()).rejects.toThrow()
        expect(recovery.hasPendingExternalApplication()).toBe(false)
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
        expect(mocks.refreshWorkingSet).not.toHaveBeenCalled()
        expect(mocks.persistentBackgroundError).not.toHaveBeenCalled()
    })

    it('retains the replacement fence through authoritative restore plugin reload and adoption', async () => {
        const {
            installExternalStorageProduction,
            requestExternalStorageRestore,
        } = await import('./production')
        await installExternalStorageProduction()
        mocks.revision = 23
        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...succeeded('old-sync', '23'), id,
            kind: 'restore',
            applicationStarted: true,
            result: { snapshotId: 'snapshot-1', receivedRevision: '24' },
        }))
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
        }), expect.any(String))
        expect(mocks.refreshWorkingSet).toHaveBeenCalledWith(24)
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.releaseFence).toHaveBeenCalledOnce()
        expect(events).toEqual(['plugins-reloaded', 'fence-released'])
    })

    it('reopens the store the native commit closed before projecting the restored library', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        await installExternalStorageProduction()
        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...succeeded('old-sync', '8'), id, kind: 'restore', applicationStarted: true,
            result: { snapshotId: 'snapshot-1', receivedRevision: '9' },
        }))
        const events: string[] = []
        mocks.openStore.mockImplementation(async () => { events.push('store-opened') })
        mocks.refreshWorkingSet.mockImplementation(async revision => {
            events.push('projected')
            return { kind: 'committed', revision, projection: 'applied' }
        })
        await expect(requestExternalStorageRestore('old-sync', 'snapshot-1', ['library'])).resolves.toMatchObject({ state: 'succeeded' })
        expect(events).toEqual(['store-opened', 'projected'])
        expect(mocks.bridge.confirmRestoreAdoption).toHaveBeenCalledWith(expect.any(String), '9', undefined)
    })

    it('keeps confirming a restore whose job read fails while its commit holds the store', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        const recovery = await import('./applicationRecovery')
        await installExternalStorageProduction()
        vi.useFakeTimers()
        try {
            const running = (id: string): ExternalJobSummary => ({
                ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'running', phase: 'applying-local',
                applicationStarted: true, result: undefined,
            })
            mocks.bridge.startJob.mockImplementation(async (_request, id) => running(id))
            mocks.bridge.getJob
                .mockRejectedValueOnce({ kind: 'localFailure', httpStatus: null, retryAtMs: null })
                .mockImplementationOnce(async id => ({
                    ...running(id), phase: 'awaiting-adoption', result: { snapshotId: 'snapshot-1', receivedRevision: '9' },
                }))
            const operation = requestExternalStorageRestore('old-sync', 'snapshot-1', ['library'])
            await vi.advanceTimersByTimeAsync(1_100)
            await expect(operation).resolves.toMatchObject({ result: { receivedRevision: '9' } })
            expect(mocks.bridge.getJob).toHaveBeenCalledTimes(2)
            expect(mocks.openStore).toHaveBeenCalledOnce()
            expect(mocks.refreshWorkingSet).toHaveBeenCalledWith(9)
            expect(mocks.releaseFence).toHaveBeenCalledOnce()
            expect(recovery.hasPendingExternalApplication()).toBe(false)
        } finally { vi.useRealTimers() }
    })

    it('stops confirming a restore whose job stays unreadable or fails for another reason', async () => {
        const { installExternalStorageProduction, requestExternalStorageRestore } = await import('./production')
        const recovery = await import('./applicationRecovery')
        await installExternalStorageProduction()
        vi.useFakeTimers()
        try {
            const transient = { kind: 'transient', httpStatus: null, retryAtMs: null }
            mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
                ...succeeded('old-sync', '8'), id, kind: 'restore', state: 'running', phase: 'downloading',
            }))
            mocks.bridge.getJob.mockRejectedValue(transient)
            const unreadable = expect(requestExternalStorageRestore('old-sync', 'snapshot-1', ['library'])).rejects.toEqual(transient)
            await vi.advanceTimersByTimeAsync(6_000)
            await unreadable
            expect(mocks.bridge.getJob).toHaveBeenCalledTimes(11)
            expect(recovery.hasPendingExternalApplication()).toBe(true)
            const corrupt = { kind: 'corrupt', httpStatus: null, retryAtMs: null }
            mocks.bridge.getJob.mockReset().mockRejectedValue(corrupt)
            const refused = expect(recovery.retryExternalApplication()).rejects.toEqual(corrupt)
            await vi.advanceTimersByTimeAsync(600)
            await refused
            expect(mocks.bridge.getJob).toHaveBeenCalledOnce()
            expect(mocks.openStore).not.toHaveBeenCalled()
            expect(mocks.releaseFence).not.toHaveBeenCalled()
        } finally { vi.useRealTimers() }
    })

    it('continues a committed restore once after read-only recovery without reactivation', async () => {
        const {
            installExternalStorageProduction,
            requestExternalStorageRestore,
        } = await import('./production')
        await installExternalStorageProduction()
        mocks.revision = 23
        mocks.bridge.startJob.mockImplementation(async (_request, id) => ({
            ...succeeded('old-sync', '23'), id,
            kind: 'restore',
            applicationStarted: true,
            result: { snapshotId: 'snapshot-1', receivedRevision: '24' },
        }))
        mocks.refreshWorkingSet.mockResolvedValueOnce({
            kind: 'committed', revision: 24, projection: 'refresh-required',
        })

        await expect(requestExternalStorageRestore(
            'old-sync',
            'snapshot-1',
            ['library'],
        )).rejects.toBeInstanceOf(Error)

        expect(mocks.releaseFence).not.toHaveBeenCalled()
        expect(mocks.reloadPlugins).not.toHaveBeenCalled()
        expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
        await (await import('./applicationRecovery')).retryExternalApplication()
        expect(mocks.refreshReleased).not.toHaveBeenCalled()
        expect(mocks.reloadPlugins).toHaveBeenCalledOnce()
        expect(mocks.bridge.startJob).toHaveBeenCalledOnce()
        expect(mocks.refreshWorkingSet).toHaveBeenCalledTimes(2)
    })

})
