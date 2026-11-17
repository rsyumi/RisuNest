import { externalErrorKind, externalJobIsPaused } from './connection'
import { isTauriDesktop } from '../../../platform'
import { get } from 'svelte/store'
import { selectedCharID } from '../../../stores.svelte'
import { getDatabase } from '../../database.svelte'
import { flushDeviceStateBeforeRestore, refreshDeviceStateAfterRestore } from '../../deviceStateRestore'
import { beginMobileBackgroundTask, hasMobileBackgroundTasks, subscribeMobileBackgroundTasks, runWithMobileBackgroundTask, measuredTaskPercent, type MobileBackgroundTask } from '../../../mobileBackgroundTask'
import {
    acquireDestructiveReplacementFence,
    capturePersistentMutationToken,
    flushPendingDataLocally,
    getPersistentDataRuntime,
    refreshActiveWorkingSetFromStore,
} from '../../persistentDataRuntime.svelte'
import { PersistentMutationFencedError } from '../../saveCoordinator'
import type { PersistentDestructiveReplacementFence } from '../../persistentDataRuntime'
import type { RetainableReplacementFence } from '../../retainableReplacementFence'
import { subscribeLocalPersistentRevision } from '../../persistentRevisionEvents'
import { isLibraryFileOperationReserved, registerLibraryFileOperationGate, subscribeLibraryFileOperationReleased } from '../../libraryFileOperation'
import type { SyncExitDrainAdapter } from '../../syncExitCoordinator'
import { hasPendingExternalApplication, runExternalApplication } from './applicationRecovery'
import { getExternalStorageBridge } from './bridge'
import { installExternalLwwAdapters, refreshExternalLwwAdapters, externalLwwExitDrain } from './lwwProduction'
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
    StartExternalJobRequest,
} from './types'

interface ProductionRuntime {
    state: ExternalStorageState
    controller: ExternalStorageController
    scheduler: ExternalStorageScheduler
    session: ExternalExecutionSession
    /** Native holds the hidden session, which ended the jobs of the previous one. */
    hidden?: boolean
    lifecycle: Promise<void>
    dispose(): void
}

let runtime: ProductionRuntime | undefined
let installation: Promise<() => void> | undefined
let transportsOnly: Promise<() => void> | undefined
// A receive during an exit drain applies under the fence the exit holds.

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
        .filter(connection => connection.purpose === 'backup' && !connection.automaticBackupPaused && connection.status === 'ready')
        .map(connection => ({ connectionId: connection.id, kind: 'backup' as const }))
    return result
}

function recoverVerifiedConnections(current: ProductionRuntime, state: ExternalStorageState): void {
    for (const connection of state.connections) {
        const previous = current.state.connections.find(item => item.id === connection.id)
        if (connection.lastVerifiedAtMs !== undefined
            && connection.lastVerifiedAtMs !== previous?.lastVerifiedAtMs) {
            current.scheduler.recovered(connection.id)
        }
    }
    // A removed connection can no longer be signed in, so the backups that waited on it run again.
    for (const previous of current.state.connections) {
        if (!state.connections.some(connection => connection.id === previous.id)) current.scheduler.recovered(previous.id)
    }
}

