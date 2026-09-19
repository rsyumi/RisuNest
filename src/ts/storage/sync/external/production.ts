import {
    acquireDestructiveReplacementFence,
    capturePersistentMutationToken,
    flushPendingDataLocally,
    refreshActiveWorkingSetFromStore,
} from '../../persistentDataRuntime.svelte'
import { subscribeLocalPersistentRevision } from '../../persistentRevisionEvents'
import type { SyncExitDrainAdapter } from '../../syncExitCoordinator'
import { hasPendingExternalApplication, runExternalApplication } from './applicationRecovery'
import {
    exportPortableBackupFromReferenceSource,
    restoreBackupFromNativeSource,
} from '../../portableBackupFileRouteProduction.svelte'
import { getExternalStorageBridge } from './bridge'
import {
    createExternalStorageController,
    type ExternalControllerResult,
    type ExternalExecutionSession,
    type ExternalStorageController,
} from './controller'
import {
    createExternalStorageScheduler,
    type ExternalScheduledDestination,
    type ExternalStorageScheduler,
} from './scheduler'
import type {
    DecimalString,
    ExternalJobSummary,
    ExternalHistoryItem,
    ExternalRestoreArea,
    ExternalStorageState,
} from './types'

interface ProductionRuntime {
    state: ExternalStorageState
    controller: ExternalStorageController
    scheduler: ExternalStorageScheduler
    session: ExternalExecutionSession
    lifecycle: Promise<void>
    dispose(): void
}

let runtime: ProductionRuntime | undefined
let installation: Promise<() => void> | undefined

function assertNoPendingApplication(): void {
    if (hasPendingExternalApplication()) {
        throw new Error('Confirm the pending external application before starting another operation')
    }
}

function newSession(kind: ExternalExecutionSession['kind']): ExternalExecutionSession {
    return { kind, id: crypto.randomUUID() }
}

function destinations(state: ExternalStorageState): ExternalScheduledDestination[] {
    const result: ExternalScheduledDestination[] = state.connections
        .filter(connection => connection.purpose === 'backup' && connection.status === 'ready')
        .map(connection => ({ connectionId: connection.id, kind: 'backup' as const }))
    const selected = state.selection
    if (selected.kind !== 'external' || selected.paused || selected.decisionRequired) return result
    const connection = state.connections.find(item =>
        item.id === selected.connectionId
        && item.purpose === 'sync'
        && item.status === 'ready')
    if (connection) result.push({ connectionId: connection.id, kind: 'sync' })
    return result
}

async function refreshForeground(current: ProductionRuntime): Promise<void> {
    const bridge = getExternalStorageBridge()
    current.session = newSession('foreground')
    await bridge.setExecutionSession(current.session)
    const state = await bridge.getState()
    current.state = state
    current.controller.replaceState(state)
    const token = await capturePersistentMutationToken('external-storage-foreground')
    current.scheduler.durableRevision(String(token.revision) as DecimalString)
    current.scheduler.resume()
}

function parseExternalLocalRevision(value: string | number): number {
    if (typeof value === 'string' && !/^(0|[1-9][0-9]*)$/.test(value)) {
        throw new RangeError('Invalid local revision')
    }
    const revision = typeof value === 'number' ? value : Number(value)
    if (!Number.isSafeInteger(revision) || revision < 0) {
        throw new RangeError('Local revision is outside the supported range')
    }
    return revision
}

async function applyReceivedSync(
    current: ProductionRuntime,
    job: ExternalJobSummary,
): Promise<void> {
    assertNoPendingApplication()
    if (
        job.result?.receiveReady !== true
        || job.result.expectedRevision === undefined
        || !/^(0|[1-9]\d*)$/.test(job.result.expectedRevision)
    ) {
        throw new Error('External sync receive readiness is incomplete')
    }
    await flushPendingDataLocally('external-storage-sync-receive')
    const token = await capturePersistentMutationToken(
        'external-storage-sync-receive', { publishOfficial: false },
    )
    if (String(token.revision) !== job.result.expectedRevision) {
        await getExternalStorageBridge().cancelJob(job.id)
        throw new Error('Local data changed while external sync receive was prepared')
    }
    const fence = await acquireDestructiveReplacementFence(token)
    await runExternalApplication({
        jobId: job.id,
        fence,
        confirm: async () => {
            try {
                const result = await getExternalStorageBridge().applyReceived(
                    job.id, String(fence.revision) as DecimalString,
                )
                return { kind: 'committed', revision: parseExternalLocalRevision(result.receivedRevision) }
            } catch (error) {
                const observed = await getExternalStorageBridge().getJob(job.id)
                if (observed.state === 'succeeded' && observed.result?.receivedRevision !== undefined) {
                    return { kind: 'committed', revision: parseExternalLocalRevision(observed.result.receivedRevision) }
                }
                throw error
            }
        },
        refreshReleased: refreshActiveWorkingSetFromStore,
        afterRefresh: async () => {
            await (await import('../../../plugins/plugins.svelte')).loadPluginsAfterAuthoritativeRestore()
            const state = await getExternalStorageBridge().getState()
            current.state = state
            current.controller.replaceState(state)
        },
        settled: () => current.scheduler.resume(),
    })
}

