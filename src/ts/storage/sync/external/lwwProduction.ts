import { joinAbortSignals } from '../../../abortSignals'
import { notifySyncReceiveCompleted } from '../bindingChanges'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { isTauri } from '../../../platform'
import { beginDeferredMobileBackgroundTask, runWithMobileBackgroundTask, type MobileBackgroundTask } from '../../../mobileBackgroundTask'
import { generatingConversations } from '../../generatingConversationRegistry'
import { flushPendingDataLocally, getPersistentDataRuntime } from '../../persistentDataRuntime.svelte'
import { subscribeLocalPersistentRevision } from '../../persistentRevisionEvents'
import type { LwwStageReceive } from '../../persistentDataStore'
import type { SyncExitDrainAdapter } from '../../syncExitCoordinator'
import type { BindingContext, SyncBindingState, SyncBindingTransport, InspectedSyncTarget, StagedSyncTarget, NewDeviceBindingPreparation } from '../bindingFlow'
import { createNativeSyncBindingBridge, replaceNativeSyncBinding, replaceNativeSyncBindingAsNewDevice } from '../bindingNative'
import { registerSyncBindingTransport, resumeCurrentSyncBinding } from '../bindingRegistry'
import { SERVER_SYNC_DEVICE_CHANGED_EVENT } from '../serverSyncNativeSignals'
import { downloadRemoteAssets } from '../serverAssetResidency'
import { isLibraryFileOperationReserved, registerLibraryFileOperationGate, subscribeLibraryFileOperationReleased } from '../../libraryFileOperation'
import { createLwwScheduler, failureKind, formatKinds, type LwwCall, type LwwOperation, type LwwTurn } from './lwwScheduler'
import type { ExternalStorageState } from './types'
import { EXTERNAL_ASSET_KEY_CODE, EXTERNAL_ASSET_REAUTH_CODE, externalRemedyConnection } from './connection'
import { beginExternalProgress, clearExternalProgress, clearExternalSyncProgress, externalProgressFailure, type ExternalProgressRun } from './progress'

interface Adapter { transport: SyncBindingTransport; scheduler: ReturnType<typeof createLwwScheduler>; recover(renewed: ReadonlySet<string>): boolean; stopHydration(): Promise<void>; resumeHydration(pendingOnly?: boolean, continuation?: boolean): void; hide(publish: boolean): Promise<void>; dispose(): void; state?: SyncBindingState; generation: number; foreground?: Promise<void>; recoveries?: ReadonlyMap<string, string> }
const adapters = new Map<string, Adapter>()
const failures = new Map<string, unknown>()
const failureListeners = new Set<(failures: ReadonlyMap<string, unknown>) => void>()
function reportFailure(id: string, error?: unknown): void {
    if (error === undefined) failures.delete(id)
    else failures.set(id, error)
    for (const listener of failureListeners) listener(new Map(failures))
}
export function subscribeExternalLwwFailures(listener: (failures: ReadonlyMap<string, unknown>) => void): () => void {
    failureListeners.add(listener); listener(new Map(failures))
    return () => { failureListeners.delete(listener) }
}
const catchingUp = new Set<string>()
const catchUpListeners = new Set<(ids: ReadonlySet<string>) => void>()
function reportCatchingUp(id: string, value: boolean): void {
    if (value === catchingUp.has(id)) return
    if (value) catchingUp.add(id)
    else catchingUp.delete(id)
    for (const listener of catchUpListeners) listener(new Set(catchingUp))
}
/** The connections whose last receive left changes that the next receive continues at once. */
export function subscribeExternalLwwCatchUp(listener: (ids: ReadonlySet<string>) => void): () => void {
    catchUpListeners.add(listener); listener(new Set(catchingUp))
    return () => { catchUpListeners.delete(listener) }
}
export function supportsExternalLwwNewDevice(id: string): boolean {
    const transport = adapters.get(id)?.transport
    return !!(transport?.prepareNewDeviceBinding && transport.replaceAsNewDevice && transport.resumeNewDeviceBinding)
}
const header = (connectionId: string, context: BindingContext, requestId: string = crypto.randomUUID()) => ({ connectionId, bindingAuthority: context.state.targetAuthority, requestId, generating: generatingConversations.snapshot() })
const native = createNativeSyncBindingBridge()
const networkAvailable = () => navigator.onLine && !isLibraryFileOperationReserved()
const foregroundAvailable = () => document.visibilityState !== 'hidden' && networkAvailable()
// A start that left sync off registers the transports, so settings can still change the sync target,
// but leaves the bound target stopped until the user starts it.
let resumeBoundTarget = true
let refreshGeneration = 0

