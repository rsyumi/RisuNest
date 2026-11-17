export interface ServerSyncSchedulerDependencies {
    push(): Promise<void>
    pull(completeAvailable?: boolean): Promise<void>
    connect(operationId: string): Promise<void>
    /** Stops the notification channel and cancels running work. */
    disconnect(): Promise<void>
    /** Stops only the notification channel, so running work continues while hidden. */
    stopNotifications?(): Promise<void>
    retryClock?(): Promise<void>
    /** Checks the library format level again, failing while it still stops sync. */
    checkFormat?(): Promise<void>
    publishHidden?(): Promise<void>
    automaticSyncAvailable?(): boolean
    /** `retried` is true when the failed request began after an outage delay that no success has ended since. */
    failed(error: unknown, retried: boolean): void
    recovered?(): void
    retrying?(): void
    /** Called after a send, receive or clock check starts or settles. */
    activity?(): void
}

export const integrityCodes = ['clock-skew', 'incoming-clock-skew', 'accepted-clock-correction-required', 'writer-collision', 'equal-stamp-integrity', 'server-epoch-changed', 'unauthorized', 'invalid-device-token']
// A new clock sample can clear these, so they are rechecked instead of waiting for a manual retry.
const clockCodes = ['clock-skew', 'incoming-clock-skew']
// The library format level can change on another device or server, so these are rechecked too.
export const formatCodes = ['library-format-switch-required', 'app-update-required', 'sync-server-update-required']
export function serverSyncErrorCode(error: unknown): string {
    if (typeof error !== 'object' || !error) return ''
    if ('message' in error && typeof error.message === 'string' && integrityCodes.includes(error.message)) return error.message
    return 'code' in error ? String(error.code) : ''
}
const isCancellation = (error: unknown) => serverSyncErrorCode(error) === 'cancelled' || (typeof error === 'object' && !!error && 'name' in error && error.name === 'AbortError')
export function isServerSyncUnavailable(error: unknown): boolean {
    if (typeof error === 'object' && error && 'retryable' in error && error.retryable === false) return false
    const code = serverSyncErrorCode(error)
    if (['server-unreachable', 'server-timeout', 'server-updating', 'request-timeout', 'incomplete-response', 'directory-unreachable', 'directory-response-error', 'directory-incomplete-response', 'sync-retry-budget-exhausted'].includes(code)) return true
    if (typeof error !== 'object' || error === null || !('status' in error) || typeof error.status !== 'number') return false
    // Only an activation's server check marks its failures, so a rate limit or a coded 5xx there came from the server.
    if ('startupLocalReady' in error && (error.status === 429 || error.status >= 500 && error.status < 600)) return true
    // Tunnel/gateway HTML responses have no Sync error body.
    return code === 'server-response-error' && error.status >= 500 && error.status < 600
}
type Lane = 'push' | 'pull' | 'connect'

