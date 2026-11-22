import { notifySyncReceiveCompleted } from './bindingChanges'
import { invoke } from '@tauri-apps/api/core'
import type { SyncExitDrainAdapter } from '../syncExitCoordinator'
import type { RetainableReplacementFence } from '../retainableReplacementFence'
import { isTauri, isTauriAndroid } from 'src/ts/platform'
import { createSyncBindingRecoveryRegistration, installSyncBindingFlow } from './bindingProduction'
import { withPausedPersistentWrites, beginActivatedLibraryGuard, refreshActivatedLibraryUnderPause, applyPersistentLwwReceive, flushPendingDataLocally, getPersistentDataRuntime } from '../persistentDataRuntime.svelte'
import type { PersistentMutationToken } from '../saveCoordinator'
import { RevisionConflictError, type LwwStageReceive, type LwwRemoteChange } from '../persistentDataStore'
import { beginDeferredMobileBackgroundTask, runWithDeferredMobileBackgroundTask, runWithMobileBackgroundTask, type MobileBackgroundTask } from '../../mobileBackgroundTask'
import { isBackgroundExpiryReason } from '../../iosNative'
import { fencePluginExecutionForAuthorityReplacement, invalidatePluginCachesAfterAuthorityReplacement, restartPluginsAfterAuthorityReplacement } from 'src/ts/plugins/apiV3/v3.svelte'
import { registerSyncBindingTransport, resumeCurrentSyncBinding } from './bindingRegistry'
import type { SyncBindingTransport, BindingContext, BindingOutcome, SyncBindingOptions, StagedSyncTarget } from './bindingFlow'
import { createNativeSyncBindingBridge, replaceNativeSyncBinding, replaceNativeSyncBindingAsNewDevice } from './bindingNative'
import { createServerSyncScheduler, integrityCodes, isServerSyncUnavailable, serverSyncErrorCode } from './serverSyncScheduler'
import { alertError } from '../../alert'
import { language } from 'src/lang'
import { subscribeLocalPersistentRevision } from '../persistentRevisionEvents'
import { SERVER_SYNC_DEVICE_CHANGED_EVENT, SERVER_SYNC_REMOTE_HINT_EVENT } from './serverSyncNativeSignals'
import { listen } from '@tauri-apps/api/event'
import { generatingConversations } from '../generatingConversationRegistry'
import { bindSyncTarget, unbindSyncTarget } from './bindingRegistry'
import { subscribeSyncBindingChanges } from './bindingChanges'
import type { ServerConfig } from './serverSync'
import { get } from 'svelte/store'
import { selectedCharID } from 'src/ts/stores.svelte'
import { getDatabase } from '../database.svelte'
import { bytePhase, laneDeltas, readServerSyncLanes, routineMoved, routinePeak, type ServerSyncAttempt, type ServerSyncLane, type ServerSyncStage } from './serverSyncProgress'
import { createRemainingTimeEstimator } from './remainingTime'
import { setAssetResidencyPolicy, type AssetResidencyPolicy } from './serverAssetResidency'
import { isLibraryFileOperationReserved, registerLibraryFileOperationGate, subscribeLibraryFileOperationReleased } from '../libraryFileOperation'
import { getDeviceMarkers } from '../deviceMarkers'

import { readRepresentativeImagePriority, subscribeRepresentativeImagePriority } from '../../ui/representativeImagePriority'

const selectedCharacterId = () => getDatabase().characters[get(selectedCharID)]?.chaId ?? null

let nativeBindings: ReturnType<typeof installSyncBindingFlow> | undefined
let disposeServer: (() => void) | undefined
export function initializeNativeSyncBindings(): () => void {
    if (!isTauri) return () => {}
    if (nativeBindings) return disposeNativeSyncBindings
    let activeToken: PersistentMutationToken | undefined
    const token = () => { if (!activeToken) throw new Error('Sync binding pause is unavailable'); return activeToken }
    const bridge = createNativeSyncBindingBridge()
    nativeBindings = installSyncBindingFlow({
        // Switching to a received state checks that state again, so it is part of applying it.
        native: { ...bridge, switchTarget: (...args) => bindingStaged ? during('applying', () => bridge.switchTarget(...args)) : bridge.switchTarget(...args) },
        whileAsking,
        recovery: createSyncBindingRecoveryRegistration(getPersistentDataRuntime, token),
        withPausedWrites: operation => withPausedPersistentWrites('sync-binding', async pausedToken => {
            activeToken = pausedToken
            try { return await operation() } finally { activeToken = undefined }
        }),
        beginActivatedLibraryGuard: () => beginActivatedLibraryGuard(token()),
        refreshActivatedLibrary: async () => {
            const refresh = () => refreshActivatedLibraryUnderPause(token())
            const outcome = await (binding ? during('refreshing', refresh) : refresh())
            if (outcome.projection !== 'applied') throw new Error('Sync binding projection is unavailable')
        },
        plugins: { fenceExecution: fencePluginExecutionForAuthorityReplacement, invalidateCaches: invalidatePluginCachesAfterAuthorityReplacement, restart: restartPluginsAfterAuthorityReplacement },
    })
    return disposeNativeSyncBindings
}
export function disposeNativeSyncBindings(): void { disposeServer?.(); disposeServer = undefined; nativeBindings?.dispose(); nativeBindings = undefined }