const receivedBodyReferences = (request: LwwStageReceive) => request.changes.some(change => {
    const [kind] = JSON.parse(change.key) as string[]
    if (change.value.kind === 'inline') return kind === 'asset' || kind === 'inlay'
    if (change.value.kind !== 'object' || kind === 'messages') return false
    const descriptor = change.value.descriptor as { dependencies?: unknown[]; dependencyRoot?: unknown; relationRoot?: unknown }
    return !!descriptor.dependencies?.length || !!descriptor.dependencyRoot || !!descriptor.relationRoot
})

function createAdapter(connectionId: string): Adapter {
    let active: BindingContext | undefined
    let bindingProgress: ExternalProgressRun | undefined
    let hydrating: Promise<void> | undefined
    let hydrationController: AbortController | undefined
    let hydrationEnabled = false
    let hydrationPending = false
    let hydrationRetryAt = 0
    let hydrationServiceRetryAt = 0
    let hydrationBlocked = false
    const operationFailures = new Map<LwwOperation | 'assets', unknown>()
    const reportOperations = () => {
        const values = [...operationFailures.values()]
        // A format level stop stays reported with its switch while another operation fails.
        reportFailure(connectionId, values.find(value => formatKinds.includes(failureKind(value))) ?? values.at(-1))
    }
    const reportOperation = (operation: LwwOperation | 'assets', error?: unknown) => {
        if (error === undefined) operationFailures.delete(operation)
        else operationFailures.set(operation, error)
        reportOperations()
    }
    /** `continuation` lets hydration that received pages or a manual sync requested start or continue while hidden. */
    const continueHydration = (continuation = false) => {
        if (!hydrationEnabled || !hydrationPending || hydrating || hydrationBlocked || Date.now() < hydrationRetryAt
            || !active || active.signal.aborted || !(foregroundAvailable() || continuation && networkAvailable())) return
        const captured = active
        hydrationController = new AbortController()
        const joined = joinAbortSignals([captured.signal, hydrationController.signal])
        const signal = joined.signal
        hydrationPending = false
        // A connection-only download excludes bodies that multiple storage connections hold.
        hydrating = downloadRemoteAssets(undefined, { progressConnectionId: connectionId, signal, showEmpty: false, routine: true }).then(() => {
            if (active === captured && !signal.aborted) reportOperation('assets')
        }).catch(error => {
            if (active !== captured || signal.aborted) return
            hydrationPending = true
            const failure = error as { kind?: string; code?: string; retryAtMs?: string | number; retryable?: boolean; action?: string }
            const deadline = Number(failure?.retryAtMs)
            if (Number.isFinite(deadline)) hydrationServiceRetryAt = Math.max(hydrationServiceRetryAt, deadline)
            hydrationRetryAt = Math.max(Date.now() + 20_000, hydrationServiceRetryAt)
            hydrationBlocked = failure?.retryable === false || ['reauthenticate', 'unlock-key', 'free-space', 'check-endpoint'].includes(failure?.action ?? '')
                || ['unauthorized', 'reauthRequired', EXTERNAL_ASSET_REAUTH_CODE, EXTERNAL_ASSET_KEY_CODE, 'deviceVaultUnavailable', 'repositoryKeyUnavailable', 'localStorageFull', 'localPermissionDenied', 'storageFull', 'endpointRejected', 'corrupt', 'repositoryMismatch'].includes(failure?.kind ?? failure?.code ?? '')
            reportOperation('assets', error)
        }).finally(() => {
            joined.dispose()
            hydrating = undefined
            hydrationController = undefined
            continueHydration(true)
        })
    }
    const stopHydration = () => {
        hydrationEnabled = false
        hydrationController?.abort()
        return hydrating ?? Promise.resolve()
    }
    const resumeHydration = (pendingOnly = false, continuation = false) => {
        hydrationEnabled = true
        hydrationPending ||= !pendingOnly
        hydrationBlocked = false
        hydrationRetryAt = hydrationServiceRetryAt
        continueHydration(continuation)
    }
    // Routine sync holds the service only if the app leaves the screen while it runs.
    let hold: MobileBackgroundTask | Promise<MobileBackgroundTask> | undefined
    let hidePublication: AbortController | undefined
    const hide = (publish: boolean) => {
        const controller = new AbortController()
        hidePublication = controller
        return scheduler.background(publish, controller.signal).finally(() => { if (hidePublication === controller) hidePublication = undefined })
    }
    const holdWhileBusy = (busy: boolean) => {
        if (busy === !!hold) return
        if (!busy) {
            const released = hold
            hold = undefined
            void Promise.resolve(released).then(task => task?.dispose())
            return
        }
        const current = beginDeferredMobileBackgroundTask('sync')
        hold = current
        // Hydration and manual requests hold their own time; returning to the screen resumes routine sync.
        void Promise.resolve(current).then(task => task.signal?.addEventListener('abort', () => {
            if (hold !== current) return
            hidePublication?.abort(task.signal?.reason)
            scheduler.expire()
        }, { once: true }))
    }
    const withContext = async <T>(signal: AbortSignal | undefined, run: (binding: BindingContext) => Promise<T>): Promise<T> => {
        const state = await native.state()
        if (state.target.kind !== 'external' || state.target.connectionId !== connectionId) throw new Error('Sync binding changed')
        const joined = joinAbortSignals([signal, active?.signal])
        try { return await run({ state, signal: joined.signal }) } finally { joined.dispose() }
    }
    // Native code returns one bounded page per call and lists the next batch at once while segments remain;
    // with `completeAvailable` it also lists again after a finished listing, so later arrivals are received too.
    const receivePages = async (binding: BindingContext, turnLimit?: number, completeAvailable = false): Promise<LwwTurn> => {
        const progress = beginExternalProgress(connectionId, 'sync')
        let received = 0
        try {
            for (;;) {
                binding.signal.throwIfAborted()
                const request = await progress.invoke<LwwStageReceive | null>('external_lww_receive', { request: { ...header(connectionId, binding), ...(completeAvailable ? { completeAvailable } : {}) } }, binding.state.targetAuthority)
                binding.signal.throwIfAborted()
                if (!request) { progress.finish('complete', received > 0); notifySyncReceiveCompleted(); return { count: received, more: false } }
                binding.signal.throwIfAborted()
                progress.stage('applying')
                await getPersistentDataRuntime().applyLwwReceive(request)
                received++
                binding.signal.throwIfAborted()
                if (receivedBodyReferences(request)) hydrationPending = true
                if (turnLimit !== undefined && received >= turnLimit) { progress.finish('complete', true); return { count: received, more: true } }
            }
        } catch (error) { progress.finish(externalProgressFailure(error)); throw error }
    }
    // The plugin startup receive runs beside the scheduler, and native code hands every receive the first unfinished page.
    let receiving: Promise<unknown> = Promise.resolve()
    const receive = (binding: BindingContext, turnLimit?: number, completeAvailable = false): Promise<LwwTurn> => {
        const turn = receiving.then(() => receivePages(binding, turnLimit, completeAvailable))
        receiving = turn.catch(() => {})
        return turn
    }
    const publishTurn = async (current: BindingContext, bound: boolean, flush: boolean, call?: LwwCall): Promise<LwwTurn> => {
        current.signal.throwIfAborted()
        const progress = (bound && bindingProgress) || beginExternalProgress(connectionId, 'sync')
        const owned = progress !== bindingProgress
        let result: { segments: string; more: boolean } = { segments: '0', more: false }
        let meaningful = false
        try {
            progress.stage('preparing', current.state.targetAuthority)
            try {
                if (flush) await flushPendingDataLocally('external-lww-publish')
                current.signal.throwIfAborted()
                result = await progress.invoke<{ segments: string; more: boolean }>('external_lww_publish', { request: { ...header(connectionId, current), ...(call?.turnLimit !== undefined ? { turnLimit: call.turnLimit } : {}) } }, current.state.targetAuthority)
                meaningful = Number(result.segments) > 0
            } finally {
                progress.stage('finalizing')
                current.signal.throwIfAborted()
                const runtime = getPersistentDataRuntime()
                await runtime.runStorageOnlyMutation(async () => {
                    current.signal.throwIfAborted()
                    const page = await runtime.store.lwwReadOutbox({ bindingAuthority: current.state.targetAuthority, requestId: crypto.randomUUID(), limit: '1' })
                    return page.revision
                })
            }
            if (owned) progress.finish('complete', meaningful)
        } catch (error) { if (owned) progress.finish(externalProgressFailure(error)); throw error }
        return { count: Number(result.segments), more: result.more }
    }
    const publish = (binding?: BindingContext, flush = true, call?: LwwCall): Promise<LwwTurn> => binding
        ? publishTurn(binding, true, flush, call)
        : withContext(call?.signal, current => publishTurn(current, false, flush, call))
    const scheduler = createLwwScheduler({
        available: () => !!active && !active.signal.aborted && networkAvailable(),
        busy: holdWhileBusy,
        catchingUp: value => reportCatchingUp(connectionId, value),
        publish: call => publish(undefined, true, call),
        receive: call => withContext(call.signal, binding => receive(binding, call.turnLimit)),
        maintain: call => withContext(call.signal, async current => { current.signal.throwIfAborted(); await invoke('external_lww_maintenance', { request: header(connectionId, current) }); current.signal.throwIfAborted() }),
        checkFormat: call => withContext(call.signal, async current => { await invoke('external_lww_check_format', { request: header(connectionId, current) }); current.signal.throwIfAborted() }),
        succeeded: operation => { reportOperation(operation); if (operation === 'receive') continueHydration(true) },
        failed: (error, operation) => {
            reportOperation(operation, error)
            const kind = (error as { kind?: string })?.kind
            if (kind === 'corrupt' || kind === 'repositoryMismatch') { scheduler.stop(); void stopHydration() }
        },
    })
    const transport: SyncBindingTransport & { receiveAvailableChanges(context: BindingContext): Promise<void> } = {
        async observeBinding(operation) {
            if (bindingProgress) return operation()
            // Inspecting, staging and applying continue while the app is in the background.
            return runWithMobileBackgroundTask('sync', async () => {
                const progress = beginExternalProgress(connectionId, 'binding')
                bindingProgress = progress
                try {
                    const result = await operation()
                    progress.finish(result.kind === 'cancelled' ? 'cancelled' : 'complete')
                    return result
                } catch (error) { progress.finish(externalProgressFailure(error)); throw error }
                finally { if (bindingProgress === progress) bindingProgress = undefined }
            }, undefined, true)
        },
        setConfirmationPending(waiting) { bindingProgress?.stage(waiting ? 'waiting' : 'checking') },
        inspectTarget: binding => bindingProgress ? bindingProgress.invoke<InspectedSyncTarget>('external_lww_inspect', { request: header(connectionId, binding) }, binding.state.targetAuthority) : invoke<InspectedSyncTarget>('external_lww_inspect', { request: header(connectionId, binding) }),
        pullAvailableState: (target, binding) => {
            const args = { request: { ...header(connectionId, binding), inspectionId: target.inspectionId, targetId: target.targetId, libraryId: target.libraryId } }
            return bindingProgress ? bindingProgress.invoke<StagedSyncTarget>('external_lww_stage_binding', args, binding.state.targetAuthority) : invoke<StagedSyncTarget>('external_lww_stage_binding', args)
        },
        async replaceFromTarget(staged, binding) { bindingProgress?.stage('applying', binding.state.targetAuthority); await replaceNativeSyncBinding(staged, binding) },
        reportStopped(error) {
            const cause = error instanceof AggregateError ? error.errors[0] : error
            if ((cause as { name?: unknown } | null)?.name !== 'AbortError') reportFailure(connectionId, cause)
        },
        publishInitialSharedState: async binding => { await publish(binding, false) },
        async resumeBinding(binding) {
            const generation = ++adapter.generation
            scheduler.stop()
            await Promise.all([scheduler.settled(), stopHydration()])
            binding.signal.throwIfAborted()
            await invoke('pds_lww_finish_initial_publication', { request: { bindingAuthority: binding.state.targetAuthority, requestId: crypto.randomUUID() } })
            if (generation !== adapter.generation) throw new DOMException('Binding changed', 'AbortError')
            binding.signal.throwIfAborted()
            await invoke('external_lww_resume', { connectionId })
            binding.signal.throwIfAborted()
            if (generation !== adapter.generation) throw new DOMException('Binding changed', 'AbortError')
            operationFailures.clear()
            hydrationRetryAt = 0
            hydrationServiceRetryAt = 0
            reportFailure(connectionId)
            active = binding
            adapter.state = binding.state
            scheduler.recovered()
            if (foregroundAvailable()) { scheduler.start(); resumeHydration() }
        },
        async fenceOldJobs(binding) {
            adapter.generation++
            scheduler.stop()
            const hydration = stopHydration()
            active = undefined
            adapter.state = undefined
            const old = binding.state.target
            if (old.kind === 'external') {
                clearExternalSyncProgress(old.connectionId)
                const adapter = adapters.get(old.connectionId)
                adapter?.scheduler.stop()
                const oldHydration = adapter?.stopHydration()
                if (adapter) { adapter.generation++; adapter.state = undefined }
                await invoke('external_lww_fence', { connectionId: old.connectionId, newDevice: binding.mode === 'new-device' })
                await Promise.all([adapter?.scheduler.settled(), oldHydration])
            }
            await Promise.all([scheduler.settled(), hydration])
        },
        prepareNewDeviceBinding: (staged, binding) => invoke<NewDeviceBindingPreparation>('external_lww_prepare_new_device', {
            request: { ...header(connectionId, binding, staged.receiveId), stagingId: staged.stagingId },
        }),
        async replaceAsNewDevice(staged, preparation, binding) { bindingProgress?.stage('applying', binding.state.targetAuthority); return replaceNativeSyncBindingAsNewDevice(staged, preparation, binding) },
        async resumeNewDeviceBinding(_preparation, _result, binding) { await transport.resumeBinding(binding) },
        async receiveAvailableChanges(binding) {
            if (!active || active.signal.aborted || active.state.targetAuthority !== binding.state.targetAuthority || active.state.selectionEpoch !== binding.state.selectionEpoch) throw new Error('Sync is stopped')
            const joined = joinAbortSignals([binding.signal, active.signal])
            try { await receive({ ...binding, signal: joined.signal }, undefined, true) } finally { joined.dispose() }
        },
    }
    const unregister = registerSyncBindingTransport({ kind: 'external', connectionId }, transport)
    const unregisterGate = registerLibraryFileOperationGate(async () => {
        if (bindingProgress) throw Object.assign(new Error('server-sync-busy'), { code: 'server-sync-busy' })
        await adapter.foreground
        await scheduler.settled()
        await hydrating
    })
    // A renewed sign-in or recovery key clears the stops it caused at once rather than after the receive it starts. An
    // asset download can stop on another storage connection, which only that connection's renewal or unlock clears.
    // Returns whether downloads resume.
    const recover = (renewed: ReadonlySet<string>) => {
        const before = operationFailures.size
        for (const [operation, error] of operationFailures) {
            const target = externalRemedyConnection(error, connectionId)
            if (target !== undefined && renewed.has(target)) operationFailures.delete(operation)
        }
        const cleared = operationFailures.size !== before
        if (cleared) reportOperations()
        const own = renewed.has(connectionId)
        if (own) scheduler.recovered()
        return (own || cleared) && externalRemedyConnection(operationFailures.get('assets'), connectionId) === undefined
    }
    const adapter: Adapter = { transport, scheduler, recover, stopHydration, resumeHydration, hide, generation: 0, dispose() { adapter.generation++; scheduler.stop(); void stopHydration(); active = undefined; reportFailure(connectionId); unregister(); unregisterGate(); clearExternalProgress(connectionId); if (adapters.get(connectionId) === adapter) adapters.delete(connectionId) } }
    return adapter
}