async function refreshForeground(current: ProductionRuntime): Promise<void> {
    if (isLibraryFileOperationReserved()) return
    const bridge = getExternalStorageBridge()
    // Routine work that began on return to the screen must not keep the hidden session.
    if (current.hidden || !hasMobileBackgroundTasks()) {
        current.session = newSession('foreground')
        await bridge.setExecutionSession(current.session)
        current.hidden = false
    }
    const state = await bridge.getState()
    recoverVerifiedConnections(current, state)
    current.state = state
    current.controller.replaceState(state)
    await refreshExternalLwwAdapters(state)
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
        const controller = createExternalStorageController(bridge, state)
        const scheduler = createExternalStorageScheduler(controller, {
            available: () => !hasPendingExternalApplication() && !isLibraryFileOperationReserved()
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
        disposers.push(registerLibraryFileOperationGate(() => controller.settled()))
        disposers.push(await installExternalLwwAdapters(state))
        disposers.push(controller.subscribe(snapshot => {
            for (const [connectionId, job] of snapshot.activeJobs) {
                if (job.error?.action !== 'reauthenticate' && job.error?.action !== 'unlock-key') continue
                const fixedOn = job.error.connectionId || connectionId
                current.state = { ...current.state, connections: current.state.connections.map(connection =>
                    connection.id === fixedOn ? { ...connection, status: job.error?.action === 'reauthenticate' ? 'reauth-required' as const : 'key-locked' as const, lastError: job.error } : connection) }
            }
        }))
        disposers.push(subscribeLocalPersistentRevision(value => {
            scheduler.durableRevision(String(value) as DecimalString)
        }))

        const onOnline = (): void => {
            current.lifecycle = current.lifecycle.then(async () => {
                if (runtime === current && document.visibilityState !== 'hidden') {
                    await refreshForeground(current)
                }
            }).catch(() => {})
        }
        const onOffline = (): void => {
            void scheduler.suspend(() => false)
        }
        const onVisibility = (): void => {
            if (isLibraryFileOperationReserved()) return
            if (isTauriDesktop) {
                if (document.visibilityState !== 'hidden') current.scheduler.resume()
                return
            }
            const hidden = document.visibilityState === 'hidden'
            current.lifecycle = current.lifecycle.then(async () => {
                if (runtime !== current || isLibraryFileOperationReserved()) return
                if (hidden) {
                    if (hasMobileBackgroundTasks()) return
                    current.session = { kind: 'foreground', id: crypto.randomUUID() }
                    await bridge.setExecutionSession({ kind: 'hidden', id: current.session.id })
                    current.hidden = true
                    await scheduler.suspend(() => false)
                } else await refreshForeground(current)
            }).catch(() => {})
        }
        disposers.push(subscribeMobileBackgroundTasks(() => {
            if (document.visibilityState === 'hidden' && !hasMobileBackgroundTasks()) onVisibility()
        }))
        window.addEventListener('online', onOnline)
        window.addEventListener('offline', onOffline)
        document.addEventListener('visibilitychange', onVisibility)
        disposers.push(() => window.removeEventListener('online', onOnline))
        disposers.push(() => window.removeEventListener('offline', onOffline))
        disposers.push(() => document.removeEventListener('visibilitychange', onVisibility))
        disposers.push(subscribeLibraryFileOperationReleased(onVisibility))

        const token = await capturePersistentMutationToken('external-storage-startup')
        scheduler.durableRevision(String(token.revision) as DecimalString)
        return current.dispose
    })().catch(error => {
        installation = undefined
        throw error
    })
    return installation
}

/** Registers the sync connections' transports without starting sync or automatic backups. */
export function installExternalSyncTransports(): Promise<() => void> {
    if (installation) return installation
    if (transportsOnly) return transportsOnly
    transportsOnly = (async () => {
        const bridge = getExternalStorageBridge()
        if (!bridge.supported) return () => {}
        const dispose = await installExternalLwwAdapters(await bridge.getState(), false)
        return () => { dispose(); transportsOnly = undefined }
    })().catch(error => {
        transportsOnly = undefined
        throw error
    })
    return transportsOnly
}

/** Refreshes scheduler routing after an explicit settings mutation. */
export async function refreshExternalStorageProductionState(): Promise<void> {
    const current = runtime
    if (!current) {
        if (transportsOnly && await transportsOnly.then(() => true, () => false)) {
            await refreshExternalLwwAdapters(await getExternalStorageBridge().getState())
        }
        return
    }
    const state = await getExternalStorageBridge().getState()
    recoverVerifiedConnections(current, state)
    current.state = state
    current.controller.replaceState(state)
    await refreshExternalLwwAdapters(state)
    const token = await capturePersistentMutationToken('external-storage-routing-changed')
    if (runtime !== current) return
    current.scheduler.durableRevision(String(token.revision) as DecimalString)
    current.scheduler.resume()
}

/** Flushes local state, captures its durable revision, then waits for that goal. */
export async function requestExternalStorageNow(
    connectionId: string,
    kind: 'backup' | 'cleanup',
): Promise<ExternalControllerResult> {
    return runWithMobileBackgroundTask(kind === 'cleanup' ? 'maintenance' : kind, async background => {
        assertNoPendingApplication()
        if (kind !== 'cleanup') await flushPendingDataLocally('external-backup-now')
        const token = await capturePersistentMutationToken('external-storage-manual')
        const current = runtime
        if (!current) throw new Error('External storage production is not installed')
        background.signal?.throwIfAborted()
        const result = await current.scheduler.requestNow(
            connectionId,
            kind,
            String(token.revision) as DecimalString,
            background,
        )
        if (result.kind !== 'complete') await background.dispose(false)
        return result
    }, undefined, true)
}

