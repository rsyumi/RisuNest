import type { StreamingDisplayOptimizationMode } from '../storage/database.svelte'

export interface ScheduledSnapshot<T> {
    sequence: number
    value: T
}

export interface StreamingDisplayProcessContext {
    signal: AbortSignal
    canCommit(): boolean
}

type SchedulerStatus = 'open' | 'closing' | 'aborted' | 'failed' | 'closed'

interface SchedulerClock {
    now(): number
    setTimeout(callback: () => void, delay: number): ReturnType<typeof setTimeout>
    clearTimeout(timer: ReturnType<typeof setTimeout>): void
}

interface SchedulerFailure {
    error: unknown
}

export interface LeadingEdgeSchedulerOptions<T> {
    process(
        snapshot: ScheduledSnapshot<T>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    intervalMs?: number
    clock?: SchedulerClock
    onError?(error: unknown): void
}

export interface LeadingEdgeSchedulerInspection {
    status: SchedulerStatus
    activeCount: 0 | 1
    pendingCount: 0 | 1
    timerCount: 0 | 1
    lastCompletedSequence: number
}

export interface LeadingEdgeScheduler<T> {
    submit(value: T): void
    finish(): Promise<void>
    abort(): Promise<void>
    inspect(): LeadingEdgeSchedulerInspection
}

const defaultClock: SchedulerClock = {
    now: () => Date.now(),
    setTimeout: (callback, delay) => setTimeout(callback, delay),
    clearTimeout: (timer) => clearTimeout(timer),
}

export function createLeadingEdgeScheduler<T = string>(
    options: LeadingEdgeSchedulerOptions<T>,
): LeadingEdgeScheduler<T> {
    const intervalMs = options.intervalMs ?? 125
    const clock = options.clock ?? defaultClock
    const abortController = new AbortController()
    let status: SchedulerStatus = 'open'
    let sequence = 0
    let active: Promise<void> | null = null
    let pending: ScheduledSnapshot<T> | null = null
    let timer: ReturnType<typeof setTimeout> | null = null
    let lastStartedAt: number | null = null
    let lastCompletedSequence = 0
    let failure: SchedulerFailure | null = null

    const clearTimer = () => {
        if (timer === null) return
        clock.clearTimeout(timer)
        timer = null
    }

    const canCommit = () => status !== 'aborted' && status !== 'failed'

    const fail = (error: unknown) => {
        if (status === 'aborted' || status === 'failed') return
        failure = { error }
        status = 'failed'
        pending = null
        clearTimer()
        abortController.abort()
        try {
            options.onError?.(error)
        }
        catch {
            // The processor error remains authoritative.
        }
    }

    const schedulePending = () => {
        if (status !== 'open' || active || !pending || timer !== null) return
        const eligibleAt = (lastStartedAt ?? clock.now()) + intervalMs
        const delay = Math.max(0, eligibleAt - clock.now())
        if (delay === 0) {
            startPending()
            return
        }
        timer = clock.setTimeout(() => {
            timer = null
            startPending()
        }, delay)
    }

    const start = (snapshot: ScheduledSnapshot<T>) => {
        clearTimer()
        lastStartedAt = clock.now()
        const context: StreamingDisplayProcessContext = {
            signal: abortController.signal,
            canCommit,
        }
        const running = (async () => options.process(snapshot, context))()
            .then(() => {
                if (canCommit()) lastCompletedSequence = snapshot.sequence
            })
            .catch(fail)
            .finally(() => {
                if (active === running) active = null
                schedulePending()
            })
        active = running
    }

    function startPending() {
        if (status !== 'open' || active || !pending) return
        const snapshot = pending
        pending = null
        start(snapshot)
    }

    const throwFailure = () => {
        if (failure !== null) throw failure.error
    }

    return {
        submit(value) {
            if (status !== 'open') return
            const snapshot = { sequence: ++sequence, value }
            if (!active && lastStartedAt === null) {
                start(snapshot)
                return
            }
            pending = snapshot
            schedulePending()
        },
        async finish() {
            if (status === 'closed') return
            if (status === 'aborted') return
            if (status === 'failed') throwFailure()
            status = 'closing'
            clearTimer()
            if (active) await active
            throwFailure()
            if (pending && pending.sequence > lastCompletedSequence) {
                const snapshot = pending
                pending = null
                start(snapshot)
                if (active) await active
                throwFailure()
            }
            pending = null
            status = 'closed'
        },
        async abort() {
            if (status === 'aborted' || status === 'closed') return
            if (status === 'failed') {
                if (active) await active
                return
            }
            status = 'aborted'
            clearTimer()
            pending = null
            abortController.abort()
            if (active) await active
        },
        inspect() {
            return {
                status,
                activeCount: active ? 1 : 0,
                pendingCount: pending ? 1 : 0,
                timerCount: timer === null ? 0 : 1,
                lastCompletedSequence,
            }
        },
    }
}

interface StreamingDisplayControllerOptions<T> {
    mode: StreamingDisplayOptimizationMode
    processSemantic(
        snapshot: ScheduledSnapshot<T>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    processPreview(
        snapshot: ScheduledSnapshot<T>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    intervalMs?: number
    clock?: SchedulerClock
    onError?(error: unknown): void
}

export interface StreamingDisplayController<T> {
    readonly mode: StreamingDisplayOptimizationMode
    submit(value: T): Promise<void>
    finish(): Promise<void>
    abort(): Promise<void>
    inspect(): LeadingEdgeSchedulerInspection
}

export function createStreamingDisplayController<T = string>(
    options: StreamingDisplayControllerOptions<T>,
): StreamingDisplayController<T> {
    const mode = options.mode
    if (mode === 'off') return createExactController(options)

    let latest: ScheduledSnapshot<T> | null = null
    let submittedSequence = 0
    let completedNormally = false
    let aborted = false
    let finalActive: Promise<void> | null = null
    const finalAbortController = new AbortController()
    const scheduler = createLeadingEdgeScheduler<T>({
        process: mode === 'strong' ? options.processPreview : options.processSemantic,
        intervalMs: options.intervalMs,
        clock: options.clock,
        onError: options.onError,
    })

    return {
        mode,
        async submit(value) {
            latest = { sequence: ++submittedSequence, value }
            scheduler.submit(value)
        },
        async finish() {
            await scheduler.finish()
            if (mode === 'strong' && latest && !completedNormally && !aborted) {
                completedNormally = true
                const context: StreamingDisplayProcessContext = {
                    signal: finalAbortController.signal,
                    canCommit: () => !aborted,
                }
                finalActive = options.processSemantic(latest, context)
                await finalActive
                finalActive = null
            }
        },
        async abort() {
            aborted = true
            finalAbortController.abort()
            await scheduler.abort()
            if (finalActive) await finalActive.catch(() => {})
        },
        inspect: () => scheduler.inspect(),
    }
}

function createExactController<T>(
    options: StreamingDisplayControllerOptions<T>,
): StreamingDisplayController<T> {
    const abortController = new AbortController()
    let status: SchedulerStatus = 'open'
    let sequence = 0
    let lastCompletedSequence = 0
    let active: Promise<void> | null = null
    const queued: Array<{
        snapshot: ScheduledSnapshot<T>
        resolve(): void
        reject(error: unknown): void
    }> = []
    let failure: SchedulerFailure | null = null

    const context: StreamingDisplayProcessContext = {
        signal: abortController.signal,
        canCommit: () => status === 'open' || status === 'closing',
    }

    const startNext = () => {
        if (active || (status !== 'open' && status !== 'closing')) return
        const item = queued.shift()
        if (!item) return
        const running = options.processSemantic(item.snapshot, context)
            .then(() => {
                if (context.canCommit()) lastCompletedSequence = item.snapshot.sequence
                item.resolve()
            })
            .catch((error) => {
                failure = { error }
                status = 'failed'
                abortController.abort()
                item.reject(error)
                for (const waiting of queued.splice(0)) waiting.reject(error)
                try {
                    options.onError?.(error)
                }
                catch {
                    // The processor error remains authoritative.
                }
            })
            .finally(() => {
                if (active === running) active = null
                startNext()
            })
        active = running
    }

    return {
        mode: 'off',
        submit(value) {
            if (status !== 'open') return Promise.resolve()
            const snapshot = { sequence: ++sequence, value }
            const submitted = new Promise<void>((resolve, reject) => {
                queued.push({ snapshot, resolve, reject })
            })
            startNext()
            return submitted
        },
        async finish() {
            if (status === 'closed' || status === 'aborted') return
            if (failure !== null) throw failure.error
            status = 'closing'
            while (active || queued.length > 0) {
                startNext()
                if (active) await active
            }
            if (failure !== null) throw failure.error
            status = 'closed'
        },
        async abort() {
            if (status === 'closed' || status === 'aborted') return
            status = 'aborted'
            abortController.abort()
            for (const waiting of queued.splice(0)) waiting.resolve()
            if (active) await active.catch(() => {})
        },
        inspect() {
            return {
                status,
                activeCount: active ? 1 : 0,
                pendingCount: queued.length > 0 ? 1 : 0,
                timerCount: 0,
                lastCompletedSequence,
            }
        },
    }
}
