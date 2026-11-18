export type LwwOperation = 'publish' | 'receive' | 'maintenance'
export interface LwwCall { signal: AbortSignal; turnLimit?: number }
export interface LwwTurn { count: number; more: boolean }
export interface LwwSchedulerDependencies {
    available(): boolean
    publish(call: LwwCall): Promise<LwwTurn>
    receive(call: LwwCall): Promise<LwwTurn>
    maintain?(call: LwwCall): Promise<void>
    /** Checks the library format level again, failing while it still stops sync. */
    checkFormat?(call: LwwCall): Promise<void>
    failed(error: unknown, operation: LwwOperation): void
    succeeded?(operation: LwwOperation): void
    now?(): number
    wallNow?(): number
    turnLimit?: number
    setTimer?(callback: () => void, delay: number): unknown
    clearTimer?(timer: unknown): void
    /** Reports whether work is running, waiting, or due without a new check; work a stop blocks is not due. */
    busy?(busy: boolean): void
    /** Reports whether the last receive left changes to receive, so the next one follows at once. */
    catchingUp?(catchingUp: boolean): void
}
type ExplicitKind = 'publish' | 'receive' | 'manual'
// Another device can switch the library format, so these blocks are checked again on each sync trigger.
export const formatKinds = ['libraryFormatSwitchRequired', 'appUpdateRequired']
export const failureKind = (error: unknown) => (error as { kind?: string; code?: string } | undefined)?.kind ?? (error as { code?: string } | undefined)?.code ?? ''
/** Whether a failure stops the operation until recovery instead of retrying it. */
const blocks = (error: unknown) => {
    const failure = error as { retryable?: boolean; action?: string } | undefined
    const kind = failureKind(error)
    return kind !== 'clockSkew' && kind !== 'cancelled' && (failure?.retryable === false || ['reauthenticate', 'unlock-key', 'free-space', 'check-endpoint'].includes(failure?.action ?? '')
        || ['unauthorized', 'reauthRequired', 'deviceVaultUnavailable', 'repositoryKeyUnavailable', 'localStorageFull', 'localPermissionDenied', 'storageFull', 'endpointRejected', 'corrupt', 'repositoryMismatch', ...formatKinds].includes(kind))
}
interface Waiting {
    kind: ExplicitKind
    promise: Promise<void>
    resolve(): void
    reject(error: unknown): void
    controller: AbortController
    participants: number
    unbounded: boolean
    publish?: (call: LwwCall) => Promise<LwwTurn>
}

