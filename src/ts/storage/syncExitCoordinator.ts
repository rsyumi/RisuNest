export interface SyncExitTarget {
    revision: number
    libraryEpoch: string
    selectionEpoch: string
    selectionId: string
}

export type SyncExitDrainResult =
    | { kind: 'complete' }
    | { kind: 'blocked'; reason: string }

export type SyncExitDisposition = 'exit' | 'cancelled'
export type SyncExitDecision = 'wait' | 'exit-unsynced' | 'cancel-exit'

export type SyncExitState =
    | { phase: 'idle' }
    | { phase: 'edit-blocked'; error: unknown }
    | { phase: 'saving' }
    | { phase: 'local-failed'; error: unknown }
    | { phase: 'capturing' }
    | { phase: 'syncing'; target: SyncExitTarget; destination: string }
    | { phase: 'remote-delayed'; target: SyncExitTarget; destination: string }
    | {
        phase: 'remote-blocked'
        target: SyncExitTarget | null
        destination: string
        reason: string
    }
    | { phase: 'complete'; target: SyncExitTarget | null }
    | { phase: 'cancelled'; target: SyncExitTarget | null }

export interface SyncExitFence {
    release(): void
}

/**
 * A drain adapter owns one selected synchronization engine. A sequential
 * adapter must tie all retries to signal and must not publish after it aborts.
 */
export interface SyncExitDrainAdapter {
    readonly id: string
    drain(target: SyncExitTarget, signal: AbortSignal): Promise<SyncExitDrainResult>
    /** Resolves only after the engine has stopped scheduling work for this session. */
    cancel(reason: Exclude<SyncExitDecision, 'wait'>): Promise<void>
}

export interface SyncExitCoordinatorDependencies {
    acquireEditFence(): Promise<SyncExitFence>
    flushLocal(): Promise<void>
    checkpointLocal(): Promise<void>
    captureTarget(): Promise<SyncExitTarget>
    selectedDrain(): SyncExitDrainAdapter | null | Promise<SyncExitDrainAdapter | null>
    softWaitMillis?: number
    setTimer?(callback: () => void, delay: number): unknown
    clearTimer?(timer: unknown): void
    reportError?(error: unknown): void
}

interface PendingDecision {
    resolve(decision: SyncExitDecision): void
    promise: Promise<SyncExitDecision>
}

function deferredDecision(): PendingDecision {
    let resolve!: (decision: SyncExitDecision) => void
    const promise = new Promise<SyncExitDecision>((settle) => {
        resolve = settle
    })
    return { resolve, promise }
}

function blockedReason(error: unknown): string {
    if (
        typeof error === 'object'
        && error !== null
        && 'code' in error
        && typeof error.code === 'string'
    ) return error.code
    if (error instanceof Error && error.message) return error.message
    return 'sync-exit-drain-failed'
}

function isEditFenceError(error: unknown): boolean {
    return error instanceof Error && (
        error.name === 'PersistentMutationFencedError'
        || error.name === 'SelectedConversationTransitionInProgressError'
    )
}

function validateTarget(target: SyncExitTarget): SyncExitTarget {
    if (!Number.isSafeInteger(target.revision) || target.revision < 0) {
        throw new RangeError('Exit target revision must be a nonnegative safe integer')
    }
    if (!target.libraryEpoch || !target.selectionEpoch || !target.selectionId) {
        throw new Error('Exit target identity is incomplete')
    }
    return target
}

