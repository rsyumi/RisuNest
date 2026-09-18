import type { SyncExitDrainResult, SyncExitTarget } from '../../syncExitCoordinator'
import type {
    DecimalString,
    ExternalConnectionError,
    ExternalJobKind,
    ExternalJobSummary,
    ExternalStorageState,
    StartExternalJobRequest,
} from './types'

export type ExternalExecutionSession =
    | { kind: 'foreground'; id: string }
    | { kind: 'exitDrain'; id: string }

export type ExternalJobReason = 'automatic' | 'manual' | 'exitDrain'

export interface ExternalStorageJobBridge {
    startJob(request: StartExternalJobRequest): Promise<ExternalJobSummary>
    getJob(jobId: string): Promise<ExternalJobSummary>
    cancelJob(jobId: string): Promise<ExternalJobSummary>
}

export interface ExternalStorageControllerDependencies {
    wait?(delay: number, signal?: AbortSignal): Promise<void>
    applyReceived?(job: ExternalJobSummary): Promise<void>
}

export interface ExternalControllerRequest {
    connectionId: string
    kind: Extract<ExternalJobKind, 'sync' | 'backup'>
    targetRevision: DecimalString
    reason: ExternalJobReason
    session: ExternalExecutionSession
    signal?: AbortSignal
}

export type ExternalControllerResult =
    | { kind: 'complete'; revision: DecimalString; job: ExternalJobSummary }
    | { kind: 'blocked'; reason: string; error?: ExternalConnectionError; job?: ExternalJobSummary }
    | { kind: 'cancelled' }

export interface ExternalStorageControllerSnapshot {
    state: ExternalStorageState
    activeJobs: ReadonlyMap<string, ExternalJobSummary>
    errors: ReadonlyMap<string, string>
}

interface Goal {
    revision: bigint
    request: ExternalControllerRequest
    resolve(result: ExternalControllerResult): void
    settled: boolean
    removeAbortListener?: () => void
}

interface DestinationRun {
    goals: Goal[]
    active?: ExternalJobSummary
    running?: Promise<void>
    cancelled: boolean
    abort: AbortController
    activeGoal?: Goal
    cancelling?: Promise<void>
}

const terminalStates = new Set<ExternalJobSummary['state']>([
    'succeeded',
    'failed',
    'cancelled',
    'uncertain',
    'conflict',
])

function revision(value: string): bigint {
    if (!/^(0|[1-9]\d*)$/.test(value)) throw new RangeError('Revision must be a decimal string')
    return BigInt(value)
}

function resultRevision(job: ExternalJobSummary): bigint | undefined {
    const value = job.result?.publishedRevision
    return value === undefined ? undefined : revision(value)
}

function blockedReason(job: ExternalJobSummary): string {
    if (job.error?.reason) return job.error.reason
    if (job.error?.code) return job.error.code
    if (job.state === 'conflict') return 'external-storage-conflict'
    if (job.state === 'uncertain') return 'publication-unknown'
    if (job.state === 'cancelled') return 'external-storage-job-cancelled'
    return 'external-storage-job-failed'
}

function wait(delay: number, signal?: AbortSignal): Promise<void> {
    if (signal?.aborted) return Promise.reject(signal.reason)
    return new Promise((resolve, reject) => {
        const abort = () => {
            clearTimeout(timer)
            reject(signal?.reason)
        }
        const timer = setTimeout(() => {
            signal?.removeEventListener('abort', abort)
            resolve()
        }, delay)
        signal?.addEventListener('abort', abort, { once: true })
    })
}

function throwIfAborted(signal?: AbortSignal): void {
    if (signal?.aborted) throw signal.reason
}