export async function refreshExternalLwwAdapters(state: ExternalStorageState): Promise<void> {
    if (!isTauri) return
    const generation = ++refreshGeneration
    const sync = new Set(state.connections.filter(connection => connection.purpose === 'sync' && ['webdav', 's3', 'google_drive', 'onedrive'].includes(connection.providerId)).map(connection => connection.id))
    const recovered = new Set<Adapter>()
    for (const [id, adapter] of adapters) if (!sync.has(id)) adapter.dispose()
    for (const id of sync) if (!adapters.has(id)) adapters.set(id, createAdapter(id))
    // The verified time changes when a sign-in, key or address is renewed, which a sync failure leaves unmarked. A
    // removed connection can no longer be signed in, so the stops it caused clear as well.
    const recoveries = new Map(state.connections.map(connection => [connection.id, JSON.stringify([connection.status, connection.lastError, connection.lastVerifiedAtMs])]))
    for (const adapter of adapters.values()) {
        const previous = adapter.recoveries
        adapter.recoveries = recoveries
        if (!previous) continue
        const renewed = new Set([...previous].filter(([id, recovery]) => recoveries.get(id) !== recovery).map(([id]) => id))
        if (renewed.size && adapter.recover(renewed)) recovered.add(adapter)
    }
    const binding = await native.state()
    if (generation !== refreshGeneration) return
    for (const [id, adapter] of adapters) if (binding.target.kind !== 'external' || binding.target.connectionId !== id) {
        adapter.generation++; adapter.scheduler.stop(); void adapter.stopHydration(); adapter.state = undefined
    } else if (adapter.state?.targetAuthority === binding.targetAuthority && recovered.has(adapter)) {
        adapter.resumeHydration(true)
    }
    if (binding.target.kind === 'external' && resumeBoundTarget) {
        const adapter = adapters.get(binding.target.connectionId)
        if (adapter && adapter.state?.targetAuthority !== binding.targetAuthority) await resumeCurrentSyncBinding(binding.target)
    }
}