export async function requestExternalConflictRestore(
    conflictId: string,
    side: 'local' | 'remote',
): Promise<void> {
    const bridge = getExternalStorageBridge()
    let token: string | undefined
    try {
        await restoreBackupFromNativeSource(async () => {
            const result = await bridge.openConflictSource(conflictId, side)
            token = result.source.token
            return result.source
        })
    } finally {
        if (token) await bridge.releaseConflictSource(token)
    }
}

export async function requestExternalConflictExport(
    conflictId: string,
    side: 'local' | 'remote',
): Promise<void> {
    const bridge = getExternalStorageBridge()
    let token: string | undefined
    try {
        await exportPortableBackupFromReferenceSource(async () => {
            const result = await bridge.openConflictSource(conflictId, side)
            token = result.source.token
            return result.source
        })
    } finally {
        if (token) await bridge.releaseConflictSource(token)
    }
}

/** Installs one lightweight native state read and durable-revision scheduling. */
export function installExternalStorageProduction(): Promise<() => void> {
    if (installation) return installation
    installation = (async () => {
        const bridge = getExternalStorageBridge()
        if (!bridge.supported) return () => {}
        const session = newSession('foreground')
        await bridge.setExecutionSession(session)
        const state = await bridge.getState()
        const holder: { current?: ProductionRuntime } = {}
        const controller = createExternalStorageController(bridge, state, {
            applyReceived: async job => {
                const current = holder.current
                if (!current) throw new Error('External storage production is not installed')
                await applyReceivedSync(current, job)
            },
        })
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => !hasPendingExternalApplication()
                && document.visibilityState !== 'hidden' && navigator.onLine,
            destinations: () => destinations(holder.current?.state ?? state),
            session: () => holder.current?.session ?? session,
            maintenance: () => {
                const current = holder.current?.state ?? state
                return destinations(current).filter(destination => {
                    const capabilities = current.connections.find(item => item.id === destination.connectionId)?.capabilities
                    return capabilities?.snapshotDiscovery && capabilities.leaseOperations && capabilities.deleteObjects
                }).map(destination => ({ connectionId: destination.connectionId,
                    lastAttemptAt: Math.max(0, ...current.jobs.filter(job =>
                        job.connectionId === destination.connectionId && job.kind === 'cleanup',
                    ).map(job => Number(job.startedAtMs))) || undefined,
                }))
            },
        })
        const disposers: Array<() => void> = []
        const current: ProductionRuntime = {
            state,
            controller,
            scheduler,
            session,
            lifecycle: Promise.resolve(),
            dispose: () => {
                scheduler.stop()
                for (const dispose of disposers.splice(0)) dispose()
                if (runtime === current) runtime = undefined
                installation = undefined
            },
        }
        holder.current = current
        runtime = current
        disposers.push(subscribeLocalPersistentRevision((value, cause) => {
            scheduler.durableRevision(String(value) as DecimalString, cause)
        }))

        const onOnline = (): void => {
            current.lifecycle = current.lifecycle.then(async () => {
                if (runtime === current && document.visibilityState !== 'hidden') {
                    await refreshForeground(current)
                }
            }).catch(() => {})
        }
        const onOffline = (): void => {
            void scheduler.suspend(destination => destination.kind === 'sync')
        }
        const onVisibility = (): void => {
            const hidden = document.visibilityState === 'hidden'
            current.lifecycle = current.lifecycle.then(async () => {
                if (hidden) {
                    current.session = { kind: 'foreground', id: crypto.randomUUID() }
                    await bridge.setExecutionSession({ kind: 'hidden', id: current.session.id })
                    await scheduler.suspend(destination => {
                        if (destination.kind !== 'sync') return false
                        return current.state.connections.find(item =>
                            item.id === destination.connectionId)?.strategy === 'sequential'
                    })
                } else await refreshForeground(current)
            }).catch(() => {})
        }
        window.addEventListener('online', onOnline)
        window.addEventListener('offline', onOffline)
        document.addEventListener('visibilitychange', onVisibility)
        disposers.push(() => window.removeEventListener('online', onOnline))
        disposers.push(() => window.removeEventListener('offline', onOffline))
        disposers.push(() => document.removeEventListener('visibilitychange', onVisibility))

        const token = await capturePersistentMutationToken('external-storage-startup')
        scheduler.durableRevision(String(token.revision) as DecimalString)
        return current.dispose
    })().catch(error => {
        installation = undefined
        throw error
    })
    return installation
}