export function createExternalStorageController(
    bridge: ExternalStorageJobBridge,
    initialState: ExternalStorageState,
    dependencies: ExternalStorageControllerDependencies = {},
) {
    let state = initialState
    const runs = new Map<string, DestinationRun>()
    const errors = new Map<string, string>()
    const listeners = new Set<(snapshot: ExternalStorageControllerSnapshot) => void>()
    const pause = dependencies.wait ?? wait

    const snapshot = (): ExternalStorageControllerSnapshot => ({
        state,
        activeJobs: new Map(
            [...runs.entries()].flatMap(([id, run]) => run.active ? [[id, run.active]] : []),
        ),
        errors: new Map(errors),
    })
    const publish = (): void => {
        const current = snapshot()
        for (const listener of listeners) listener(current)
    }
    const settleGoal = (goal: Goal, result: ExternalControllerResult): void => {
        if (goal.settled) return
        goal.settled = true
        goal.removeAbortListener?.()
        goal.resolve(result)
    }
    const settleThrough = (
        run: DestinationRun,
        kind: ExternalControllerRequest['kind'],
        achieved: bigint,
        job: ExternalJobSummary,
    ): void => {
        const remaining: Goal[] = []
        for (const goal of run.goals) {
            if (goal.request.kind === kind && goal.revision <= achieved) {
                settleGoal(goal, { kind: 'complete', revision: achieved.toString() as DecimalString, job })
            } else remaining.push(goal)
        }
        run.goals = remaining
    }
    const settleKind = (
        run: DestinationRun,
        kind: ExternalControllerRequest['kind'],
        result: ExternalControllerResult,
    ): void => {
        const matching = run.goals.filter(goal => goal.request.kind === kind)
        run.goals = run.goals.filter(goal => goal.request.kind !== kind)
        for (const goal of matching) settleGoal(goal, result)
    }
    const settleAll = (run: DestinationRun, result: ExternalControllerResult): void => {
        const goals = run.goals.splice(0)
        for (const goal of goals) settleGoal(goal, result)
    }
    const poll = async (
        initial: ExternalJobSummary,
        signal?: AbortSignal,
    ): Promise<ExternalJobSummary> => {
        let job = initial
        throwIfAborted(signal)
        while (!terminalStates.has(job.state)) {
            throwIfAborted(signal)
            if (job.state === 'waiting' && job.phase === 'remote-apply') {
                throwIfAborted(signal)
                if (!dependencies.applyReceived) {
                    throw new Error('External received state has no activation handler')
                }
                await dependencies.applyReceived(job)
                job = await bridge.getJob(job.id)
                throwIfAborted(signal)
                continue
            }
            if (job.state === 'waiting' && job.phase !== 'device-capture') return job
            await pause(job.state === 'waiting' ? 5_000 : 500, signal)
            throwIfAborted(signal)
            job = await bridge.getJob(job.id)
            throwIfAborted(signal)
        }
        throwIfAborted(signal)
        return job
    }
    const runDestination = async (connectionId: string, run: DestinationRun): Promise<void> => {
        while (run.goals.length > 0 && !run.cancelled) {
            if (run.cancelling) {
                await run.cancelling
                run.cancelling = undefined
            }
            if (run.cancelled || run.goals.length === 0) break
            const sessionGoal = run.goals.find(candidate => candidate.request.reason === 'exitDrain')
            const selectedGoal = sessionGoal ?? run.goals.reduce((current, candidate) =>
                candidate.revision > current.revision ? candidate : current)
            const latest = run.goals
                .filter(candidate => candidate.request.kind === selectedGoal.request.kind)
                .reduce((current, candidate) => candidate.revision > current.revision ? candidate : current)
            const request = selectedGoal.request
            run.activeGoal = selectedGoal
            if (run.abort.signal.aborted) run.abort = new AbortController()
            try {
                const started = await bridge.startJob({
                    connectionId,
                    kind: request.kind,
                    targetRevision: latest.revision.toString() as DecimalString,
                    reason: request.reason,
                    session: request.session.kind,
                    sessionId: request.session.id,
                })
                run.active = started
                publish()
                if (
                    (run.cancelled || run.abort.signal.aborted || request.signal?.aborted)
                    && !terminalStates.has(started.state)
                ) {
                    run.cancelling ??= bridge.cancelJob(started.id).then(() => undefined, () => undefined)
                    await run.cancelling
                    run.cancelling = undefined
                    throw run.abort.signal.reason ?? request.signal?.reason
                }
                const completed = await poll(started, run.abort.signal)
                run.active = completed
                if (completed.state !== 'succeeded') {
                    const reason = blockedReason(completed)
                    errors.set(connectionId, reason)
                    settleKind(run, request.kind, {
                        kind: 'blocked', reason, error: completed.error, job: completed,
                    })
                    continue
                }
                const achieved = resultRevision(completed)
                if (achieved === undefined) {
                    if (completed.result?.receivedRevision !== undefined) {
                        revision(completed.result.receivedRevision)
                        publish()
                        continue
                    }
                    const reason = 'external-storage-published-revision-missing'
                    errors.set(connectionId, reason)
                    settleKind(run, request.kind, { kind: 'blocked', reason, job: completed })
                    continue
                }
                errors.delete(connectionId)
                settleThrough(run, request.kind, achieved, completed)
                publish()
            } catch (error) {
                if (run.cancelled) {
                    settleAll(run, { kind: 'cancelled' })
                    break
                } else if (request.signal?.aborted) {
                    continue
                } else {
                    const reason = error instanceof Error && error.message
                        ? error.message
                        : 'external-storage-job-failed'
                    errors.set(connectionId, reason)
                    settleKind(run, request.kind, { kind: 'blocked', reason })
                    continue
                }
            } finally {
                run.active = undefined
                run.activeGoal = undefined
                publish()
            }
        }
    }

    const kick = (connectionId: string, run: DestinationRun): void => {
        if (run.running || run.cancelled || run.goals.length === 0) return
        run.running = runDestination(connectionId, run).finally(() => {
            run.running = undefined
            if (run.goals.length > 0 && !run.cancelled) kick(connectionId, run)
            else if (run.goals.length === 0 && !run.active && runs.get(connectionId) === run) {
                runs.delete(connectionId)
            }
        })
    }
    const request = (requestValue: ExternalControllerRequest): Promise<ExternalControllerResult> => {
        revision(requestValue.targetRevision)
        let run = runs.get(requestValue.connectionId)
        if (!run || run.cancelled) {
            // A cancelled run keeps ownership of its late start and cleanup.
            // New requests wait for it without reviving or sharing its goals.
            const previous = run
            run = {
                goals: [], cancelled: false, abort: new AbortController(),
                cancelling: previous?.running?.then(() => undefined, () => undefined),
            }
            runs.set(requestValue.connectionId, run)
        }
        const promise = new Promise<ExternalControllerResult>((resolve) => {
            const goal: Goal = {
                revision: revision(requestValue.targetRevision),
                request: requestValue,
                resolve,
                settled: false,
            }
            const abort = (): void => {
                if (goal.settled) return
                run!.goals = run!.goals.filter(candidate => candidate !== goal)
                settleGoal(goal, { kind: 'cancelled' })
                if (run!.activeGoal === goal) {
                    run!.abort.abort(requestValue.signal?.reason)
                    if (run!.active && !terminalStates.has(run!.active.state)) {
                        run!.cancelling ??= bridge.cancelJob(run!.active.id)
                            .then(() => undefined, () => undefined)
                    }
                }
            }
            if (requestValue.signal) {
                requestValue.signal.addEventListener('abort', abort, { once: true })
                goal.removeAbortListener = () => requestValue.signal?.removeEventListener('abort', abort)
            }
            run!.goals.push(goal)
            if (requestValue.signal?.aborted) abort()
        })
        kick(requestValue.connectionId, run)
        return promise
    }

    return {
        snapshot,
        replaceState(next: ExternalStorageState): void {
            state = next
            publish()
        },
        request,
        async cancel(connectionId: string): Promise<void> {
            const run = runs.get(connectionId)
            if (!run) return
            run.cancelled = true
            run.abort.abort()
            if (run.active && !terminalStates.has(run.active.state)) {
                try {
                    await bridge.cancelJob(run.active.id)
                } catch {
                    // The native session guard remains authoritative after cancellation races.
                }
            }
            await run.running
            settleAll(run, { kind: 'cancelled' })
        },
        async drainToRevision(
            connectionId: string,
            target: SyncExitTarget,
            sessionId: string,
            signal: AbortSignal,
        ): Promise<SyncExitDrainResult> {
            const result = await request({
                connectionId,
                kind: 'sync',
                targetRevision: String(target.revision) as DecimalString,
                reason: 'exitDrain',
                session: { kind: 'exitDrain', id: sessionId },
                signal,
            })
            if (result.kind === 'complete') return { kind: 'complete' }
            if (result.kind === 'cancelled') return { kind: 'blocked', reason: 'cancelled' }
            return { kind: 'blocked', reason: result.reason }
        },
        subscribe(listener: (snapshot: ExternalStorageControllerSnapshot) => void): () => void {
            listeners.add(listener)
            listener(snapshot())
            return () => listeners.delete(listener)
        },
    }
}

export type ExternalStorageController = ReturnType<typeof createExternalStorageController>
