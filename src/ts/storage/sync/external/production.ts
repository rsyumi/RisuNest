import {
    acquireDestructiveReplacementFence,
    capturePersistentMutationToken,
    flushPendingDataLocally,
    getPersistentDataRuntime,
    getPersistentStorageAuthorityEpoch,
} from '../../persistentDataRuntime.svelte'
import { subscribeLocalPersistentRevision } from '../../persistentRevisionEvents'
import type { SyncExitDrainAdapter } from '../../syncExitCoordinator'
import { registerCommittedWorkingSetContinuation } from '../../committedWorkingSetContinuation'
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
    let fenceReleased = false
    const releaseFence = (): void => {
        if (fenceReleased) return
        fenceReleased = true
        fence.release()
    }
    try {
        const result = await getExternalStorageBridge().applyReceived(
            job.id,
            String(fence.revision) as DecimalString,
        )
        const receivedRevision = parseExternalLocalRevision(result.receivedRevision)
        const continueAfterRefresh = async (): Promise<void> => {
            await (
                await import('../../../plugins/plugins.svelte')
            ).loadPluginsAfterAuthoritativeRestore()
            const state = await getExternalStorageBridge().getState()
            current.state = state
            current.controller.replaceState(state)
        }
        try {
            const outcome = await fence.refreshCommittedWorkingSet(receivedRevision)
            releaseFence()
            if (outcome.projection === 'refresh-required') {
                registerCommittedWorkingSetContinuation(
                    receivedRevision,
                    getPersistentDataRuntime(),
                    getPersistentStorageAuthorityEpoch(),
                    continueAfterRefresh,
                )
                throw new Error('Committed external receive requires a read-only working-set refresh')
            }
            await continueAfterRefresh()
        } catch (error) {
            const { NativeFileJobActivationCommittedError } = await import('../../nativeFileJobs')
            throw new NativeFileJobActivationCommittedError(receivedRevision, error)
        }
    } finally {
        releaseFence()
    }
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
            available: () => document.visibilityState !== 'hidden' && navigator.onLine,
            destinations: () => destinations(holder.current?.state ?? state),
            session: () => holder.current?.session ?? session,
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

        const onOnline = (): void => scheduler.resume()
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
    current.scheduler.resume()
}

/** Flushes local state, captures its durable revision, then waits for that goal. */
export async function requestExternalStorageNow(
    connectionId: string,
    kind: 'sync' | 'backup',
): Promise<ExternalControllerResult> {
    await flushPendingDataLocally(kind === 'sync' ? 'external-sync-now' : 'external-backup-now')
    const token = await capturePersistentMutationToken('external-storage-manual')
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    return current.scheduler.requestNow(
        connectionId,
        kind,
        String(token.revision) as DecimalString,
    )
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
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    await flushPendingDataLocally('external-storage-restore')
    const token = await capturePersistentMutationToken(
        'external-storage-restore', { publishOfficial: false },
    )
    const fence = await acquireDestructiveReplacementFence(token)
    let fenceReleased = false
    const releaseFence = (): void => {
        if (fenceReleased) return
        fenceReleased = true
        fence.release()
    }
    await current.scheduler.suspend(destination => destination.kind === 'sync')
    try {
        let job = await getExternalStorageBridge().startJob({
            connectionId,
            kind: 'restore',
            snapshotId,
            restoreAreas,
            targetRevision: String(fence.revision) as DecimalString,
            reason: 'manual',
            session: current.session.kind,
            sessionId: current.session.id,
        })
        while (true) {
            if (job.state === 'succeeded') break
            if (['failed', 'cancelled', 'uncertain', 'conflict'].includes(job.state)) {
                throw restoreFailure(job)
            }
            await new Promise(resolve => setTimeout(resolve, job.state === 'waiting' ? 5_000 : 500))
            job = await getExternalStorageBridge().getJob(job.id)
        }
        const received = job.result?.receivedRevision
        if (received === undefined || !/^(0|[1-9]\d*)$/.test(received)) {
            throw new Error('External restore did not report an activated revision')
        }
        const receivedRevision = Number(received)
        if (!Number.isSafeInteger(receivedRevision)) {
            throw new RangeError('External restore revision exceeds the renderer range')
        }
        const continueAfterRefresh = async (): Promise<void> => {
            await (
                await import('../../../plugins/plugins.svelte')
            ).loadPluginsAfterAuthoritativeRestore()
            const state = await getExternalStorageBridge().getState()
            current.state = state
            current.controller.replaceState(state)
        }
        try {
            const outcome = await fence.refreshCommittedWorkingSet(receivedRevision)
            releaseFence()
            if (outcome.projection === 'refresh-required') {
                registerCommittedWorkingSetContinuation(
                    receivedRevision,
                    getPersistentDataRuntime(),
                    getPersistentStorageAuthorityEpoch(),
                    continueAfterRefresh,
                )
                throw new Error('Committed external restore requires a read-only working-set refresh')
            }
            await continueAfterRefresh()
        } catch (error) {
            const { NativeFileJobActivationCommittedError } = await import('../../nativeFileJobs')
            throw new NativeFileJobActivationCommittedError(receivedRevision, error)
        }
        return job
    } finally {
        releaseFence()
        current.scheduler.resume()
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