export async function installExternalLwwAdapters(state: ExternalStorageState, resumeBound = true): Promise<() => void> {
    if (!isTauri) return () => {}
    resumeBoundTarget = resumeBound
    await refreshExternalLwwAdapters(state)
    const dirty = (generation = false) => { for (const adapter of adapters.values()) if (adapter.state) adapter.scheduler.dirty(generation) }
    const disposeRevision = subscribeLocalPersistentRevision((_revision, cause) => dirty(cause === 'generation-complete'))
    const disposeDevice = await listen(SERVER_SYNC_DEVICE_CHANGED_EVENT, () => dirty())
    let lastConversation = ''
    const disposeConversation = getPersistentDataRuntime().subscribeActiveConversationViewportSource(source => {
        if (!source) return
        const value = getPersistentDataRuntime().captureSelectedConversationTarget()
        if (!value) return
        const identity = JSON.stringify([value.characterId, value.conversationId])
        if (identity === lastConversation) return
        lastConversation = identity
        for (const adapter of adapters.values()) if (adapter.state) void adapter.scheduler.conversationOpened()
    })
    let hiddenFlushed = document.visibilityState === 'hidden'
    let wasOnline = navigator.onLine
    let disposed = false
    const foreground = () => {
        if (disposed || isLibraryFileOperationReserved()) return
        const wentOffline = !navigator.onLine && wasOnline
        wasOnline = navigator.onLine
        const flushHidden = document.visibilityState === 'hidden' && !hiddenFlushed
        hiddenFlushed = document.visibilityState === 'hidden'
        for (const adapter of adapters.values()) if (adapter.state) {
            if (document.visibilityState === 'hidden' || !navigator.onLine) {
                if (wentOffline) { adapter.generation++; adapter.scheduler.stop(); void adapter.stopHydration() }
                // Running sends, receives and hydration continue hidden; the scheduler's hold covers the publication.
                else if (flushHidden) void adapter.hide(navigator.onLine).catch(() => {})
            }
            else if (!adapter.foreground) {
                adapter.generation++
                adapter.scheduler.pauseAutomatic()
                const generation = adapter.generation
                const authority = adapter.state.targetAuthority
                adapter.foreground = (async () => {
                    await adapter.scheduler.settled()
                    if (adapters.get(adapter.state?.target.kind === 'external' ? adapter.state.target.connectionId : '') !== adapter
                        || generation !== adapter.generation || document.visibilityState === 'hidden' || !navigator.onLine) return
                    const current = await native.state()
                    if (generation !== adapter.generation || current.targetAuthority !== authority || current.target.kind !== 'external' || !foregroundAvailable()) return
                    await invoke('external_lww_resume', { connectionId: current.target.connectionId })
                    if (generation === adapter.generation && foregroundAvailable()) { adapter.resumeHydration(); await adapter.scheduler.resumeForeground() }
                })().catch(() => {}).finally(() => {
                    adapter.foreground = undefined
                    if (generation !== adapter.generation && adapter.state && foregroundAvailable() && [...adapters.values()].includes(adapter)) foreground()
                })
            }
        }
    }
    document.addEventListener('visibilitychange', foreground)
    window.addEventListener('online', foreground)
    window.addEventListener('offline', foreground)
    const releaseFileOperation = subscribeLibraryFileOperationReleased(() => {
        const resuming = [...adapters.values()].flatMap(adapter => adapter.foreground ? [adapter.foreground] : [])
        if (resuming.length) void Promise.allSettled(resuming).then(foreground)
        else foreground()
    })
    return () => {
        disposed = true
        releaseFileOperation()
        disposeRevision(); disposeDevice(); disposeConversation()
        document.removeEventListener('visibilitychange', foreground)
        window.removeEventListener('online', foreground); window.removeEventListener('offline', foreground)
        refreshGeneration++
        for (const adapter of [...adapters.values()]) adapter.dispose()
        resumeBoundTarget = true
    }
}