export async function resumeExternalStorageJob(job: ExternalJobSummary): Promise<ExternalControllerResult> {
    if (job.kind === 'pin-history' || job.kind === 'delete-history' || job.kind === 'check-repository') {
        const details = job.kind === 'pin-history' ? job.pinRequest
            : job.kind === 'delete-history' ? job.deleteRequest : job.checkRequest
        if (!details) throw new Error('The pending operation has no admission request')
        const current = runtime
        if (!current) throw new Error('External storage production is not installed')
        return runWithMobileBackgroundTask<ExternalControllerResult>('maintenance', async background => {
            assertNoPendingApplication()
            let resumed = await getExternalStorageBridge().startJob({
                connectionId: job.connectionId, kind: job.kind, ...details,
                reason: job.reason ?? 'manual', session: current.session.kind, sessionId: current.session.id,
            }, job.id)
            while (['queued', 'running', 'waiting'].includes(resumed.state) && !externalJobIsPaused(resumed)) {
                await new Promise(resolve => setTimeout(resolve, 500))
                resumed = await readBackgroundJob(job.id, background)
                background.progress(measuredTaskPercent(Number(resumed.completedBytes), Number(resumed.totalBytes)))
            }
            if (resumed.state === 'succeeded') return { kind: 'complete', revision: '0', job: resumed }
            await background.dispose(false)
            return { kind: 'blocked', reason: resumed.error?.reason ?? resumed.error?.code ?? resumed.state,
                error: resumed.error, job: resumed }
        }, undefined, true)
    }
    if (!['backup', 'cleanup'].includes(job.kind)) throw new Error('Unsupported resume operation')
    if (!job.reason || (job.kind !== 'cleanup' && job.targetRevision === undefined)) {
        throw new Error('The pending operation has no admission request')
    }
    const current = runtime
    if (!current) throw new Error('External storage production is not installed')
    return runWithMobileBackgroundTask(job.kind === 'cleanup' ? 'maintenance' : job.kind as 'backup', async background => {
        assertNoPendingApplication()
        const result = await current.controller.request({
            connectionId: job.connectionId, kind: job.kind as 'backup' | 'cleanup',
            targetRevision: job.targetRevision ?? '0', reason: job.state === 'uncertain' ? 'manual' : job.reason!,
            session: current.session, backgroundTask: background, jobId: job.id,
        })
        if (result.kind !== 'complete') await background.dispose(false)
        return result
    }, undefined, true)
}

export async function requestExternalStorageDeleteHistory(
    connectionId: string,
    item: ExternalHistoryItem,
    confirmOtherDevice: boolean,
    confirmLastRetained: boolean,
): Promise<ExternalJobSummary> {
    return runWithMobileBackgroundTask('maintenance', async (background) => {
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
            job = await readBackgroundJob(job.id, background)
            background.progress(measuredTaskPercent(Number(job.completedBytes), Number(job.totalBytes)))
        }
        const state = await getExternalStorageBridge().getState()
        current.state = state
        current.controller.replaceState(state)
        return job
    }, undefined, true)
}

/** Starts a keep or check job and holds background time until it settles. */
export async function startExternalStorageMaintenanceJob(request: StartExternalJobRequest): Promise<ExternalJobSummary> {
    const background = await beginMobileBackgroundTask('maintenance', undefined, true)
    let job: ExternalJobSummary
    try { job = await getExternalStorageBridge().startJob(request) }
    catch (error) { await background.dispose(); throw error }
    if (background.protection?.state === 'refused') { await background.dispose(); return job }
    void (async () => {
        try {
            while (['queued', 'running', 'waiting'].includes(job.state) && !externalJobIsPaused(job)) {
                await new Promise(resolve => setTimeout(resolve, 500))
                job = await readBackgroundJob(job.id, background)
                background.progress(measuredTaskPercent(Number(job.completedBytes), Number(job.totalBytes)))
            }
        } catch {} finally { await background.dispose(job.state === 'succeeded') }
    })()
    return job
}

async function retryPausedJob(
    job: ExternalJobSummary,
    request: StartExternalJobRequest,
    attempts: number,
    background: MobileBackgroundTask,
): Promise<ExternalJobSummary | undefined> {
    if (attempts >= 3 || background.signal?.aborted
        || !['retry', 'wait'].includes(job.error?.action ?? '')) return undefined
    const resumeAt = Math.max(Date.now() + 5_000, Number(job.error?.retryAtMs ?? 0))
    while (Date.now() < resumeAt && !background.signal?.aborted) {
        await new Promise<void>(resolve => {
            const abort = () => { clearTimeout(timer); resolve() }
            const timer = setTimeout(() => {
                background.signal?.removeEventListener('abort', abort)
                resolve()
            }, Math.min(2_147_483_647, resumeAt - Date.now()))
            background.signal?.addEventListener('abort', abort, { once: true })
        })
    }
    if (background.signal?.aborted) return undefined
    return getExternalStorageBridge().startJob(request, job.id)
}