type Status = { configured: boolean; libraryId?: string | null; deviceId?: string | null; writerId: string; bindingAuthority: string; assetPolicy?: AssetResidencyPolicy }
let status: Status = { configured: false, writerId: '', bindingAuthority: '0' }
type PendingBinding = { endpoint: string; libraryId: string; epoch: string; serverEmpty: boolean }
let bindingIncomplete = false
let persistedBinding: BindingContext['state'] | undefined
let context: BindingContext | undefined
let activationPending: BindingContext | undefined
let activating: Promise<void> | undefined
let startupIdentity = ''
let initialReceive = true
let startupFailure: unknown
// Launch requests retry quickly until one of them fails after its retries; later attempts wait for the scheduler.
let startupRetriesSpent = false
// The outage outlasted a retry: a launch request's quick retries or the next automatic attempt.
let outageConfirmed = false
let error = ''
let serverUnavailable = false
const recordFailure = (value: unknown) => {
    error = serverSyncErrorCode(value) || 'server-unreachable'
    serverUnavailable = isServerSyncUnavailable(value)
}
let foreground = false
// Hidden after starting on screen: in-flight work continues, and only a halt stops it.
let continuing = false
const active = () => foreground || continuing
const halt = () => { foreground = false; continuing = false }
// Routine sync's background time ran out while the app was away, so it waits for the return. Android expires a hold
// only while the app is away, also when it keeps the page running and visible; elsewhere the page is hidden then.
let outOfTime = false
type Hold = MobileBackgroundTask | Promise<MobileBackgroundTask>
const releaseHold = (hold: Hold | undefined) => { if (hold) void Promise.resolve(hold).then(task => task.dispose()) }
// Routine sync takes service time only if the app leaves the screen while it runs.
const holdRoutineSync = (): Hold => {
    const hold = beginDeferredMobileBackgroundTask('sync')
    // When the platform's time runs out, routine sync stops where it stands; returning to the screen resumes it.
    void Promise.resolve(hold).then(task => task.signal?.addEventListener('abort', () => {
        if (!isBackgroundExpiryReason(task.signal?.reason)) return
        if (isTauriAndroid || document.visibilityState === 'hidden') outOfTime = true
        halt(); if (hydrating) hydrationPending = true
        void scheduler.fence()
    }, { once: true }))
    return hold
}
let attemptHold: Hold | undefined
let pendingHold: Hold | undefined
let localChanges = 0
const noteLocalChange = () => {
    localChanges++
    if (foreground && !pendingHold) pendingHold = holdRoutineSync()
}
let heldForCommittedRestore = false
// The hold stays with the binding it stopped, through its resumes and restarts, until Sync now or a new device binding.
const RESTORE_HOLD = 'risuNestServerSyncRestoreHold'
async function saveRestoreHold(state?: BindingContext['state']): Promise<void> {
    const epoch = heldForCommittedRestore && state ? state.selectionEpoch : null
    const markers = getDeviceMarkers()
    if (markers.getItem(RESTORE_HOLD) !== epoch) await markers.commitItem!(RESTORE_HOLD, epoch)
}
let replacing = false
let hydrating: Promise<void> | undefined
let hydrationPending = false
let representativesPending = false
let representativesTimer: ReturnType<typeof setTimeout> | undefined
let priorityTimer: ReturnType<typeof setTimeout> | undefined
let hydrationAgain = false
let hydrationError = ''
let schedulerAuthority: string | undefined
// A connection that asked to keep assets on the server, until its binding to that library is active.
let requestedRemoteAssets: { libraryId: string } | undefined
const listeners = new Set<(value: ReturnType<typeof snapshot>) => void>()
let offlineNoticePending = false
let offlineNoticeClaimed = false
const changed = () => {
    for (const listener of listeners) listener(snapshot())
    if (offlineNoticePending || offlineNoticeClaimed || !isTauri || document.visibilityState === 'hidden'
        // A committed restore keeps automatic sync stopped, so nothing retries while it holds.
        || binding || heldForCommittedRestore || (!context && persistedBinding?.target.kind !== 'server')
        || getDatabase().risunestSyncStatusUi === false
        || !error || !serverUnavailable
        || !outageConfirmed) return
    offlineNoticePending = true
    // Native state survives a WebView reload, including iOS content-process recovery.
    void invoke<boolean>('server_sync_claim_offline_notice').then(claimed => {
        offlineNoticeClaimed = true
        if (claimed) alertError(language.risuNest.serverSync.offlineNotice)
    }).catch(() => {}).finally(() => { offlineNoticePending = false })
}
// One attempt runs from the first sync step until sending, receiving, asset downloads and binding
// have all stopped. Native counts are read only while a view watches it.
let attempt: ServerSyncAttempt | undefined
let attemptFailed = false
let attemptCountsAsSync = false
// Read when a watched attempt starts, so even an attempt that ends before its next read has a start to count from.
let baseline: Promise<ServerSyncLane[] | undefined> | undefined
let binding = false
let connectionAttempt: { dismissed: boolean } | undefined
// A binding that received the server state it replaces this library with.
let bindingStaged = false
let operations = 0
let lastSuccessAt: number | undefined
// A routine attempt that moved anything stays on screen as finished for a moment.
let finished: ServerSyncAttempt | undefined
let finishedTimer: ReturnType<typeof setTimeout> | undefined
let watchers = 0
let sampling: ReturnType<typeof setInterval> | undefined
const remaining = createRemainingTimeEstimator()
const readLanes = () => readServerSyncLanes().catch(() => undefined)
async function sample(): Promise<void> {
    const captured = attempt
    if (!captured) return
    const first = !baseline
    const start = baseline ??= readLanes()
    // The upload a bar fills toward is fixed once, when the attempt begins publishing.
    const plan = captured.plannedSend === undefined && captured.stages.includes('publishing')
    const [before, lanes, pending] = await Promise.all([start, first ? start : readLanes(), plan ? controller.pendingChanges().catch(() => undefined) : undefined])
    if (captured !== attempt) return
    if (!before) { if (baseline === start) baseline = undefined; return }
    if (!lanes) return
    captured.lanes = laneDeltas(lanes, before)
    if (typeof pending === 'number') captured.plannedSend = pending + (captured.lanes.find(lane => lane.lane === 'send')?.itemsDone ?? 0)
    // A receive is measured against the server changes it reported when first seen at work.
    const receive = captured.lanes.find(lane => lane.lane === 'receive')
    if (captured.plannedReceive === undefined && receive && receive.backlogDone > 0) captured.plannedReceive = receive.backlogDone + receive.backlogLeft
    captured.peak = routinePeak(captured)
    const at = Date.now()
    captured.network = {
        id: captured.network?.id ?? crypto.randomUUID(), atMs: performance.now(),
        sentBytes: String(captured.lanes.reduce((total, lane) => total + lane.sentBytes, 0)),
        receivedBytes: String(captured.lanes.reduce((total, lane) => total + lane.receivedBytes, 0)),
        sending: lanes.some(lane => lane.active && lane.sending),
        receiving: lanes.some(lane => lane.active && lane.receiving),
    }
    const phase = bytePhase(captured)
    captured.remainingMs = remaining.update(phase?.key, phase?.done ?? 0, phase?.total ?? 0, at, phase?.items)
    changed()
}
const clearFinished = () => { clearTimeout(finishedTimer); finishedTimer = undefined; finished = undefined }
/** Shows a routine attempt that sent or received anything as finished, from one last read of its counts. */
async function finish(ended: ServerSyncAttempt, start: Promise<ServerSyncLane[] | undefined>): Promise<void> {
    const [before, lanes] = await Promise.all([start, readLanes()])
    if (!before || !lanes || attempt || !watchers) return
    ended.lanes = laneDeltas(lanes, before)
    if (!routineMoved(ended)) return
    clearFinished()
    ended.endedAt = Date.now()
    ended.remainingMs = undefined
    finished = ended
    finishedTimer = setTimeout(() => { clearFinished(); changed() }, 1500)
    changed()
}
const watchSamples = () => {
    const wanted = watchers > 0 && !!attempt
    if (wanted && !sampling) { sampling = setInterval(() => { void sample() }, 500); void sample() }
    else if (!wanted && sampling) { clearInterval(sampling); sampling = undefined }
}
/** Shows `stage` as running until `operation` settles. Sending and receiving can run at once. */
async function during<T>(stage: ServerSyncStage, operation: () => Promise<T>, countsAsSync = true): Promise<T> {
    // Connecting and downloading every asset show their steps; a routine attempt they join becomes one of them.
    const mode = binding || operations ? 'full' : 'routine'
    if (!attempt) { attempt = { mode, startedAt: Date.now(), stages: [], active: [], current: stage }; attemptFailed = false; attemptCountsAsSync = false; baseline = undefined; remaining.reset(); clearFinished(); watchSamples(); attemptHold ??= holdRoutineSync() }
    else if (mode === 'full') attempt.mode = mode
    attemptCountsAsSync ||= countsAsSync
    const entered = attempt
    if (!entered.stages.includes(stage)) entered.stages.push(stage)
    entered.active.push(stage); entered.current = stage
    changed()
    try { return await operation() }
    finally { entered.active.splice(entered.active.lastIndexOf(stage), 1); if (entered === attempt) changed() }
}
const settleAttempt = () => {
    if (!attempt || attempt.active.length || scheduler.isRunning() || hydrating || binding || operations) return
    const ended = attempt, start = baseline
    if (!attemptFailed && attemptCountsAsSync) lastSuccessAt = Date.now()
    attempt = undefined
    releaseHold(attemptHold); attemptHold = undefined
    watchSamples()
    if (!attemptFailed && ended.mode === 'routine' && start) void finish(ended, start)
    changed()
}
/** Stops the elapsed time of a binding while it waits for the user to answer. */
async function whileAsking<T>(ask: () => Promise<T>): Promise<T> {
    const held = binding ? attempt : undefined
    if (!held || held.pausedAt !== undefined) return ask()
    const pausedAt = held.pausedAt = Date.now()
    changed()
    try { return await ask() }
    finally { held.pausedMs = (held.pausedMs ?? 0) + Date.now() - pausedAt; held.pausedAt = undefined; if (held === attempt) changed() }
}
/** A cancelled or failed step keeps the attempt from counting as a completed sync. */
async function attempted<T>(operation: () => Promise<T>): Promise<T> {
    try { return await operation() } catch (value) { attemptFailed = true; throw value }
}
const header = () => {
    if (!context || context.signal.aborted) throw new Error('Sync binding is unavailable')
    return { bindingAuthority: context.state.targetAuthority, requestId: crypto.randomUUID() }
}
const checkContext = (captured: BindingContext) => {
    captured.signal.throwIfAborted()
    if (captured !== context) throw new Error('Sync binding changed')
}
const bindingIdentity = (state: BindingContext['state']) => JSON.stringify([state.libraryId, state.targetAuthority, state.selectionEpoch, state.target])
const checkReceiveContext = (c: BindingContext) => {
    c.signal.throwIfAborted()
    if (!context || bindingIdentity(context.state) !== bindingIdentity(c.state)) throw new Error('Sync binding changed')
    context.signal.throwIfAborted()
}
// A binding this launch did not resume has no context, which is its launch failure, not a binding change.
const checkResumedReceiveContext = (c: BindingContext) => {
    c.signal.throwIfAborted()
    if (!context) throw new Error(error || 'Sync binding is unavailable')
    checkReceiveContext(c)
}
const startupRetryDelays = [1_000, 2_000]
// A retry starts only this soon after the first attempt: a slow failure is reported at once, and a wait that ran late while the app was suspended ends the retries.
const startupRetryWindow = 10_000
// A server update, a rate limit and a spent retry budget keep their own waits.
const retriesQuickly = (value: unknown) => isServerSyncUnavailable(value) && !['server-updating', 'sync-retry-budget-exhausted'].includes(serverSyncErrorCode(value))
    && !(typeof value === 'object' && value !== null && 'status' in value && value.status === 429)