/** Whether sync with this connection was started in this session. */
export function isExternalLwwRunning(connectionId: string): boolean {
    return !!adapters.get(connectionId)?.state
}

export async function requestExternalLwwNow(connectionId: string): Promise<void> {
    const adapter = adapters.get(connectionId)
    if (!adapter) throw new Error('Sync binding transport is unavailable')
    await runWithMobileBackgroundTask('sync', async task => {
        // A binding the start left stopped resumes before it publishes.
        if (!adapter.state) await resumeCurrentSyncBinding({ kind: 'external', connectionId })
        task.signal?.throwIfAborted()
        if (adapters.get(connectionId) !== adapter || !adapter.state) throw new DOMException('Binding changed', 'AbortError')
        await adapter.scheduler.manualNow(task.signal)
        adapter.resumeHydration(true, true)
    }, undefined, true)
}

/** Raises the library to this app's data format. Sync resumes on the next sync request. */
export async function switchExternalLwwLibraryFormat(connectionId: string): Promise<void> {
    const adapter = adapters.get(connectionId)
    if (!adapter) throw new Error('Sync binding transport is unavailable')
    // A connection whose first publication stopped at the format level resumes before it switches.
    if (!adapter.state) await resumeCurrentSyncBinding({ kind: 'external', connectionId })
    const state = adapter.state
    if (adapters.get(connectionId) !== adapter || !state) throw new DOMException('Binding changed', 'AbortError')
    const progress = beginExternalProgress(connectionId, 'sync')
    try {
        await progress.invoke('external_lww_switch_format', { request: header(connectionId, { state, signal: new AbortController().signal }) }, state.targetAuthority)
        progress.finish('complete', true)
    } catch (error) { progress.finish(externalProgressFailure(error)); throw error }
}