/** Coordinates one held normal-exit request. It never exits on a timer. */
export function createSyncExitCoordinator(
    dependencies: SyncExitCoordinatorDependencies,
) {
    let state: SyncExitState = { phase: 'idle' }
    let pending: Promise<SyncExitDisposition> | undefined
    let decision: PendingDecision | undefined
    const listeners = new Set<(state: SyncExitState) => void>()
    const setTimer = dependencies.setTimer
        ?? ((callback: () => void, delay: number): unknown =>
            globalThis.setTimeout(callback, delay))
    const clearTimer = dependencies.clearTimer
        ?? ((timer: unknown): void => globalThis.clearTimeout(
            timer as ReturnType<typeof globalThis.setTimeout>,
        ))
    const softWaitMillis = dependencies.softWaitMillis ?? 5_000

    const publish = (next: SyncExitState): void => {
        state = next
        for (const listener of listeners) listener(state)
    }
    const waitForDecision = (): Promise<SyncExitDecision> => {
        const pendingDecision = deferredDecision()
        decision = pendingDecision
        return pendingDecision.promise.finally(() => {
            if (decision === pendingDecision) decision = undefined
        })
    }
    const settleCancelled = (target: SyncExitTarget | null): SyncExitDisposition => {
        publish({ phase: 'cancelled', target })
        return 'cancelled'
    }
    const cancelDrain = async (
        adapter: SyncExitDrainAdapter,
        reason: Exclude<SyncExitDecision, 'wait'>,
    ): Promise<void> => {
        try {
            await adapter.cancel(reason)
        } catch (error) {
            dependencies.reportError?.(error)
        }
    }

    const settleLocal = async (): Promise<
        { kind: 'settled' }
        | { kind: 'exit' }
        | { kind: 'cancelled' }
    > => {
        while (true) {
            try {
                publish({ phase: 'saving' })
                await dependencies.flushLocal()
                await dependencies.checkpointLocal()
                return { kind: 'settled' }
            } catch (error) {
                if (isEditFenceError(error)) {
                    publish({ phase: 'edit-blocked', error })
                    if (await waitForDecision() === 'cancel-exit') {
                        return { kind: 'cancelled' }
                    }
                    continue
                }
                dependencies.reportError?.(error)
                publish({ phase: 'local-failed', error })
                const choice = await waitForDecision()
                if (choice === 'exit-unsynced') return { kind: 'exit' }
                if (choice === 'cancel-exit') return { kind: 'cancelled' }
            }
        }
    }
    const captureDrainTarget = async (): Promise<
        { kind: 'target'; target: SyncExitTarget }
        | { kind: 'exit' }
        | { kind: 'cancelled' }
    > => {
        while (true) {
            try {
                publish({ phase: 'capturing' })
                return {
                    kind: 'target',
                    target: validateTarget(await dependencies.captureTarget()),
                }
            } catch (error) {
                publish({
                    phase: 'remote-blocked',
                    target: null,
                    destination: 'selection',
                    reason: blockedReason(error),
                })
                const choice = await waitForDecision()
                if (choice === 'exit-unsynced') return { kind: 'exit' }
                if (choice === 'cancel-exit') return { kind: 'cancelled' }
            }
        }
    }

    const drainRemote = async (
        target: SyncExitTarget,
        adapter: SyncExitDrainAdapter,
    ): Promise<SyncExitDisposition> => {
        const abort = new AbortController()
        try {
            while (true) {
                publish({
                    phase: 'syncing',
                    target,
                    destination: adapter.id,
                })
                const drain = adapter.drain(target, abort.signal)
                const drainOutcome = drain.then(
                    (result) => ({ kind: 'result' as const, result }),
                    (error) => ({ kind: 'error' as const, error }),
                )
                let outcome:
                    | Awaited<typeof drainOutcome>
                    | undefined
                while (!outcome) {
                    let timer: unknown
                    const delayed = new Promise<'delayed'>((resolve) => {
                        timer = setTimer(() => resolve('delayed'), softWaitMillis)
                    })
                    const first = await Promise.race([
                        drainOutcome,
                        delayed.then(() => ({ kind: 'delayed' as const })),
                    ])
                    if (timer !== undefined) clearTimer(timer)
                    if (first.kind !== 'delayed') {
                        outcome = first
                        break
                    }

                    publish({
                        phase: 'remote-delayed',
                        target,
                        destination: adapter.id,
                    })
                    const next = await Promise.race([
                        drainOutcome,
                        waitForDecision().then((choice) => ({
                            kind: 'decision' as const,
                            choice,
                        })),
                    ])
                    if (next.kind !== 'decision') {
                        if (decision) decision.resolve('wait')
                        outcome = next
                        break
                    }
                    if (next.choice === 'wait') {
                        publish({
                            phase: 'syncing',
                            target,
                            destination: adapter.id,
                        })
                        continue
                    }
                    abort.abort()
                    await cancelDrain(adapter, next.choice)
                    return next.choice === 'exit-unsynced' ? 'exit' : 'cancelled'
                }

                if (outcome.kind === 'error') throw outcome.error
                if (outcome.result.kind === 'complete') {
                    return 'exit'
                } else {
                    publish({
                        phase: 'remote-blocked',
                        target,
                        destination: adapter.id,
                        reason: outcome.result.reason,
                    })
                }

                const choice = await waitForDecision()
                if (choice === 'wait') continue
                abort.abort()
                await cancelDrain(adapter, choice)
                return choice === 'exit-unsynced' ? 'exit' : 'cancelled'
            }
        } catch (error) {
            if (abort.signal.aborted) throw error
            publish({
                phase: 'remote-blocked',
                target,
                destination: adapter.id,
                reason: blockedReason(error),
            })
            const choice = await waitForDecision()
            if (choice === 'wait') return drainRemote(target, adapter)
            abort.abort()
            await cancelDrain(adapter, choice)
            return choice === 'exit-unsynced' ? 'exit' : 'cancelled'
        }
    }

    const run = async (): Promise<SyncExitDisposition> => {
        let fence: SyncExitFence | undefined
        let target: SyncExitTarget | null = null
        try {
            const local = await settleLocal()
            if (local.kind === 'exit') {
                publish({ phase: 'complete', target: null })
                return 'exit'
            }
            if (local.kind === 'cancelled') return settleCancelled(null)
            while (!fence) {
                try {
                    fence = await dependencies.acquireEditFence()
                } catch (error) {
                    publish({ phase: 'edit-blocked', error })
                    const choice = await waitForDecision()
                    if (choice === 'cancel-exit') return settleCancelled(null)
                }
            }
            const capture = await captureDrainTarget()
            if (capture.kind === 'exit') {
                publish({ phase: 'complete', target: null })
                return 'exit'
            }
            if (capture.kind === 'cancelled') return settleCancelled(null)
            target = capture.target
            const adapter = await dependencies.selectedDrain()
            if (adapter) {
                const result = await drainRemote(target, adapter)
                if (result === 'cancelled') return settleCancelled(target)
            }
            publish({ phase: 'complete', target })
            return 'exit'
        } finally {
            fence?.release()
        }
    }

    return {
        requestExit(): Promise<SyncExitDisposition> {
            if (pending) return pending
            pending = run().finally(() => {
                pending = undefined
                decision = undefined
            })
            return pending
        },
        decide(choice: SyncExitDecision): boolean {
            if (!decision) return false
            decision.resolve(choice)
            return true
        },
        snapshot: (): SyncExitState => state,
        subscribe(listener: (state: SyncExitState) => void): () => void {
            listeners.add(listener)
            listener(state)
            return () => listeners.delete(listener)
        },
    }
}

export type SyncExitCoordinator = ReturnType<typeof createSyncExitCoordinator>