/** Gives answer-completion saves the five-second synchronization policy. */
export function notifyExternalStorageGenerationComplete(revision: number): void {
    try {
        revision = parseExternalLocalRevision(revision)
    } catch {
        return
    }
    runtime?.scheduler.durableRevision(String(revision) as DecimalString, 'generation-complete')
}

/** Refreshes scheduler routing after an explicit settings mutation. */
export async function refreshExternalStorageProductionState(): Promise<void> {
    const current = runtime
    if (!current) return
    const state = await getExternalStorageBridge().getState()
    current.state = state
    current.controller.replaceState(state)
    const token = await capturePersistentMutationToken('external-storage-routing-changed')
    if (runtime !== current) return
    current.scheduler.durableRevision(String(token.revision) as DecimalString)
    current.scheduler.resume()
}

/** Flushes local state, captures its durable revision, then waits for that goal. */
export async function requestExternalStorageNow(
    connectionId: string,
    kind: 'sync' | 'backup' | 'cleanup',
): Promise<ExternalControllerResult> {
    assertNoPendingApplication()
    if (kind !== 'cleanup') await flushPendingDataLocally(kind === 'sync' ? 'external-sync-now' : 'external-backup-now')
    const token = await capturePersistentMutationToken('external-storage-manual')
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    return current.scheduler.requestNow(
        connectionId,
        kind,
        String(token.revision) as DecimalString,
    )
}

export async function requestExternalStorageDeleteHistory(
    connectionId: string,
    item: ExternalHistoryItem,
    confirmOtherDevice: boolean,
    confirmLastRetained: boolean,
): Promise<ExternalJobSummary> {
    assertNoPendingApplication()
    if (!item.pointId || !item.pointObservation || !item.deletable) {
        throw new Error('The selected history item cannot be deleted')
    }
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    let job = await getExternalStorageBridge().startJob({
        connectionId,
        kind: 'delete-history',
        pointId: item.pointId,
        pointObservation: item.pointObservation,
        confirmOtherDevice,
        confirmLastRetained,
        reason: 'manual',
        session: current.session.kind,
        sessionId: current.session.id,
    })
    while (true) {
        if (job.state === 'succeeded') break
        if (['failed', 'cancelled', 'uncertain', 'conflict', 'waiting'].includes(job.state)) {
            throw restoreFailure(job)
        }
        await new Promise(resolve => setTimeout(resolve, 500))
        job = await getExternalStorageBridge().getJob(job.id)
    }
    const state = await getExternalStorageBridge().getState()
    current.state = state
    current.controller.replaceState(state)
    return job
}

/**
 * Runs a conflict decision to its end. The native side answers a received
 * repository side with `remote-apply`, which only this layer can activate, so
 * starting the job through the bridge alone would leave it waiting.
 */
export async function requestExternalStorageResolveConflict(
    connectionId: string,
    conflictId: string,
    choice: 'local' | 'remote',
): Promise<ExternalJobSummary> {
    assertNoPendingApplication()
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    await flushPendingDataLocally('external-storage-resolve-conflict')
    await current.scheduler.suspend(destination =>
        destination.kind === 'sync' && destination.connectionId === connectionId)
    try {
        let job = await getExternalStorageBridge().startJob({
            connectionId,
            kind: 'resolve-conflict',
            conflictId,
            choice,
            reason: 'manual',
            session: current.session.kind,
            sessionId: current.session.id,
        })
        while (true) {
            if (job.state === 'waiting' && job.phase === 'remote-apply') {
                await applyReceivedSync(current, job)
                job = await getExternalStorageBridge().getJob(job.id)
                continue
            }
            if (job.state === 'succeeded') break
            if (['failed', 'cancelled', 'uncertain', 'conflict'].includes(job.state)) {
                throw restoreFailure(job)
            }
            await new Promise(resolve => setTimeout(resolve, job.state === 'waiting' ? 5_000 : 500))
            job = await getExternalStorageBridge().getJob(job.id)
        }
        const state = await getExternalStorageBridge().getState()
        current.state = state
        current.controller.replaceState(state)
        return job
    } finally {
        current.scheduler.resume()
    }
}

function restoreFailure(job: ExternalJobSummary): Error {
    const error = new Error(job.error?.message ?? 'External storage restore failed')
    error.name = job.error?.code ?? 'ExternalStorageRestoreError'
    return error
}