export function externalLwwExitDrain(connectionId: string, selectionEpoch: string, fenced = false): SyncExitDrainAdapter | null {
    const adapter = adapters.get(connectionId)
    if (!adapter) return null
    return {
        id: `external:${connectionId}:${selectionEpoch}`,
        async drain(target, signal) {
            signal.throwIfAborted()
            adapter.generation++
            adapter.scheduler.stop()
            await Promise.all([adapter.scheduler.settled(), adapter.stopHydration()])
            signal.throwIfAborted()
            if (target.selectionId !== `external:${connectionId}:${selectionEpoch}` || target.selectionEpoch !== selectionEpoch) return { kind: 'blocked', reason: 'sync-binding-changed' }
            const binding = await native.state()
            if (binding.target.kind !== 'external' || binding.target.connectionId !== connectionId || binding.selectionEpoch !== selectionEpoch) return { kind: 'blocked', reason: 'sync-binding-changed' }
            try {
                await adapter.scheduler.publishNow(true, signal, async call => {
                    call.signal.throwIfAborted()
                    const result = await invoke<{ segments: string; more: boolean }>('external_lww_publish', { request: { ...header(connectionId, { state: binding, signal: call.signal }), exitTarget: { revision: String(target.revision), libraryEpoch: target.libraryEpoch, selectionEpoch } } })
                    call.signal.throwIfAborted()
                    return { count: Number(result.segments), more: result.more }
                })
                signal.throwIfAborted()
                if (!fenced) await adapter.scheduler.receiveNow(true, signal)
                return { kind: 'complete' }
            } catch (error) { return { kind: 'blocked', reason: (error as { kind?: string })?.kind ?? 'sync-unavailable' } }
        },
        async cancel() { adapter.generation++; adapter.scheduler.stop(); const hydration = adapter.stopHydration(); await invoke('external_lww_fence', { connectionId, newDevice: false }); await Promise.all([adapter.scheduler.settled(), hydration]) },
        async resumeAfterExitCancel() {
            const generation = ++adapter.generation
            await adapter.scheduler.settled()
            if (adapters.get(connectionId) !== adapter || !adapter.state) return
            const authority = adapter.state.targetAuthority
            const binding = await native.state()
            if (generation !== adapter.generation || binding.target.kind !== 'external' || binding.target.connectionId !== connectionId
                || binding.targetAuthority !== authority || binding.selectionEpoch !== selectionEpoch) return
            await invoke('external_lww_resume', { connectionId })
            if (generation === adapter.generation && adapters.get(connectionId) === adapter && foregroundAvailable()) { adapter.scheduler.start(); adapter.resumeHydration() }
        },
    }
}
