import type { DecimalString } from './types'
import type {
    ExternalControllerRequest,
    ExternalExecutionSession,
    ExternalStorageController,
} from './controller'

export type ExternalRevisionCause = 'edit' | 'generation-complete'

export interface ExternalScheduledDestination {
    connectionId: string
    kind: 'sync' | 'backup'
    quietMillis?: number
    maximumMillis?: number
}

export interface ExternalSchedulerDependencies {
    available(): boolean
    destinations(): ExternalScheduledDestination[]
    session(): ExternalExecutionSession
    now?(): number
    setTimer?(callback: () => void, delay: number): unknown
    clearTimer?(timer: unknown): void
}

interface PendingRevision {
    revision: bigint
    firstAt: number
    dueAt: number
}

const defaultPolicy = {
    sync: { quietMillis: 15_000, maximumMillis: 60_000 },
    backup: { quietMillis: 60_000, maximumMillis: 300_000 },
} as const

function parseRevision(value: DecimalString): bigint {
    if (!/^(0|[1-9]\d*)$/.test(value)) throw new RangeError('Revision must be a decimal string')
    return BigInt(value)
}

/** Coalesces durable revisions only. Native owns job persistence and quota waits. */
export function createExternalStorageScheduler(
    controller: ExternalStorageController,
    dependencies: ExternalSchedulerDependencies,
) {
    const pending = new Map<string, PendingRevision>()
    let timer: unknown
    let stopped = false
    const now = dependencies.now ?? Date.now
    const setTimer = dependencies.setTimer
        ?? ((callback: () => void, delay: number): unknown => globalThis.setTimeout(callback, delay))
    const clearTimer = dependencies.clearTimer
        ?? ((value: unknown): void => globalThis.clearTimeout(
            value as ReturnType<typeof globalThis.setTimeout>,
        ))

    const clear = (): void => {
        if (timer !== undefined) clearTimer(timer)
        timer = undefined
    }
    const destinationKey = (destination: ExternalScheduledDestination): string =>
        `${destination.kind}:${destination.connectionId}`
    const scheduleNext = (): void => {
        clear()
        if (stopped || !dependencies.available() || pending.size === 0) return
        const next = Math.min(...[...pending.values()].map(item => item.dueAt))
        timer = setTimer(runDue, Math.max(0, next - now()))
    }
    const merge = (
        destination: ExternalScheduledDestination,
        target: bigint,
        cause: ExternalRevisionCause,
    ): void => {
        const currentTime = now()
        const policy = defaultPolicy[destination.kind]
        const quiet = destination.quietMillis ?? policy.quietMillis
        const maximum = destination.maximumMillis ?? policy.maximumMillis
        const key = destinationKey(destination)
        const current = pending.get(key)
        const firstAt = current?.firstAt ?? currentTime
        const causeDue = currentTime + (
            cause === 'generation-complete' && destination.kind === 'sync' ? 5_000 : quiet
        )
        const quietDue = cause === 'generation-complete' && destination.kind === 'sync'
            ? Math.min(current?.dueAt ?? Number.POSITIVE_INFINITY, causeDue)
            : causeDue
        pending.set(key, {
            revision: current && current.revision > target ? current.revision : target,
            firstAt,
            dueAt: Math.min(quietDue, firstAt + maximum),
        })
    }
    const scheduleRetry = (
        destination: ExternalScheduledDestination,
        target: bigint,
        retryAtMs: DecimalString,
    ): boolean => {
        const parsed = parseRevision(retryAtMs)
        if (parsed > BigInt(Number.MAX_SAFE_INTEGER)) return false
        const currentTime = now()
        const key = destinationKey(destination)
        const current = pending.get(key)
        pending.set(key, {
            revision: current && current.revision > target ? current.revision : target,
            firstAt: current?.firstAt ?? currentTime,
            dueAt: Math.max(currentTime + 5_000, Number(parsed)),
        })
        return true
    }
    function runDue(): void {
        clear()
        if (stopped || !dependencies.available()) return
        const currentTime = now()
        const destinations = new Map(
            dependencies.destinations().map(destination => [destinationKey(destination), destination]),
        )
        for (const [key, item] of pending) {
            if (item.dueAt > currentTime) continue
            const destination = destinations.get(key)
            if (!destination) {
                pending.delete(key)
                continue
            }
            pending.delete(key)
            const request: ExternalControllerRequest = {
                connectionId: destination.connectionId,
                kind: destination.kind,
                targetRevision: item.revision.toString() as DecimalString,
                reason: 'automatic',
                session: dependencies.session(),
            }
            void controller.request(request).then((result) => {
                if (result.kind !== 'blocked') return
                if (
                    result.error?.retryable
                    && result.error.retryAtMs !== undefined
                    && ['retry', 'wait'].includes(result.error.action)
                    && scheduleRetry(destination, item.revision, result.error.retryAtMs)
                ) {
                    scheduleNext()
                    return
                }
                if (result.error?.action === 'wait' || result.error?.action === 'reauthenticate'
                    || result.error?.action === 'unlock-key'
                    || result.error?.action === 'resolve-conflict'
                    || result.error?.action === 'free-space'
                    || result.reason === 'publication-unknown') return
                merge(destination, item.revision, 'edit')
                scheduleNext()
            })
        }
        scheduleNext()
    }

    return {
        durableRevision(value: DecimalString, cause: ExternalRevisionCause = 'edit'): void {
            const target = parseRevision(value)
            for (const destination of dependencies.destinations()) merge(destination, target, cause)
            scheduleNext()
        },
        requestNow(
            connectionId: string,
            kind: 'sync' | 'backup',
            value: DecimalString,
        ) {
            const target = parseRevision(value)
            const key = `${kind}:${connectionId}`
            const newer = pending.get(key)
            if (!newer || newer.revision <= target) pending.delete(key)
            scheduleNext()
            return controller.request({
                connectionId,
                kind,
                targetRevision: target.toString() as DecimalString,
                reason: 'manual',
                session: dependencies.session(),
            })
        },
        resume(): void {
            scheduleNext()
        },
        async suspend(
            cancel: (destination: ExternalScheduledDestination) => boolean = () => true,
        ): Promise<void> {
            clear()
            await Promise.all(
                dependencies.destinations().filter(cancel).map(destination =>
                    controller.cancel(destination.connectionId)),
            )
        },
        stop(): void {
            stopped = true
            clear()
        },
        pendingRevision(connectionId: string, kind: 'sync' | 'backup'): DecimalString | undefined {
            return pending.get(`${kind}:${connectionId}`)?.revision.toString() as DecimalString | undefined
        },
    }
}

export type ExternalStorageScheduler = ReturnType<typeof createExternalStorageScheduler>
