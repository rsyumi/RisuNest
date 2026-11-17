import { externalErrorKind } from './connection'
import type { MobileBackgroundTask } from '../../../mobileBackgroundTask'
import type { DecimalString } from './types'
import type {
    ExternalControllerRequest,
    ExternalExecutionSession,
    ExternalStorageController,
} from './controller'

export interface ExternalScheduledDestination {
    connectionId: string
    kind: 'backup'
    quietMillis?: number
    maximumMillis?: number
}

export interface ExternalSchedulerDependencies {
    available(): boolean
    destinations(): ExternalScheduledDestination[]
    session(): ExternalExecutionSession
    maintenance?(): Array<{ connectionId: string; lastAttemptAt?: number }>
    now?(): number
    wallNow?(): number
    setTimer?(callback: () => void, delay: number): unknown
    clearTimer?(timer: unknown): void
}

interface PendingRevision {
    revision: bigint
    firstAt: number
    dueAt: number
}

const defaultPolicy = { quietMillis: 60_000, maximumMillis: 300_000 } as const

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
    let suspended = false
    const refusals = new Map<string, number>()
    const retryNotBefore = new Map<string, number>()
    /** Each stopped destination, with the connection whose recovery lets it run again. */
    const blocked = new Map<string, string>()
    const now = dependencies.now ?? (() => performance.now())
    const wallNow = dependencies.wallNow ?? Date.now
    const setTimer = dependencies.setTimer
        ?? ((callback: () => void, delay: number): unknown => globalThis.setTimeout(callback, delay))
    const clearTimer = dependencies.clearTimer
        ?? ((value: unknown): void => globalThis.clearTimeout(
            value as ReturnType<typeof globalThis.setTimeout>,
        ))

    const lastCleanup = new Map<string, number>()
    const inFlight = new Map<string, number>()
    const started = (id: string) => inFlight.set(id, (inFlight.get(id) ?? 0) + 1)
    const finished = (id: string) => {
        const remaining = (inFlight.get(id) ?? 1) - 1
        if (remaining > 0) inFlight.set(id, remaining)
        else inFlight.delete(id)
    }
    let maintenanceAt = now() + 60_000
    const cleanupInterval = 6 * 60 * 60 * 1000
    const clear = (): void => {
        if (timer !== undefined) clearTimer(timer)
        timer = undefined
    }
    const destinationKey = (destination: ExternalScheduledDestination): string =>
        `${destination.kind}:${destination.connectionId}`
    const scheduleNext = (): void => {
        clear()
        if (stopped || suspended || !dependencies.available()) return
        const next = Math.min(...[...pending.entries()]
            .filter(([key]) => !blocked.has(key) && !inFlight.has(key.slice(key.indexOf(':') + 1)))
            .map(([key, item]) => Math.max(item.dueAt, retryNotBefore.get(key) ?? 0)),
            dependencies.maintenance ? maintenanceAt : Number.POSITIVE_INFINITY)
        if (!Number.isFinite(next)) return
        timer = setTimer(runDue, Math.max(0, next - now()))
    }
    const merge = (destination: ExternalScheduledDestination, target: bigint): void => {
        const currentTime = now()
        const quiet = destination.quietMillis ?? defaultPolicy.quietMillis
        const maximum = destination.maximumMillis ?? defaultPolicy.maximumMillis
        const key = destinationKey(destination)
        const current = pending.get(key)
        const firstAt = current?.firstAt ?? currentTime
        pending.set(key, {
            revision: current && current.revision > target ? current.revision : target,
            firstAt,
            dueAt: Math.min(currentTime + quiet, firstAt + maximum),
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
        const deadline = currentTime + Math.max(5_000, Number(parsed) - wallNow())
        retryNotBefore.set(key, Math.max(retryNotBefore.get(key) ?? 0, deadline))
        pending.set(key, {
            revision: current && current.revision > target ? current.revision : target,
            firstAt: current?.firstAt ?? currentTime,
            dueAt: deadline,
        })
        return true
    }
    function runDue(): void {
        clear()
        if (stopped || suspended || !dependencies.available()) return
        const currentTime = now()
        const destinations = new Map(
            dependencies.destinations().map(destination => [destinationKey(destination), destination]),
        )
        for (const [key, item] of pending) {
            if (blocked.has(key) || item.dueAt > currentTime
                || (retryNotBefore.get(key) ?? 0) > currentTime) continue
            const destination = destinations.get(key)
            if (!destination) {
                pending.delete(key)
                continue
            }
            if (inFlight.has(destination.connectionId)) continue
            pending.delete(key)
            const request: ExternalControllerRequest = {
                connectionId: destination.connectionId,
                kind: destination.kind,
                targetRevision: item.revision.toString() as DecimalString,
                reason: 'automatic',
                session: dependencies.session(),
            }
            started(destination.connectionId)
            void controller.request(request).then((result) => {
                if (stopped) return
                if (result.kind === 'complete') {
                    const latest = pending.get(key)
                    if (latest && latest.revision <= parseRevision(result.revision)) pending.delete(key)
                }
                if (result.kind !== 'blocked') {
                    refusals.delete(key)
                    return
                }
                const stopRetry = () => { blocked.set(key, result.error?.connectionId || destination.connectionId) }
                if (result.reason === 'publication-unknown' || result.error?.reason === 'publication-unknown') { stopRetry(); return }
                const causeKind = result.error?.code ?? externalErrorKind('cause' in result ? result.cause : undefined) ?? result.reason
                if (result.error?.retryable === false) {
                    if (!['clockSkew', 'preconditionFailed'].includes(causeKind)) stopRetry()
                    return
                }
                if (!result.error && causeKind === 'clockSkew') return
                if (!result.error && ['endpointRejected', 'unauthorized', 'reauthRequired',
                    'repositoryKeyUnavailable', 'deviceVaultUnavailable', 'storageFull',
                    'localStorageFull', 'localPermissionDenied', 'unsupported', 'corrupt', 'notFound',
                    'repositoryMismatch'].includes(causeKind)) { stopRetry(); return }
                if (!result.error && result.reason === 'preconditionFailed') {
                    const count = (refusals.get(key) ?? 0) + 1
                    refusals.set(key, count)
                    if (count >= 3) return
                }
                if (
                    result.error?.retryable
                    && result.error.retryAtMs !== undefined
                    && ['retry', 'wait'].includes(result.error.action)
                    && scheduleRetry(destination, item.revision, result.error.retryAtMs)
                ) {
                    scheduleNext()
                    return
                }
                if (result.error?.action === 'reauthenticate'
                    || result.error?.action === 'unlock-key'
                    || result.error?.action === 'free-space') { stopRetry(); return }
                if (result.error?.action === 'wait') return
                merge(destination, item.revision)
                scheduleNext()
            }).finally(() => {
                finished(destination.connectionId)
                runDue()
            })
        }
        if (dependencies.maintenance && currentTime >= maintenanceAt) {
            maintenanceAt = currentTime + 60_000
            for (const candidate of dependencies.maintenance?.() ?? []) {
                if (currentTime - (lastCleanup.get(candidate.connectionId) ?? -Infinity) < cleanupInterval
                    || wallNow() - (candidate.lastAttemptAt ?? -Infinity) < cleanupInterval
                    || inFlight.has(candidate.connectionId)
                    || [...pending.keys()].some(key => key.endsWith(`:${candidate.connectionId}`))) continue
                lastCleanup.set(candidate.connectionId, currentTime)
                started(candidate.connectionId)
                void controller.request({ connectionId: candidate.connectionId, kind: 'cleanup',
                    targetRevision: '0', reason: 'automatic', session: dependencies.session(),
                }).finally(() => {
                    finished(candidate.connectionId)
                    runDue()
                })
            }
        }
        scheduleNext()
    }

    return {
        durableRevision(value: DecimalString): void {
            const target = parseRevision(value)
            refusals.clear()
            for (const destination of dependencies.destinations()) merge(destination, target)
            scheduleNext()
        },
        requestNow(
            connectionId: string,
            kind: 'backup' | 'cleanup',
            value: DecimalString,
            backgroundTask?: MobileBackgroundTask,
        ) {
            const target = parseRevision(value)
            const key = `${kind}:${connectionId}`
            blocked.delete(key)
            const newer = pending.get(key)
            if (!newer || newer.revision <= target) pending.delete(key)
            scheduleNext()
            if (kind === 'cleanup') lastCleanup.set(connectionId, now())
            started(connectionId)
            return controller.request({
                connectionId,
                kind,
                targetRevision: target.toString() as DecimalString,
                reason: 'manual',
                session: dependencies.session(),
                ...(backgroundTask ? { backgroundTask } : {}),
            }).then(result => {
                if (result.kind === 'complete') {
                    const latest = pending.get(key)
                    if (latest && latest.revision <= parseRevision(result.revision)) pending.delete(key)
                }
                return result
            }).finally(() => {
                finished(connectionId)
                runDue()
            })
        },
        resume(): void {
            suspended = false
            scheduleNext()
        },
        recovered(connectionId: string): void {
            for (const [key, waitsOn] of blocked) if (waitsOn === connectionId) blocked.delete(key)
            blocked.delete(`backup:${connectionId}`)
            refusals.delete(`backup:${connectionId}`)
            scheduleNext()
        },
        async suspend(
            cancel: (destination: ExternalScheduledDestination) => boolean = () => true,
        ): Promise<void> {
            suspended = true
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
    }
}

export type ExternalStorageScheduler = ReturnType<typeof createExternalStorageScheduler>