async function readBackgroundJob(id: string, background: MobileBackgroundTask): Promise<ExternalJobSummary> {
    const bridge = getExternalStorageBridge()
    return background.signal?.aborted ? bridge.cancelJob(id) : bridge.getJob(id)
}

/** Ends a restore whose local application could not be confirmed and keeps the library as it is. */
export async function stopExternalStorageRestore(jobId: string): Promise<ExternalJobSummary> {
    const stopped = await getExternalStorageBridge().stopRestore(jobId)
    const current = runtime
    if (current) {
        const state = await getExternalStorageBridge().getState()
        current.state = state
        current.controller.replaceState(state)
    }
    return stopped
}

function restoreFailure(job: ExternalJobSummary): Error {
    const error = new Error(job.error?.message ?? 'External storage restore failed')
    error.name = job.error?.code ?? 'ExternalStorageRestoreError'
    Object.assign(error, { code: job.error?.code })
    return error
}

export async function requestExternalStorageRestore(
    connectionId: string,
    snapshotId: string,
    restoreAreas: ExternalRestoreArea[],
): Promise<ExternalJobSummary> {
    return runWithMobileBackgroundTask('restore', async (background) => {
        assertNoPendingApplication()
        const current = runtime
        if (!current) throw new Error('External storage production is not installed')
        let fence: Awaited<ReturnType<typeof acquireDestructiveReplacementFence>> | undefined
        let replacement: Awaited<ReturnType<typeof import('../bindingRegistry')['prepareBoundLibraryReplacement']>> | undefined
        let pause: Awaited<ReturnType<typeof import('../../upstreamReplacement')['acquireUpstreamImportPause']>> | undefined
        let committed = false
        let handedOff = false
        let replacementFenced = false
        let pluginsFenced = false
        await current.scheduler.suspend(() => false)
        try {
            await flushPendingDataLocally('external-storage-restore')
            const areas = [...restoreAreas]
            const nativeState = await getExternalStorageBridge().getState()
            const observed = nativeState.jobs.find(job => job.kind === 'restore'
                && job.connectionId === connectionId
                && !['succeeded', 'failed', 'cancelled'].includes(job.state))
            const previous = observed ? await getExternalStorageBridge().getJob(observed.id) : undefined
            replacement = await (await import('../bindingRegistry')).prepareBoundLibraryReplacement({
                confirmedCommittedRestore: previous?.applicationStarted === true && previous.result?.receivedRevision !== undefined,
            })
            replacementFenced = true
            await replacement.fence()
            pluginsFenced = true
            await (await import('../../../plugins/apiV3/v3.svelte')).fencePluginExecutionForAuthorityReplacement()
            pause = await (await import('../../upstreamReplacement')).acquireUpstreamImportPause(getPersistentDataRuntime(), 'external-storage-restore')
            fence = pause.fence
            const token = pause.token
            const targetRevision = previous?.applicationStarted === true
                ? previous.restoreRequest?.targetRevision ?? String(token.revision) as DecimalString
                : String(token.revision) as DecimalString
            if (previous && (previous.restoreRequest?.snapshotId !== snapshotId
                || previous.restoreRequest.targetRevision !== targetRevision
                || JSON.stringify(previous.restoreRequest.restoreAreas) !== JSON.stringify(areas))) {
                throw Object.assign(new Error('The pending restore belongs to a different snapshot, scope, or local revision'), { code: 'preconditionFailed' })
            }
            await flushDeviceStateBeforeRestore()
            await replacement.assertAuthority()
            const jobId = previous?.id ?? crypto.randomUUID()
            let completed: ExternalJobSummary | undefined
            let adoptionSelection: string | undefined
            let adoptionSelected = false
            let pluginsAdopted = false
            let adoptionComplete = false
            let unchangedReleased = false
            const operation = runExternalApplication({
                jobId,
                fence,
                refreshDeviceState: refreshDeviceStateAfterRestore,
                confirm: async () => {
                    // Retrying this ID confirms or resumes the same native job, even
                    // when the original start response was lost after it committed.
                    const request: StartExternalJobRequest = {
                        connectionId, kind: 'restore', snapshotId, restoreAreas: areas,
                        targetRevision, reason: 'manual',
                        session: current.session.kind, sessionId: current.session.id,
                    }
                    let job = await getExternalStorageBridge().startJob(request, jobId)
                    let retries = 0
                    let unreadable = 0
                    let stopping = false
                    while (true) {
                        if (job.id !== jobId) throw new Error('External restore job identity changed')
                        const received = job.result?.receivedRevision
                        if (received !== undefined && job.applicationStarted === true) {
                            committed = true
                            completed = job
                            return { kind: 'committed', revision: parseExternalLocalRevision(received) }
                        }
                        if (externalJobIsPaused(job) && !stopping) {
                            const resumed = await retryPausedJob(job, request, retries++, background)
                            if (resumed) { job = resumed; continue }
                            stopping = true
                            job = await getExternalStorageBridge().cancelJob(jobId, !background.signal?.aborted)
                            continue
                        }
                        if (job.state === 'succeeded') throw new Error('External restore has no commit receipt')
                        if (['failed', 'cancelled'].includes(job.state)
                            && (job.applicationStarted !== true || job.restoreStopped === true)) {
                            return { kind: 'not-applied', error: restoreFailure(job) }
                        }
                        if (['failed', 'cancelled', 'uncertain', 'conflict'].includes(job.state)) {
                            throw restoreFailure(job)
                        }
                        await new Promise(resolve => setTimeout(resolve, job.state === 'waiting' ? 5_000 : 500))
                        try {
                            job = await readBackgroundJob(jobId, background)
                            unreadable = 0
                        } catch (error) {
                            // The local commit briefly closes the store the job is read through.
                            if (!['transient', 'localFailure'].includes(externalErrorKind(error) ?? '') || ++unreadable > 10) throw error
                            continue
                        }
                        background.progress(measuredTaskPercent(Number(job.completedBytes), Number(job.totalBytes)))
                    }
                },
                // Opens the store if the native commit could not reopen it.
                reopenStore: () => getPersistentDataRuntime().store.open(),
                refreshReleased: refreshActiveWorkingSetFromStore,
                afterRefresh: async () => {
                    if (adoptionComplete) return
                    if (!pluginsAdopted) {
                        const plugins = await import('../../../plugins/apiV3/v3.svelte')
                        await plugins.invalidatePluginCachesAfterAuthorityReplacement()
                        await plugins.restartPluginsAfterAuthorityReplacement()
                        pluginsAdopted = true
                    }
                    if (!completed?.result?.receivedRevision) throw new Error('External restore has no commit receipt')
                    if (!adoptionSelected) {
                        adoptionSelection = getDatabase().characters[get(selectedCharID)]?.chaId
                        adoptionSelected = true
                    }
                    await getExternalStorageBridge().confirmRestoreAdoption(jobId, completed.result.receivedRevision,
                        adoptionSelection)
                    const state = await getExternalStorageBridge().getState()
                    current.state = state
                    current.controller.replaceState(state)
                    pause!.complete()
                    adoptionComplete = true
                },
                settled: async () => {
                    if (committed) await pause!.finish()
                    // A retry after a failed plugin restart finds the unchanged library already released.
                    else if (!unchangedReleased) {
                        await pause!.abortUnchanged(replacement!.assertAuthority)
                        unchangedReleased = true
                    }
                    // A failed plugin restart is still reported, after sync resumes.
                    try { if (!committed) await (await import('../../../plugins/apiV3/v3.svelte')).restartPluginsAfterAuthorityReplacement() }
                    finally {
                        await replacement!.resume()
                        current.scheduler.resume()
                    }
                },
            })
            handedOff = true
            await operation
            return completed!
        } finally {
            if (!handedOff) {
                try {
                    if (replacementFenced && replacement) {
                        if (pause) await pause.abortUnchanged(replacement.assertAuthority)
                        else await replacement.assertAuthority()
                        try {
                            if (pluginsFenced) await (await import('../../../plugins/apiV3/v3.svelte')).restartPluginsAfterAuthorityReplacement()
                        } finally { await replacement.resume() }
                    } else fence?.release()
                } finally {
                    replacement?.release()
                    current.scheduler.resume()
                }
            }
        }
    }, undefined, true)
}

export interface ExternalExitSelectionCapture {
    selection: {
        kind: 'none' | 'server' | 'external'
        id?: string
        connectionId?: string
        selectionEpoch: string
        paused: boolean
    }
}


export function getExternalStorageSyncExitDrainAdapter(
    capture?: ExternalExitSelectionCapture,
    fence?: RetainableReplacementFence,
): SyncExitDrainAdapter | null {
    const selection = capture?.selection ?? runtime?.state.selection
    if (selection?.kind !== 'external') return null
    const connectionId = selection.connectionId ?? ('id' in selection ? selection.id : undefined)
    return typeof connectionId === 'string' ? externalLwwExitDrain(connectionId, selection.selectionEpoch, !!fence) : null
}