export function createLwwScheduler(dependencies: LwwSchedulerDependencies) {
    const now = dependencies.now ?? (() => performance.now())
    const wallNow = dependencies.wallNow ?? Date.now
    const setTimer = dependencies.setTimer ?? ((callback, delay) => setTimeout(callback, delay))
    const clearTimer = dependencies.clearTimer ?? (timer => clearTimeout(timer as ReturnType<typeof setTimeout>))
    const limit = dependencies.turnLimit ?? 4
    let enabled = false
    // Hidden: timers stop, routine publishes wait, and only a receive that returned pages continues.
    let hidden = false
    let receiveMore = false
    let receiveBehind = false
    let reportedBusy = false
    let reportedCatchingUp = false
    let epoch = 0
    let lifetime = new AbortController()
    let dirtyVersion = 0
    let coveredVersion = 0
    let firstDirty: number | undefined
    let lastDirty = 0
    let immediate = false
    let receivePending = false
    let nextDirection: 'publish' | 'receive' = 'publish'
    let emptyListings = 0
    let wakeTimer: unknown
    let receiveTimer: unknown
    let maintenanceTimer: unknown
    let formatTimer: unknown
    let inFlight: Promise<void> | undefined
    let maintenance: Promise<void> | undefined
    let rechecking: Promise<void> | undefined
    let activeWaiting: Waiting | undefined
    let quiescence: { promise: Promise<void>; resolve(): void } | undefined
    const waiting = new Map<ExplicitKind, Waiting>()
    const retryAt: Record<LwwOperation, number> = { publish: 0, receive: 0, maintenance: 0 }
    const serviceRetryAt: Record<LwwOperation, number> = { publish: 0, receive: 0, maintenance: 0 }
    const blocked = new Set<LwwOperation>()
    const failureByOperation = new Map<LwwOperation, unknown>()
    const reportBusy = () => {
        const catchingUp = enabled && receiveBehind
        if (catchingUp !== reportedCatchingUp) {
            reportedCatchingUp = catchingUp
            dependencies.catchingUp?.(catchingUp)
        }
        const busy = !!inFlight || !!maintenance || !!rechecking || waiting.size > 0
            || enabled && (receivePending && !blocked.has('receive') || !hidden && hasPublish() && !blocked.has('publish'))
        if (busy === reportedBusy) return
        reportedBusy = busy
        dependencies.busy?.(busy)
    }
    const checkSettled = () => {
        if (!inFlight && !maintenance && !rechecking && !waiting.size) { quiescence?.resolve(); quiescence = undefined }
        reportBusy()
    }
    const aborted = () => new DOMException('Sync scheduling stopped', 'AbortError')
    const clearWake = () => { if (wakeTimer !== undefined) clearTimer(wakeTimer); wakeTimer = undefined }
    const clearReceive = () => { if (receiveTimer !== undefined) clearTimer(receiveTimer); receiveTimer = undefined }
    const clearMaintenance = () => { if (maintenanceTimer !== undefined) clearTimer(maintenanceTimer); maintenanceTimer = undefined }
    const clearFormat = () => { if (formatTimer !== undefined) clearTimer(formatTimer); formatTimer = undefined }
    const hasPublish = () => dirtyVersion !== coveredVersion
    const sendDue = () => Math.max(retryAt.publish, immediate ? 0 : Math.min(lastDirty + 15_000, (firstDirty ?? now()) + 60_000))
    const rememberFailure = (error: unknown, operation: LwwOperation) => {
        const deadline = Number((error as { retryAtMs?: string | number } | undefined)?.retryAtMs)
        if (Number.isFinite(deadline)) serviceRetryAt[operation] = Math.max(serviceRetryAt[operation], now() + Math.max(0, deadline - wallNow()))
        retryAt[operation] = Math.max(retryAt[operation], serviceRetryAt[operation],
            now() + (operation === 'publish' ? 15_000 : operation === 'receive' ? 20_000 : 60_000))
        if (blocks(error)) blocked.add(operation)
        failureByOperation.set(operation, error)
        dependencies.failed(error, operation)
    }
    const formatBlocks = () => [...blocked].filter(operation => formatKinds.includes(failureKind(failureByOperation.get(operation))))
    /** Clears format blocks once the level matches; a failure that still stops sync replaces them. */
    const checkFormat = (signal: AbortSignal): Promise<void> => {
        if (!dependencies.checkFormat || !formatBlocks().length) return Promise.resolve()
        if (rechecking) return rechecking
        const captured = epoch
        rechecking = Promise.resolve().then(() => dependencies.checkFormat!({ signal })).then(() => {
            if (captured !== epoch) throw aborted()
            for (const operation of formatBlocks()) {
                blocked.delete(operation); failureByOperation.delete(operation); retryAt[operation] = serviceRetryAt[operation]
                dependencies.succeeded?.(operation)
            }
        }, error => {
            if (captured === epoch && !signal.aborted && blocks(error)) for (const operation of formatBlocks()) rememberFailure(error, operation)
            throw error
        }).finally(() => { rechecking = undefined; pump(); scheduleMaintenance(); checkSettled() })
        reportBusy()
        return rechecking
    }
    const recheck = () => {
        if (enabled && !hidden && dependencies.available()) void checkFormat(lifetime.signal).catch(() => {})
    }
    const scheduleReceive = () => {
        clearReceive()
        if (!enabled || hidden) return
        receiveTimer = setTimer(() => { receiveTimer = undefined; receivePending = true; pump() }, emptyListings >= 3 ? 60_000 : 20_000)
    }
    const resetListing = () => {
        const backedOff = emptyListings >= 3
        emptyListings = 0
        if (backedOff && receiveTimer !== undefined) scheduleReceive()
    }
    const scheduleMaintenance = () => {
        if (!enabled || hidden || !dependencies.maintain || maintenanceTimer !== undefined || maintenance || blocked.has('maintenance')) return
        maintenanceTimer = setTimer(() => {
            maintenanceTimer = undefined
            if (!enabled || hidden) return
            if (!dependencies.available() || hasPublish()) { scheduleMaintenance(); return }
            const captured = epoch
            const signal = lifetime.signal
            maintenance = Promise.resolve().then(() => dependencies.maintain!({ signal })).then(() => {
                if (captured === epoch) dependencies.succeeded?.('maintenance')
            }, error => { if (captured === epoch && !signal.aborted) rememberFailure(error, 'maintenance') }).finally(() => {
                maintenance = undefined; scheduleMaintenance(); checkSettled()
            })
            reportBusy()
        }, Math.max(60_000, retryAt.maintenance - now()))
    }
    const execute = async (operation: 'publish' | 'receive', routine: boolean, signal: AbortSignal, captured: number, publish?: (call: LwwCall) => Promise<LwwTurn>) => {
        signal.throwIfAborted()
        const version = dirtyVersion
        if (operation === 'publish') { firstDirty = undefined; immediate = false }
        else { receivePending = false; clearReceive() }
        try {
            const result = await (operation === 'publish' && publish ? publish : dependencies[operation])({ signal, ...(routine ? { turnLimit: limit } : {}) })
            signal.throwIfAborted()
            if (captured !== epoch) throw aborted()
            failureByOperation.delete(operation)
            dependencies.succeeded?.(operation)
            if (operation === 'publish') {
                coveredVersion = version
                if (result.more) {
                    dirtyVersion++
                    firstDirty ??= now()
                    lastDirty = now()
                    // Generating-only outboxes cannot make progress until a later generation event.
                    immediate ||= result.count > 0
                } else if (!hasPublish()) firstDirty = undefined
            } else {
                receiveMore = result.more && result.count > 0
                receiveBehind = result.more
                receivePending ||= hidden ? receiveMore : result.more
                emptyListings = result.more || result.count ? 0 : emptyListings + 1
                if (result.more && !result.count) retryAt.receive = Math.max(retryAt.receive, now() + 20_000)
                scheduleReceive()
            }
        } catch (error) {
            if (captured === epoch && !signal.aborted) {
                if (operation === 'publish') { firstDirty ??= now(); immediate = true }
                else { receiveMore = false; receivePending ||= !hidden }
                rememberFailure(error, operation)
                // A retried receive still has changes to take; only a blocking failure ends the catch-up.
                if (operation === 'receive' && blocked.has('receive')) receiveBehind = false
            }
            throw error
        }
    }
    const explicitDue = (item: Waiting) => item.kind === 'manual' ? Math.max(serviceRetryAt.publish, serviceRetryAt.receive) : serviceRetryAt[item.kind]
    const pump = () => { dispatch(); reportBusy() }
    const dispatch = () => {
        clearWake()
        if (inFlight) return
        const pendingExplicit = waiting.get('manual') ?? waiting.get('publish') ?? waiting.get('receive')
        const explicit = pendingExplicit && explicitDue(pendingExplicit) <= now() ? pendingExplicit : undefined
        const routine = enabled && dependencies.available()
        // A hidden receive continues only while its next page is due now.
        if (hidden && receivePending && (!routine || blocked.has('receive') || retryAt.receive > now())) receivePending = false
        const publishDue = routine && !hidden && hasPublish() && !blocked.has('publish') ? sendDue() : Infinity
        const receiveDue = routine && receivePending && !blocked.has('receive') ? retryAt.receive : Infinity
        let operation: 'publish' | 'receive' | undefined
        if (!explicit) {
            if (publishDue <= now() && receiveDue <= now()) operation = nextDirection
            else if (publishDue <= now()) operation = 'publish'
            else if (receiveDue <= now()) operation = 'receive'
        }
        if ((!explicit || explicitDue(explicit) > now()) && !operation) {
            const due = Math.min(pendingExplicit ? explicitDue(pendingExplicit) : Infinity, publishDue, receiveDue)
            if (Number.isFinite(due)) wakeTimer = setTimer(() => { wakeTimer = undefined; pump() }, Math.max(0, due - now()))
            return
        }
        const captured = epoch
        const signal = explicit?.controller.signal ?? lifetime.signal
        if (explicit) { waiting.delete(explicit.kind); activeWaiting = explicit }
        else nextDirection = operation === 'publish' ? 'receive' : 'publish'
        inFlight = Promise.resolve().then(async () => {
            if (captured !== epoch) throw aborted()
            signal.throwIfAborted()
            if (explicit?.kind === 'manual') {
                await checkFormat(signal)
                await execute('publish', false, signal, captured)
                await execute('receive', false, signal, captured)
            } else await execute(explicit?.kind ?? operation!, !explicit, signal, captured, explicit?.publish)
            explicit?.resolve()
        }).catch(error => { explicit?.reject(error) }).finally(() => {
            activeWaiting = undefined
            inFlight = undefined
            pump(); scheduleMaintenance(); checkSettled()
        })
    }
    const join = (item: Waiting, signal?: AbortSignal): Promise<void> => {
        if (!signal) { item.unbounded = true; return item.promise }
        item.participants++
        return new Promise<void>((resolve, reject) => {
            let finished = false
            const finish = (error?: unknown) => {
                if (finished) return
                finished = true
                signal.removeEventListener('abort', cancel)
                item.participants--
                if (error === undefined) resolve()
                else reject(error)
            }
            const cancel = () => {
                finish(signal.reason ?? aborted())
                if (!item.participants && !item.unbounded) {
                    item.controller.abort(signal.reason ?? aborted())
                    item.reject(item.controller.signal.reason)
                    if (waiting.get(item.kind) === item) { waiting.delete(item.kind); pump(); checkSettled() }
                }
            }
            signal.addEventListener('abort', cancel, { once: true })
            void item.promise.then(() => finish(), error => finish(error))
        })
    }
    const requestExplicit = (kind: ExplicitKind, signal?: AbortSignal, publish?: (call: LwwCall) => Promise<LwwTurn>): Promise<void> => {
        if (signal?.aborted) return Promise.reject(signal.reason ?? aborted())
        if (kind !== 'manual' && blocked.has(kind)) return Promise.reject(failureByOperation.get(kind))
        const existing = waiting.get(kind)
        if (existing) return join(existing, signal)
        const controller = new AbortController()
        let resolve!: () => void
        let reject!: (error: unknown) => void
        const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no })
        const item: Waiting = { kind, promise, resolve, reject, controller, publish, participants: 0, unbounded: false }
        const caller = join(item, signal)
        waiting.set(kind, item)
        if (kind === 'manual') for (const operation of ['publish', 'receive'] as const) if (!formatBlocks().includes(operation)) blocked.delete(operation)
        pump()
        return caller
    }
    const settled = (): Promise<void> => {
        if (!inFlight && !maintenance && !rechecking && !waiting.size) return Promise.resolve()
        if (!quiescence) {
            let resolve!: () => void
            const promise = new Promise<void>(yes => { resolve = yes })
            quiescence = { promise, resolve }
        }
        return quiescence.promise
    }
    const dirty = (generationComplete = false) => {
        if (!enabled) return
        dirtyVersion++
        firstDirty ??= now()
        lastDirty = now()
        immediate ||= generationComplete
        resetListing(); pump()
        // Edits made during a library format stop check the level again once they settle.
        if (formatBlocks().includes('publish')) { clearFormat(); formatTimer = setTimer(() => { formatTimer = undefined; recheck() }, 2_000) }
    }
    const receiveNow = (force = false, signal?: AbortSignal) => {
        if (force) return requestExplicit('receive', signal)
        if (!enabled || hidden || !dependencies.available()) return Promise.resolve()
        clearReceive(); receivePending = true; pump()
        return settled()
    }
    const start = (publish = true) => {
        if (enabled) return
        enabled = true; hidden = false; receiveMore = false; epoch++; lifetime = new AbortController(); emptyListings = 0; nextDirection = 'publish'
        receivePending = true
        if (publish) { dirtyVersion++; firstDirty = now(); lastDirty = now(); immediate = true }
        recheck(); pump(); scheduleMaintenance()
    }
    const pauseAutomatic = () => {
        enabled = false
        clearWake(); clearReceive(); clearMaintenance(); clearFormat()
        coveredVersion = dirtyVersion; firstDirty = undefined; immediate = false; receivePending = false; receiveMore = false; receiveBehind = false
    }
    return {
        dirty,
        start,
        pauseAutomatic() { pauseAutomatic(); pump() },
        /** Aborts routine work whose background time ran out; requests with their own callers keep running. */
        expire() {
            pauseAutomatic(); lifetime.abort(aborted())
            pump(); checkSettled()
        },
        stop() {
            pauseAutomatic(); epoch++; lifetime.abort(aborted())
            for (const item of waiting.values()) { item.controller.abort(aborted()); item.reject(aborted()) }
            waiting.clear()
            activeWaiting?.controller.abort(aborted()); activeWaiting?.reject(aborted())
            checkSettled()
        },
        /**
         * Keeps work already running and stops starting new checks until `resumeForeground`.
         * With `publish`, what was queued before hiding is sent unless the service asked to wait.
         */
        background(publish = false, signal?: AbortSignal): Promise<void> {
            const publication = publish && !blocked.has('publish') && serviceRetryAt.publish <= now()
                ? requestExplicit('publish', signal) : Promise.resolve()
            hidden = true
            clearReceive(); clearMaintenance()
            if (!receiveMore) receivePending = false
            pump()
            return publication
        },
        resumeForeground(publish = true) {
            if (!enabled) start(publish)
            else { hidden = false; resetListing(); if (publish) dirty(true); receivePending = true; recheck(); pump(); scheduleMaintenance() }
            return settled()
        },
        conversationOpened() { resetListing(); recheck(); return receiveNow() },
        publishNow(force = false, signal?: AbortSignal, publish?: (call: LwwCall) => Promise<LwwTurn>) {
            if (force) return requestExplicit('publish', signal, publish)
            dirty(true); return settled()
        },
        receiveNow,
        manualNow: (signal?: AbortSignal) => requestExplicit('manual', signal),
        recovered() { blocked.clear(); pump(); scheduleMaintenance() },
        settled,
    }
}