/** Resolves false, without waiting further, once the binding it waits for is stopped or replaced. */
function startupRetryWait(captured: BindingContext, delay: number): Promise<boolean> {
    if (captured.signal.aborted) return Promise.resolve(false)
    return new Promise(resolve => {
        const stop = () => { clearTimeout(timer); resolve(false) }
        const timer = setTimeout(() => { captured.signal.removeEventListener('abort', stop); resolve(captured === context) }, delay)
        captured.signal.addEventListener('abort', stop, { once: true })
    })
}
async function startupRequest<T>(captured: BindingContext, command: string, args: Record<string, unknown>): Promise<T> {
    const started = performance.now()
    const withinWindow = (wait: number) => performance.now() + wait - started <= startupRetryWindow
    for (let attempt = 0; ; attempt++) {
        try {
            const result = await invoke<T>(command, args)
            if (captured === context) startupFailure = undefined
            return result
        } catch (value) {
            const delay = startupRetryDelays[attempt]
            if (args.startup === true && !startupRetriesSpent && delay !== undefined && retriesQuickly(value)
                && withinWindow(delay) && await startupRetryWait(captured, delay) && withinWindow(0)) continue
            if (captured === context && args.startup === true && isServerSyncUnavailable(value)) { startupRetriesSpent = true; outageConfirmed = true }
            if (captured === context && initialReceive) {
                const activationReady = typeof value === 'object' && value !== null && 'startupLocalReady' in value && value.startupLocalReady === true
                startupFailure = command !== 'server_sync_lww_activate' || activationReady ? value : undefined
            }
            throw value
        }
    }
}
function canStartPluginsOffline(value: unknown): boolean {
    if (!initialReceive || binding || bindingIncomplete || heldForCommittedRestore || isLibraryFileOperationReserved()
        || scheduler.isBlocked() || !context || !persistedBinding || bindingIdentity(persistedBinding) !== bindingIdentity(context.state)) return false
    if (typeof value !== 'object' || !value || ('retryable' in value && value.retryable === false)) return false
    const code = serverSyncErrorCode(value)
    if (['server-unreachable', 'server-timeout', 'directory-unreachable', 'directory-response-error', 'directory-incomplete-response', 'request-timeout', 'server-updating'].includes(code)) return true
    return code === 'server-response-error' && isServerSyncUnavailable(value)
}
async function ensureActivated(captured: BindingContext): Promise<void> {
    checkContext(captured)
    if (activationPending !== captured) return
    if (!activating) {
        activating = during('applying', async () => {
            await startupRequest(captured, 'server_sync_lww_activate', { request: header(), startup: initialReceive && !binding })
            checkContext(captured)
            if (remoteAssetsRequested(captured)) await keepAssetsRemote(captured)
            activationPending = undefined
        }).finally(() => { activating = undefined })
    }
    await activating
    checkContext(captured)
}
const receivedBodyReferences = (changes: LwwRemoteChange[]) => changes.some(change => {
    const key: string[] = JSON.parse(change.key)
    if (change.value.kind === 'inline') return key[0] === 'asset' || key[0] === 'inlay'
    if (change.value.kind !== 'object' || key[0] === 'messages') return false
    const descriptor = change.value.descriptor as { dependencies?: unknown[]; dependencyRoot?: unknown; relationRoot?: unknown }
    return !!descriptor.dependencies?.length || !!descriptor.dependencyRoot || !!descriptor.relationRoot
})
const receivedRepresentativeReferences = (changes: LwwRemoteChange[]) => changes.some(change => {
    const key: string[] = JSON.parse(change.key)
    return key[0] === 'character' && (key[2] === 'image' || key[2] === 'trashTime')
        || key[0] === 'exists' && key[1] === 'character'
        || key[0] === 'archive' || key[0] === 'asset'
        || key[0] === 'order' && key[1] === 'characters'
})
/** `continuation` lets hydration that a received page triggered start or continue while hidden. */
function continueHydration(continuation = false): void {
    if ((!hydrationPending && !representativesPending) || !(foreground || continuation && continuing) || !context || context.signal.aborted || scheduler.isBlocked() || scheduler.isWaiting() || isLibraryFileOperationReserved()) return
    if (hydrating) { hydrationAgain = true; return }
    const captured = context
    const epoch = scheduler.epoch()
    const representativesOnly = !hydrationPending
    hydrationPending = false
    representativesPending = false
    hydrationAgain = false
    const iconPriority = readRepresentativeImagePriority()
    hydrating = during('assets', async () => { await ensureActivated(captured); return invoke('server_sync_lww_hydrate', { request: header(), selectedCharacterId: selectedCharacterId(), ...(iconPriority ? { iconPriority } : {}), ...(representativesOnly ? { representativesOnly: true } : {}) }) }, !representativesOnly).then(() => {
        if (captured === context && !captured.signal.aborted && hydrationError && error === hydrationError) { error = ''; hydrationError = '' }
    }).catch(value => {
        attemptFailed = true
        if (captured !== context || captured.signal.aborted) return
        if (representativesOnly) representativesPending = true
        else hydrationPending = true
        if (serverSyncErrorCode(value) !== 'cancelled' && active() && !scheduler.isBlocked()) {
            recordFailure(value); hydrationError = error
            if (isServerSyncUnavailable(value)) scheduler.reportFailure(value, undefined, epoch)
        }
    }).finally(() => {
        hydrating = undefined
        const again = hydrationAgain
        hydrationAgain = false
        changed()
        if (again) continueHydration(true)
        settleAttempt()
    })
}
async function updateForeground(visible: boolean): Promise<void> {
    // A committed restore keeps routine sync stopped until the user resumes it.
    const usable = !replacing && !heldForCommittedRestore && !!context && !context.signal.aborted
    continuing = !visible && usable && (foreground || continuing)
    foreground = visible && usable && !outOfTime
    if (!active() && hydrating) hydrationPending = true
    // Work in flight hands its protection to the hide-time publication without a moment unheld.
    const bridge = !visible && (attemptHold || pendingHold) ? holdRoutineSync() : undefined
    try { await scheduler.foreground(foreground, !visible) }
    finally { releaseHold(bridge) }
    // Changes still unpublished after the hide-time publication wait for the next visible transition.
    if (!visible) { releaseHold(pendingHold); pendingHold = undefined }
    continueHydration()
}
async function retryNativeClock(automatic = false): Promise<void> {
    const captured = context
    if (!captured) throw new Error('Sync binding is unavailable')
    checkContext(captured)
    const request = header()
    try { await invoke('server_sync_lww_retry', { request }) }
    catch (value) { if (!automatic) scheduler.reportFailure(value); throw value }
    checkContext(captured)
    if (!automatic || error === 'clock-skew' || error === 'server-unreachable') { error = ''; hydrationError = ''; changed() }
}
const failures = (value: unknown): unknown[] => value instanceof AggregateError ? value.errors : [value]
// Classifies a failed local apply as native code does for its own store failures: an invariant
// failure stops sync until retry, a busy or unavailable store is tried again.
const localApplyFailure = (value: unknown): unknown => {
    const code = serverSyncErrorCode(value)
    if (code === 'cancelled' || integrityCodes.includes(code) || (value instanceof Error && value.name === 'AbortError')) return value
    const local = value instanceof RevisionConflictError ? 'local-revision-changed'
        : code === 'commit-busy' || (value instanceof Error && ['PersistentMutationFencedError', 'SelectedConversationTransitionInProgressError'].includes(value.name)) ? 'library-operation-busy'
            : code === 'store-error' ? 'local-storage' : 'local-validation'
    return Object.assign(new Error(local, { cause: value }), { code: local, retryable: local !== 'local-validation' })
}
const transientTransportFailure = (value: unknown) => {
    const code = serverSyncErrorCode(value)
    return !!code && !code.startsWith('local-') && typeof value === 'object' && value !== null && 'retryable' in value && value.retryable === true
}
// A revoked or replaced registration, a restored server and credentials the OS cannot open are
// resolved in settings, which a failed startup could not reach.
const settingsResolvedFailure = (value: unknown) => ['unauthorized', 'invalid-device-token', 'server-epoch-changed', 'device-credential-unavailable'].includes(serverSyncErrorCode(value))
async function resumeCurrentServerBinding(state: BindingContext['state']): Promise<void> {
    try { await resumeCurrentSyncBinding(state.target) }
    catch (value) { recordFailure(failures(value)[0]); await controller.ensureStatus(); throw value }
}
/** Sends queued pages until none remain; without `drain` it stops when sync halts. */
async function pushAvailable(captured: BindingContext, drain = false): Promise<void> {
    const changesBefore = localChanges
    await ensureActivated(captured)
    for (;;) {
        captured.signal.throwIfAborted()
        if (captured !== context) throw new Error('Sync binding changed')
        if ((!active() || isLibraryFileOperationReserved()) && !drain) return
        const receipt = await during('publishing', () => invoke('server_sync_lww_push', { request: header(), generating: generatingConversations.snapshot() }))
        captured.signal.throwIfAborted()
        if (captured !== context) throw new Error('Sync binding changed')
        if ((!active() || isLibraryFileOperationReserved()) && !drain) return
        if (!receipt) break
    }
    if (localChanges === changesBefore) { releaseHold(pendingHold); pendingHold = undefined }
    changed()
}
const scheduler = createServerSyncScheduler({
    automaticSyncAvailable: () => !isLibraryFileOperationReserved(),
    push: () => attempted(async () => { if (context) await pushAvailable(context) }),
    pull: (completeAvailable = false) => attempted(async () => {
        const captured = context
        if (!captured) throw new Error('Sync binding is unavailable')
        await ensureActivated(captured)
        let receivedBodies = false
        let receiveFrontier: string | undefined
        for (;;) {
            captured.signal.throwIfAborted()
            if (captured !== context) throw new Error('Sync binding changed')
            const request = await during('downloading', () => startupRequest<LwwStageReceive & { receiveFrontier?: string; more?: boolean }>(captured, 'server_sync_lww_pull', { request: header(), startup: initialReceive }))
            receiveFrontier ??= request.receiveFrontier
            captured.signal.throwIfAborted()
            checkContext(captured)
            initialReceive = false
            // A page is applied under the stage that received it, so reading many pages shows one stage.
            try { await during('downloading', () => applyPersistentLwwReceive(request)) }
            catch (value) { throw localApplyFailure(value) }
            checkContext(captured)
            if (receivedBodyReferences(request.changes)) { receivedBodies = true; hydrationPending = true }
            if (status.assetPolicy === 'remote' && receivedRepresentativeReferences(request.changes)) { receivedBodies = true; representativesPending = true }
            if ((!active() || isLibraryFileOperationReserved()) && !completeAvailable) break
            await invoke('server_sync_lww_ack', { request: { bindingAuthority: request.bindingAuthority, requestId: request.requestId } })
            checkContext(captured)
            if (request.more === false || (receiveFrontier !== undefined && BigInt(request.progress.cursor) >= BigInt(receiveFrontier))
                || (request.more === undefined && request.changes.length === 0)) { notifySyncReceiveCompleted(); break }
        }
        if (receivedBodies) continueHydration(true)
    }),
    retryClock: () => retryNativeClock(true),
    checkFormat: async () => {
        const captured = context
        if (!captured) throw new Error('Sync binding is unavailable')
        await invoke('server_sync_lww_check_format', { request: header() })
        checkContext(captured)
    },
    publishHidden: () => attempted(async () => {
        const captured = context
        if (!captured || captured.signal.aborted) return
        await runWithDeferredMobileBackgroundTask('sync', async () => {
            // A failed local save is reported where it happened; what is already queued still goes out.
            await flushPendingDataLocally('server-sync-hidden').catch(() => {})
            await pushAvailable(captured, true)
        })
    }),
    connect: async operationId => { if (context) { await ensureActivated(context); await invoke('server_sync_notify_start', { request: header(), operationId }) } },
    disconnect: async () => { await invoke('server_sync_notify_stop'); await invoke('server_sync_cancel') },
    stopNotifications: async () => { await invoke('server_sync_notify_stop') },
    failed(value, retried) { if (retried) outageConfirmed = true; recordFailure(value); changed() },
    recovered() { outageConfirmed = false; if (error && error !== hydrationError) { error = ''; changed() }; continueHydration() },
    retrying: continueHydration,
    activity() { settleAttempt(); changed() },
})
// Native code accepts the remote policy only once the server is active, and it must hold before the
// first hydration. A binding to another library, such as one resumed after a cancelled bind, keeps its own.
const remoteAssetsRequested = (c: BindingContext) => !!requestedRemoteAssets && requestedRemoteAssets.libraryId === c.state.libraryId
async function keepAssetsRemote(c: BindingContext): Promise<void> {
    requestedRemoteAssets = undefined
    await during('assetPolicy', () => setAssetResidencyPolicy('remote'))
    checkContext(c)
}
// Scheduler blocks and errors belong to one binding authority and never carry over to another.
const adoptSchedulerAuthority = (c: BindingContext) => {
    const identity = bindingIdentity(c.state)
    if (startupIdentity !== identity) { startupIdentity = identity; initialReceive = true; startupFailure = undefined; startupRetriesSpent = false; outageConfirmed = false }
    if (schedulerAuthority === c.state.targetAuthority) return
    scheduler.reset(); error = ''; hydrationError = ''; outageConfirmed = false; schedulerAuthority = c.state.targetAuthority
}
const transport: SyncBindingTransport = {
    heldForCommittedRestore: () => heldForCommittedRestore,
    // A registration that cannot join this way is refused here, before the library is read.
    inspectTarget: c => during('preparing', () => invoke('server_sync_lww_inspect', { request: { bindingAuthority: c.state.targetAuthority, requestId: crypto.randomUUID() }, newDevice: c.mode === 'new-device' })),
    pullAvailableState: async (inspected,c) => { const staged = await during('downloading', () => invoke<StagedSyncTarget>('server_sync_lww_stage_target', { inspectionId: inspected.inspectionId, request: { bindingAuthority: c.state.targetAuthority, requestId: crypto.randomUUID() } })); bindingStaged = true; return staged },
    replaceFromTarget: (...args) => during('applying', () => replaceNativeSyncBinding(...args)),
    reportStopped(value) {
        if (connectionAttempt?.dismissed) return
        const code = serverSyncErrorCode(failures(value)[0])
        if (code !== 'cancelled') { recordFailure(failures(value)[0]); changed() }
    },
    async fenceOldJobs(c) { halt(); context = undefined; persistedBinding = undefined; await scheduler.fence(); await hydrating; activationPending = undefined; await invoke('server_sync_lww_fence', { newDevice: c.mode === 'new-device' || c.mode === 'fresh-writer' }); hydrationPending = false; representativesPending = false; hydrationAgain = false },
    async receiveAvailableChanges(c) { checkResumedReceiveContext(c); await receiveAvailableServerChanges(); checkReceiveContext(c) },
    async preparePluginStartup(c) {
        checkResumedReceiveContext(c)
        if (canStartPluginsOffline(startupFailure)) return
        try { await transport.receiveAvailableChanges!(c) }
        catch (value) { checkReceiveContext(c); if (startupFailure !== value || !canStartPluginsOffline(value)) throw value }
    },
    async publishInitialSharedState(c) {
        context = c
        await invoke('server_sync_lww_activate', { request: header() })
        checkContext(c)
        await pushAvailable(c, true)
    },
    async resumeBinding(c) {
        try {
            heldForCommittedRestore = c.holdAutomaticSync === true || getDeviceMarkers().getItem(RESTORE_HOLD) === c.state.selectionEpoch
            context = c; adoptSchedulerAuthority(c)
            activationPending = c
            await saveRestoreHold(c.state)
            try { await ensureActivated(c) }
            catch (value) {
                // A busy or unavailable local store is tried again by automatic sync too, without the offline notice.
                if (binding || c.holdAutomaticSync || !isServerSyncUnavailable(value)
                    && !['library-operation-busy', 'server-sync-busy', 'asset-jobs-busy', 'local-store-unavailable'].includes(serverSyncErrorCode(value))) throw value
                checkContext(c)
                attemptFailed = true
                // The receive that activates the binding again clears it.
                scheduler.reportFailure(value, 'pull')
            }
            persistedBinding = c.state; hydrationPending = true
            if (!c.holdAutomaticSync) { await updateForeground(document.visibilityState !== 'hidden'); scheduler.remoteHint() }
        } catch (value) { attemptFailed = true; throw value }
        finally { settleAttempt() }
    },
    prepareNewDeviceBinding: (staged,c) => invoke('server_sync_lww_prepare_new_device', { stagingId: staged.stagingId, request: { bindingAuthority: c.state.targetAuthority, requestId: staged.receiveId } }),
    replaceAsNewDevice: (...args) => during('applying', () => replaceNativeSyncBindingAsNewDevice(...args)),
    prepareFreshWriter: (inspected,c) => invoke('server_sync_lww_prepare_fresh_writer', { inspectionId: inspected.inspectionId, request: { bindingAuthority: c.state.targetAuthority, requestId: crypto.randomUUID() } }),
    async resumeNewDeviceBinding(preparation,result,c) {
        try {
            await during('applying', () => invoke('server_sync_lww_activate_new_device', { authorizationId: preparation.authorizationId, writerId: result.writerId, request: { bindingAuthority: result.bindingAuthority, requestId: crypto.randomUUID() } }))
            heldForCommittedRestore = false; context = c; adoptSchedulerAuthority(c); checkContext(c)
            await saveRestoreHold()
            if (remoteAssetsRequested(c)) await keepAssetsRemote(c)
            persistedBinding = c.state; error = ''; hydrationError = ''; hydrationPending = true
            await scheduler.retry(); await updateForeground(document.visibilityState !== 'hidden'); scheduler.remoteHint()
        } catch (value) { attemptFailed = true; throw value }
        finally { settleAttempt() }
    },
}
export async function receiveAvailableServerChanges(): Promise<void> { await scheduler.receiveAvailableChanges() }
export async function configureServerSyncConnection(config: ServerConfig): Promise<void> { await invoke('server_sync_configure', { config }); error = '' }
// Binding and its initial publication continue while the app is in the background. A failure keeps
// what native code committed visible, so a binding that stopped after its switch can be resumed.
async function bindServer(options: SyncBindingOptions = {}): Promise<BindingOutcome> {
    const currentAttempt = { dismissed: false }
    connectionAttempt = currentAttempt
    binding = true
    try { return await runWithMobileBackgroundTask('sync', () => bindSyncTarget({ kind: 'server', connectionId: 'server' }, options), undefined, true) }
    catch (value) {
        attemptFailed = true
        const code = serverSyncErrorCode(failures(value)[0])
        const statusKnown = await controller.ensureStatus().then(() => true, () => false)
        const bound = bindingIncomplete || persistedBinding?.target.kind === 'server' || context && !context.signal.aborted
        if (currentAttempt.dismissed && statusKnown && !bound) { error = ''; hydrationError = ''; changed() }
        else if (code !== 'cancelled') { recordFailure(failures(value)[0]); changed() }
        throw value
    }
    finally {
        binding = false
        if (connectionAttempt === currentAttempt) connectionAttempt = undefined
        bindingStaged = false
        // What continues after connecting, such as reading back this device's publication, shows as automatic sync.
        if (attempt && !operations && attempt.mode === 'full') { attempt.mode = 'routine'; changed() }
        settleAttempt()
    }
}
/** With `remote`, the device keeps assets on the server from the start; `full` keeps its stored policy. */
export async function connectServerSync(config: ServerConfig, newDevice = false, policy?: AssetResidencyPolicy): Promise<BindingOutcome> {
    await configureServerSyncConnection(config)
    // Setting `full` here would download every asset inside the binding itself.
    requestedRemoteAssets = policy === 'remote' ? { libraryId: config.libraryId } : undefined
    let outcome: BindingOutcome
    try { outcome = await bindServer(newDevice ? { mode: 'new-device' } : {}) }
    finally { requestedRemoteAssets = undefined }
    await controller.ensureStatus()
    return outcome
}
/** Finishes a first binding that stopped after its target switch, with the saved registration. */
export async function completeServerSyncBinding(): Promise<void> { await bindServer(); await controller.ensureStatus() }
export async function disconnectServerSync(): Promise<void> { await unbindSyncTarget(); context = undefined; await controller.ensureStatus() }
/** Drops the failure of a connection attempt that was left without connecting. A binding, even a stopped one, keeps it. */
export function dismissServerSyncConnectionFailure(): void {
    if (bindingIncomplete || persistedBinding?.target.kind === 'server' || context && !context.signal.aborted) return
    if (connectionAttempt) connectionAttempt.dismissed = true
    if (!error) return
    error = ''; hydrationError = ''; changed()
}
/** Stops automatic sync until the returned release runs, so an asset download is not refused as busy. */
export const holdServerSync = () => controller.beginReplacement()
/** Raises the library to this app's data format. Sync resumes on the next retry. */
export async function switchServerSyncLibraryFormat(): Promise<void> {
    // A binding whose first publication stopped at the format level resumes before it switches.
    if (!context || context.signal.aborted) {
        await controller.ensureStatus()
        if (persistedBinding?.target.kind === 'server') await resumeCurrentServerBinding(persistedBinding)
    }
    const captured = context
    if (!captured) throw new Error('Sync binding is unavailable')
    await invoke('server_sync_lww_switch_format', { request: header() })
    checkContext(captured)
}
export function retryServerSync(): Promise<void> {
    return runWithMobileBackgroundTask('sync', () => syncServerNow(), undefined, true)
}
async function syncServerNow(): Promise<void> {
    await controller.ensureStatus()
    const current = persistedBinding
    if (!current || current.target.kind !== 'server') throw new Error('Sync binding is unavailable')
    if (!context || context.signal.aborted || context.state.targetAuthority !== current.targetAuthority || context.state.selectionEpoch !== current.selectionEpoch || context.state.libraryId !== current.libraryId || context.state.target.kind !== 'server' || context.state.target.connectionId !== current.target.connectionId) {
        await resumeCurrentServerBinding(current)
    }
    heldForCommittedRestore = false
    halt()
    try {
        await saveRestoreHold()
        await scheduler.fence()
        await hydrating
        if (context) await ensureActivated(context)
        await retryNativeClock(); await scheduler.retry()
    } catch (value) { scheduler.reportFailure(value); throw value }
    finally { await updateForeground(document.visibilityState !== 'hidden'); changed() }
}
/** With `resumeBound` false, the server transport is registered for settings but a bound server stays stopped. */
export async function installServerSyncProduction({ resumeBound = true }: { resumeBound?: boolean } = {}): Promise<void> {
    if (!isTauri || disposeServer) return
    const disposers = [registerSyncBindingTransport({ kind: 'server', connectionId: 'server' }, transport)]
    disposers.push(registerLibraryFileOperationGate(async () => {
        controller.assertFileOperationAvailable()
        await scheduler.waitForIdle()
        await hydrating
        controller.assertFileOperationAvailable()
    }))
    disposers.push(subscribeLibraryFileOperationReleased(() => {
        void scheduler.resumeAutomaticSync().then(() => continueHydration())
    }))
    disposers.push(subscribeLocalPersistentRevision((_revision,cause) => {
        // Saves come in bursts while chatting, so character icons are checked once they settle.
        if (status.assetPolicy === 'remote') { clearTimeout(representativesTimer); representativesTimer = setTimeout(() => { representativesTimer = undefined; representativesPending = true; continueHydration() }, 2000) }
        noteLocalChange()
        scheduler.localChange(cause === 'generation-complete')
    }))
    // Another sync target taking over fences this one without a status read, so the server card reads it again.
    disposers.push(subscribeSyncBindingChanges(() => { void controller.ensureStatus().catch(() => {}) }))
    // The source is renewed on every store revision; only a different conversation counts as opened.
    // The one open at startup is covered by the startup pull.
    const conversationIdentity = () => {
        const target = getPersistentDataRuntime().captureSelectedConversationTarget()
        return target ? JSON.stringify([target.characterId, target.conversationId]) : ''
    }
    let openedConversation = conversationIdentity()
    disposers.push(getPersistentDataRuntime().subscribeActiveConversationViewportSource(source => {
        const identity = source ? conversationIdentity() : ''
        if (!identity || identity === openedConversation) return
        openedConversation = identity
        void scheduler.conversationOpened().then(() => continueHydration())
    }))
    disposers.push(await listen(SERVER_SYNC_REMOTE_HINT_EVENT, () => scheduler.remoteHint()))
    disposers.push(await listen(SERVER_SYNC_DEVICE_CHANGED_EVENT, () => { noteLocalChange(); scheduler.localChange() }))
    disposers.push(subscribeRepresentativeImagePriority(() => {
        if (status.assetPolicy !== 'remote') return
        clearTimeout(priorityTimer)
        priorityTimer = setTimeout(() => { priorityTimer = undefined; representativesPending = true; continueHydration() }, 150)
    }))
    disposers.push(() => { clearTimeout(priorityTimer); priorityTimer = undefined })
    disposers.push(await listen<{ connected: boolean }>('risu-server-sync-notification', event => scheduler.socket(event.payload.connected)))
    const visibility = () => {
        if (document.visibilityState !== 'hidden') outOfTime = false
        void updateForeground(document.visibilityState !== 'hidden').catch(value => { if (serverSyncErrorCode(value) === 'cancelled') return; recordFailure(value); changed() })
    }
    document.addEventListener('visibilitychange', visibility)
    disposers.push(() => document.removeEventListener('visibilitychange', visibility))
    // A page Android keeps running off screen stays visible, so it hears the return only from the app's own start.
    const lifecycle = (event: Event) => { if (isTauriAndroid && (event as CustomEvent<{ reason?: unknown } | null>).detail?.reason === 'start') visibility() }
    window.addEventListener('risu-native-lifecycle', lifecycle)
    disposers.push(() => { window.removeEventListener('risu-native-lifecycle', lifecycle); outOfTime = false })
    disposeServer = () => { scheduler.dispose(); for (const dispose of disposers) dispose(); context = undefined; persistedBinding = undefined; halt(); hydrationPending = false; representativesPending = false; clearTimeout(representativesTimer); representativesTimer = undefined; hydrationAgain = false; attempt = undefined; releaseHold(attemptHold); attemptHold = undefined; releaseHold(pendingHold); pendingHold = undefined; clearFinished(); watchSamples() }
    await controller.ensureStatus()
    const current = await invoke<BindingContext['state']>('pds_lww_binding_state')
    heldForCommittedRestore ||= current.target.kind === 'server' && getDeviceMarkers().getItem(RESTORE_HOLD) === current.selectionEpoch
    await saveRestoreHold(current)
    if (!resumeBound) return
    if (current.target.kind === 'server' && status.configured) {
        try { await resumeCurrentServerBinding(current) }
        catch (value) {
            // A fenced binding that cannot reach its server keeps the local library usable until retry.
            if (!failures(value).every(failure => transientTransportFailure(failure) || settingsResolvedFailure(failure))) throw value
        }
    } else if (current.target.kind === 'server' && !heldForCommittedRestore) await completeInterruptedBinding()
}
// The user already chose to connect, so a server still as empty as when it was inspected
// is bound without asking again. Anything else waits for Connect in settings.
async function completeInterruptedBinding(): Promise<void> {
    const pending = await invoke<PendingBinding | null>('server_sync_lww_pending_binding')
    if (!pending) return
    if (pending.serverEmpty) {
        try { await completeServerSyncBinding(); if (status.configured) return }
        catch { /* Connect in settings retries with the saved registration and reports the failure. */ }
    }
    bindingIncomplete = true; changed()
}