export function createServerSyncScheduler(dependencies: ServerSyncSchedulerDependencies) {
    const available = () => dependencies.automaticSyncAvailable?.() ?? true
    const recheckFor = (code: string) => clockCodes.includes(code) ? dependencies.retryClock : formatCodes.includes(code) ? dependencies.checkFormat : undefined
    let foreground = false
    let connected = false
    let pushTimer: ReturnType<typeof setTimeout> | undefined
    let pullTimer: ReturnType<typeof setTimeout> | undefined
    let retryTimer: ReturnType<typeof setTimeout> | undefined
    let sendRetryTimer: ReturnType<typeof setTimeout> | undefined
    let sending: Promise<void> | undefined
    let receiving: Promise<void> | undefined
    let completingReceive: Promise<void> | undefined
    let pushAgain = false
    let pullAgain = false
    let reconnectAttempt = 0
    let notificationOperationId: string | undefined
    let sendRetryAttempt = 0
    let blocked = false
    let blockedCode = ''
    let rechecking: Promise<boolean> | undefined
    let disconnecting: Promise<void> | undefined
    let connecting: Promise<void> | undefined
    let outageTimer: ReturnType<typeof setTimeout> | undefined
    let outageUntil = 0
    let outageAttempt = 0
    let outageEpoch = 0
    // A monotonic clock, so setting the device clock back does not extend the delay.
    const waiting = () => performance.now() < outageUntil
    const resetOutage = () => { clearTimeout(outageTimer); outageUntil = 0; outageAttempt = 0 }
    const scheduleOutage = () => {
        clearTimeout(outageTimer)
        if (!foreground || blocked || !waiting()) return
        outageTimer = setTimeout(() => {
            connect(); void pull().catch(() => {}); void push().catch(() => {})
            dependencies.retrying?.()
        }, outageUntil - performance.now())
    }
    const failedLanes = new Set<Lane>()
    let disconnectingFully = false
    const disconnect = (stop = dependencies.disconnect) => {
        const fully = stop === dependencies.disconnect
        // A cancel requested while hiding stops only the notification channel still runs after it.
        if (disconnecting && (disconnectingFully || !fully)) return disconnecting
        const previous = disconnecting
        disconnectingFully = fully
        const current: Promise<void> = (async () => {
            if (previous) await previous.catch(() => {})
            await stop()
            const started = connecting
            // A start that was already running may finish after the stop, so stop again once it settles.
            if (started) { await started; await stop() }
        })().finally(() => { if (disconnecting === current) { disconnecting = undefined; disconnectingFully = false } })
        disconnecting = current
        return current
    }
    const code = serverSyncErrorCode
    const succeeded = (lane: Lane, epoch: number) => {
        if (epoch !== outageEpoch) return
        if (failedLanes.delete(lane) && failedLanes.size === 0 && !blocked) {
            resetOutage()
            dependencies.recovered?.()
        }
    }
    const blocks = (error: unknown) => (typeof error === 'object' && !!error && 'retryable' in error && error.retryable === false) || integrityCodes.includes(code(error))
    const fail = (error: unknown, lane?: Lane, epoch?: number) => {
        if (isCancellation(error)) return
        if (blocked && !clockCodes.includes(blockedCode) && !formatCodes.includes(blockedCode)) return
        // A check that cannot reach the server keeps the format stop it already reports.
        if (blocked && formatCodes.includes(blockedCode) && !blocks(error)) return
        if (lane) failedLanes.add(lane)
        else if (isServerSyncUnavailable(error)) failedLanes.add('pull')
        // Each outage delay since the last success advanced the epoch, so a request begun in one of them was a retry.
        const retried = epoch !== undefined && epoch > outageEpoch - outageAttempt
        // Concurrent lane failures share one delay. Edits and foreground changes do not bypass it.
        if (isServerSyncUnavailable(error) && !waiting()) {
            outageEpoch++
            outageUntil = performance.now() + Math.min(300_000, 5_000 * 2 ** Math.min(outageAttempt++, 6))
            clear()
            scheduleOutage()
        }
        if (blocks(error)) {
            blocked = true
            blockedCode = code(error)
            clear()
            void disconnect().catch(() => {})
        }
        dependencies.failed(error, retried)
    }
    const clear = () => { clearTimeout(pushTimer); clearTimeout(pullTimer); clearTimeout(retryTimer); clearTimeout(sendRetryTimer); clearTimeout(outageTimer) }
    const schedulePull = () => {
        clearTimeout(pullTimer)
        if (waiting()) { scheduleOutage(); return }
        if (foreground && !blocked) pullTimer = setTimeout(() => { void pull().catch(() => {}) }, connected ? 60_000 : 5_000)
    }
    const push = (): Promise<void> => {
        clearTimeout(pushTimer)
        clearTimeout(sendRetryTimer)
        if (waiting()) { scheduleOutage(); return Promise.resolve() }
        if (!foreground || blocked || !available()) return Promise.resolve()
        if (sending) { pushAgain = true; return sending }
        const epoch = outageEpoch
        sending = dependencies.push().then(() => { sendRetryAttempt = 0; succeeded('push', epoch) }).catch(error => {
            fail(error, 'push', epoch)
            if (foreground && !blocked && !waiting()) sendRetryTimer = setTimeout(() => { void push().catch(() => {}) }, Math.min(30_000, 1000 * 2 ** sendRetryAttempt++))
            throw error
        }).finally(() => {
            sending = undefined
            if (pushAgain && foreground) { pushAgain = false; void push().catch(() => {}) }
            dependencies.activity?.()
        })
        dependencies.activity?.()
        return sending
    }
    const pull = (completeAvailable = false): Promise<void> => {
        clearTimeout(pullTimer)
        if (!completeAvailable && waiting()) { scheduleOutage(); return Promise.resolve() }
        if (((!foreground || !available()) && !completeAvailable) || blocked) return Promise.resolve()
        if (!completeAvailable && completingReceive) { pullAgain = true; return completingReceive }
        if (receiving) { pullAgain = true; return receiving }
        const epoch = outageEpoch
        receiving = dependencies.pull(completeAvailable).then(() => succeeded('pull', epoch), error => { fail(error, 'pull', epoch); throw error }).finally(() => {
            receiving = undefined
            if (pullAgain && foreground && !completingReceive) { pullAgain = false; void pull().catch(() => {}) }
            else schedulePull()
            dependencies.activity?.()
        })
        dependencies.activity?.()
        return receiving
    }
    const connect = () => {
        if (waiting()) { scheduleOutage(); return }
        if (!foreground || blocked || !available()) return
        // A stop from the previous hidden session must finish before the new channel starts.
        if (disconnecting) { void disconnecting.then(connect, connect); return }
        if (connecting) return
        const epoch = outageEpoch
        const attempt: Promise<void> = dependencies.connect(notificationOperationId ??= crypto.randomUUID()).then(() => succeeded('connect', epoch), error => { fail(error, 'connect', epoch); socket(false) })
            .finally(() => { if (connecting === attempt) connecting = undefined })
        connecting = attempt
    }
    const socket = (value: boolean) => {
        connected = value
        clearTimeout(retryTimer)
        if (value) { reconnectAttempt = 0; notificationOperationId = undefined }
        else if (foreground && !blocked && !waiting()) {
            void pull().catch(() => {})
            retryTimer = setTimeout(connect, Math.min(30_000, 1000 * 2 ** reconnectAttempt++))
        }
        schedulePull()
    }
    // Pending work goes out before the app may be suspended. A foreground push started meanwhile waits for it.
    const publishHidden = async () => {
        await sending?.catch(() => {})
        if (foreground || blocked || sending || !available() || waiting()) return
        pushAgain = false
        const epoch = outageEpoch
        sending = dependencies.publishHidden!().then(() => { sendRetryAttempt = 0; succeeded('push', epoch) }, error => { fail(error, 'push', epoch) }).finally(() => {
            sending = undefined
            if (pushAgain && foreground) { pushAgain = false; void push().catch(() => {}) }
            dependencies.activity?.()
        })
        dependencies.activity?.()
        await sending
    }
    const recheck = (): Promise<boolean> => {
        if (!foreground || !blocked || !recheckFor(blockedCode) || !available()) return Promise.resolve(false)
        if (rechecking) return rechecking
        const epoch = outageEpoch
        rechecking = (async () => {
            await Promise.allSettled([sending, receiving, disconnecting])
            const check = recheckFor(blockedCode)
            if (!foreground || !check || !available()) return false
            await check()
            if (!foreground || recheckFor(blockedCode) !== check) return false
            blocked = false; blockedCode = ''; sendRetryAttempt = 0
            connect(); void pull().catch(() => {}); void push().catch(() => {})
            return true
        })().catch(error => { fail(error, undefined, epoch); return false }).finally(() => { rechecking = undefined; dependencies.activity?.() })
        dependencies.activity?.()
        return rechecking
    }
    return {
        async foreground(value: boolean, publishPending = false) {
            if (foreground === value) { if (value) await recheck(); return }
            foreground = value
            notificationOperationId = undefined
            clear()
            if (value) { reconnectAttempt = 0; if (!await recheck()) { connect(); void pull().catch(() => {}); void push().catch(() => {}) } }
            else {
                connected = false
                if (publishPending && !blocked && dependencies.publishHidden) await publishHidden()
                // Hiding stops checks and the notification channel; sends and receives already running finish.
                if (!foreground) await disconnect(dependencies.stopNotifications ?? dependencies.disconnect)
            }
        },
        localChange(immediate = false) {
            clearTimeout(pushTimer)
            if (!foreground) return
            // Edits made during a library format stop check the level again.
            if (blocked) { if (formatCodes.includes(blockedCode)) pushTimer = setTimeout(() => { void recheck() }, 2000); return }
            if (immediate) void push().catch(() => {})
            else pushTimer = setTimeout(() => { void push().catch(() => {}) }, 2000)
        },
        remoteHint: () => { void pull().catch(() => {}) },
        async conversationOpened() { if (!await recheck()) void pull().catch(() => {}) },
        socket,
        flush: push,
        receiveAvailableChanges() {
            if (completingReceive) return completingReceive
            completingReceive = (async () => {
                if (blocked) throw new Error('Sync is stopped')
                await receiving
                if (blocked) throw new Error('Sync is stopped')
                await pull(true)
            })().finally(() => {
                completingReceive = undefined
                if (pullAgain && foreground) { pullAgain = false; void pull().catch(() => {}) }
                dependencies.activity?.()
            })
            return completingReceive
        },
        retry() { resetOutage(); blocked = false; blockedCode = ''; sendRetryAttempt = 0; connect(); void pull().catch(() => {}); return push() },
        reset() { resetOutage(); blocked = false; blockedCode = ''; sendRetryAttempt = 0; reconnectAttempt = 0; notificationOperationId = undefined; failedLanes.clear() },
        async waitForIdle() {
            while (sending || receiving || completingReceive || rechecking) {
                await Promise.allSettled([sending, receiving, completingReceive, rechecking])
            }
        },
        async resumeAutomaticSync() {
            if (!foreground || !available()) return
            if (!await recheck()) {
                if (!connected) connect()
                void pull().catch(() => {})
                void push().catch(() => {})
            }
        },
        reportFailure: fail,
        /** Passed to reportFailure with a request that starts now, so its failure counts as a retry when it began during an outage. */
        epoch: () => outageEpoch,
        isBlocked: () => blocked,
        isWaiting: waiting,
        /** The failure that stopped sync until a retry, or '' while it is not stopped. */
        blockedCode: () => blocked ? blockedCode : '',
        isRunning: () => !!sending || !!receiving || !!completingReceive || !!rechecking,
        async fence() { foreground = false; pushAgain = false; pullAgain = false; clear(); await disconnect(); await Promise.allSettled([sending, receiving, completingReceive, rechecking]) },
        dispose() { foreground = false; clear(); void disconnect().catch(fail) },
    }
}