export async function requestExternalStorageRestore(
    connectionId: string,
    snapshotId: string,
    restoreAreas: ExternalRestoreArea[],
): Promise<ExternalJobSummary> {
    assertNoPendingApplication()
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    let fence: Awaited<ReturnType<typeof acquireDestructiveReplacementFence>> | undefined
    let handedOff = false
    await current.scheduler.suspend(destination => destination.kind === 'sync')
    try {
        await flushPendingDataLocally('external-storage-restore')
        const token = await capturePersistentMutationToken(
            'external-storage-restore', { publishOfficial: false },
        )
        const areas = [...restoreAreas]
        const nativeState = await getExternalStorageBridge().getState()
        const previous = nativeState.jobs.find(job => job.kind === 'restore'
            && job.connectionId === connectionId
            && !['succeeded', 'failed', 'cancelled'].includes(job.state))
        const targetRevision = String(token.revision) as DecimalString
        if (previous && (previous.restoreRequest?.snapshotId !== snapshotId
            || previous.restoreRequest.targetRevision !== targetRevision
            || JSON.stringify(previous.restoreRequest.restoreAreas) !== JSON.stringify(areas))) {
            throw new Error('The pending restore belongs to a different snapshot, scope, or local revision')
        }
        fence = await acquireDestructiveReplacementFence(token)
        const jobId = previous?.id ?? crypto.randomUUID()
        let completed: ExternalJobSummary | undefined
        const operation = runExternalApplication({
            jobId,
            fence,
            confirm: async () => {
                // Retrying this ID confirms or resumes the same native job, even
                // when the original start response was lost after it committed.
                let job = await getExternalStorageBridge().startJob({
                    connectionId, kind: 'restore', snapshotId, restoreAreas: areas,
                    targetRevision, reason: 'manual',
                    session: current.session.kind, sessionId: current.session.id,
                }, jobId)
                while (true) {
                    if (job.id !== jobId) throw new Error('External restore job identity changed')
                    if (job.state === 'succeeded') {
                        const received = job.result?.receivedRevision
                        if (received === undefined) throw new Error('External restore has no commit receipt')
                        completed = job
                        return { kind: 'committed', revision: parseExternalLocalRevision(received) }
                    }
                    if (['failed', 'cancelled'].includes(job.state) && job.applicationStarted !== true) {
                        return { kind: 'not-applied', error: restoreFailure(job) }
                    }
                    if (['failed', 'cancelled', 'uncertain', 'conflict'].includes(job.state)) {
                        throw restoreFailure(job)
                    }
                    await new Promise(resolve => setTimeout(resolve, job.state === 'waiting' ? 5_000 : 500))
                    job = await getExternalStorageBridge().getJob(jobId)
                }
            },
            refreshReleased: refreshActiveWorkingSetFromStore,
            afterRefresh: async () => {
                await (await import('../../../plugins/plugins.svelte')).loadPluginsAfterAuthoritativeRestore()
                const state = await getExternalStorageBridge().getState()
                current.state = state
                current.controller.replaceState(state)
            },
            settled: () => current.scheduler.resume(),
        })
        handedOff = true
        await operation
        return completed!
    } finally {
        if (!handedOff) {
            fence?.release()
            current.scheduler.resume()
        }
    }
}

export interface ExternalExitSelectionCapture {
    selection: {
        kind: 'none' | 'server' | 'external'
        id?: string
        connectionId?: string
        selectionEpoch: string
        paused: boolean
        decisionRequired: boolean
    }
}

export function getExternalStorageSyncExitDrainAdapter(
    capture?: ExternalExitSelectionCapture,
): SyncExitDrainAdapter | null {
    const current = runtime
    if (!current) return null
    const selection = capture?.selection ?? current.state.selection
    const connectionId = 'id' in selection && typeof selection.id === 'string'
        ? selection.id
        : 'connectionId' in selection && typeof selection.connectionId === 'string'
            ? selection.connectionId
            : undefined
    if (selection.kind !== 'external' || !connectionId || selection.decisionRequired) return null
    const id = `external:${connectionId}:${selection.selectionEpoch}`
    let drainSession: ExternalExecutionSession | undefined
    return {
        id,
        async drain(target, signal) {
            if (hasPendingExternalApplication()) {
                return { kind: 'blocked', reason: 'external-application-confirmation-pending' }
            }
            if (target.selectionId !== id || target.selectionEpoch !== selection.selectionEpoch) {
                return { kind: 'blocked', reason: 'external-storage-selection-changed' }
            }
            const bridge = getExternalStorageBridge()
            drainSession = newSession('exitDrain')
            await current.lifecycle
            await bridge.setExecutionSession(drainSession)
            return current.controller.drainToRevision(
                connectionId,
                target,
                drainSession.id,
                signal,
            )
        },
        async cancel(reason) {
            try {
                await current.controller.cancel(connectionId)
            } finally {
                if (drainSession && reason === 'cancel-exit') await refreshForeground(current)
                drainSession = undefined
            }
        },
    }
}