const snapshot = () => ({ status: { ...status, bound: persistedBinding?.target.kind === 'server' || !!context && !context.signal.aborted }, running: scheduler.isRunning() || !!hydrating, paused: !context || context.signal.aborted || heldForCommittedRestore || scheduler.isBlocked(), blockedCode: scheduler.blockedCode(), replacing, draining: false, error, bindingIncomplete, progress: attempt && { ...attempt, stages: [...attempt.stages], active: [...attempt.active] }, finished: finished && { ...finished, stages: [...finished.stages], active: [...finished.active] }, lastSuccessAt })
const controller = {
    snapshot,
    subscribe: (listener: (value: ReturnType<typeof snapshot>) => void) => { listeners.add(listener); listener(controller.snapshot()); return () => { listeners.delete(listener) } },
    /** Reads native transfer counts into `progress` until the returned stop runs. */
    watchProgress: () => {
        let watching = true
        watchers++; watchSamples()
        return () => { if (watching) { watching = false; watchers--; watchSamples() } }
    },
    /** Shows an operation started outside the scheduler, such as downloading every asset, as `stage`. */
    track: async <T>(stage: ServerSyncStage, operation: () => Promise<T>): Promise<T> => {
        operations++
        try { return await attempted(() => during(stage, operation)) } finally { operations--; settleAttempt() }
    },
    /** The changes waiting to be uploaded, or undefined while no server binding is active. */
    pendingChanges: async () => context && !context.signal.aborted ? invoke<number>('server_sync_lww_pending_count', { request: header() }) : undefined,
    assertFileOperationAvailable: () => {
        if (replacing || binding || operations) {
            throw Object.assign(new Error('server-sync-busy'), { code: 'server-sync-busy' })
        }
    },
    beginReplacement: async () => {
        replacing = true; halt(); if (hydrating) hydrationPending = true
        const release = async () => { replacing = false; await updateForeground(document.visibilityState !== 'hidden'); changed() }
        try { await scheduler.fence(); await hydrating } catch (value) { await release().catch(() => {}); throw value }
        changed(); return release
    },
    confirmReplacement: async () => {},
    holdAutomaticSync: () => { halt(); if (hydrating) hydrationPending = true; void scheduler.fence() },
    ensureStatus: async () => {
        const previousPolicy = status.assetPolicy
        if (isTauri) { [status,persistedBinding] = await Promise.all([invoke<Status>('server_sync_status'),invoke<BindingContext['state']>('pds_lww_binding_state')]) }
        if (status.assetPolicy === 'remote' && previousPolicy !== 'remote') { representativesPending = true; continueHydration() }
        if (status.configured || persistedBinding?.target.kind !== 'server') bindingIncomplete = false
        changed()
    },
}
export function getServerSyncController() { return controller }
export function createServerSyncExitDrainAdapter(id = 'server', _fence?: RetainableReplacementFence): SyncExitDrainAdapter {
    return { id, async drain(target, signal) {
        const cancel = () => { void invoke('server_sync_cancel') }
        signal.addEventListener('abort', cancel, { once: true })
        try {
            signal.throwIfAborted()
            if (!context || context.state.selectionEpoch !== target.selectionEpoch) return { kind: 'blocked', reason: 'binding-authority-changed' }
            halt(); if (hydrating) hydrationPending = true
            await scheduler.fence()
            await hydrating
            if (context) await ensureActivated(context)
            await invoke('server_sync_lww_drain', { request: header(), generating: generatingConversations.snapshot() })
            signal.throwIfAborted()
            return { kind: 'complete' }
        } catch (value) { return { kind: 'blocked', reason: typeof value === 'object' && value && 'code' in value ? String(value.code) : 'server-unreachable' } }
        finally { signal.removeEventListener('abort', cancel) }
    }, cancel: async () => { halt(); if (hydrating) hydrationPending = true; await scheduler.fence(); await hydrating }, resumeAfterExitCancel: async () => { await updateForeground(document.visibilityState !== 'hidden') } }
}
export function holdServerSyncAfterRestore(): void { heldForCommittedRestore = true; controller.holdAutomaticSync() }
export function resumeServerSyncAfterBackup(): void { if (context) void updateForeground(document.visibilityState !== 'hidden') }
export interface ServerSyncCacheUsage {
  /** cacheBytes plus ledgerBytes. */
  totalBytes: number;
  /** The temporary files: protectedBytes plus reclaimableBytes. */
  cacheBytes: number;
  protectedBytes: number;
  reclaimableBytes: number;
  /** The asset residency ledger kept beside the temporary files. */
  ledgerBytes: number;
  /** Part of cacheBytes: what the cache's object database occupies on disk. */
  databaseBytes: number;
  blockedReason: string | null;
}

export const getServerSyncCacheUsage = () => invoke<ServerSyncCacheUsage>('server_sync_cache_usage')
export const cleanupServerSyncCache = () => invoke<ServerSyncCacheUsage>('server_sync_cache_cleanup')
